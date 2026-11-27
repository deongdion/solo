//! Entry point that assembles an emulator: CPU backend, linker, kernel and host functions.
//!
//! Guest code reaches the host in two ways, both supervisor calls:
//! - `svc #0` is a Linux syscall, handled by the kernel.
//! - `svc #HOST_BASE+n` sits in a stub on the host page; it implements a function the dynamic linker
//!   provides on a device (libdl) or a reporting stub for an unresolved symbol.
//!
//! A call into the guest returns through the trampoline page (`backend::TRAMPOLINE`).

mod host;
pub mod report;

use std::path::PathBuf;

use backend::{Arch, Cpu, Reg, Stop, TRAMPOLINE};
use kernel::{Config as KernelConfig, Kernel, Outcome};
use linker::{Linker, Resolver};
use machine::Perms;

pub use host::HostFunction;
pub use report::{Crash, StopKind, Summary};

/// Guest memory layout, the same as unidbg's so addresses can be compared one to one.
pub mod layout {
    pub const MMAP_BASE: u64 = 0x1200_0000;
    pub const MMAP_LIMIT_32: u64 = 0xe000_0000;
    pub const MMAP_LIMIT_64: u64 = 0x7_0000_0000;
    pub const HEAP_BASE: u64 = 0x0804_8000;
    pub const STACK_BASE: u64 = 0xe500_0000;
    pub const STACK_SIZE: u64 = 0x10_0000;
    /// Stubs for host functions, one per 8 bytes.
    pub const HOST_STUBS: u64 = 0xfffe_0000;
    pub const HOST_STUBS_SIZE: u64 = 0x1_0000;
    /// Strings and structures the host hands to the guest.
    pub const HOST_DATA: u64 = 0xfff0_0000;
    pub const HOST_DATA_SIZE: u64 = 0xe_0000;
}

/// First supervisor call immediate used for host functions.
pub const HOST_BASE: u32 = 0x100;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Backend(#[from] backend::Error),
    #[error(transparent)]
    Memory(#[from] machine::Error),
    #[error(transparent)]
    Linker(#[from] linker::Error),
    #[error(transparent)]
    Kernel(#[from] kernel::Error),
    #[error("{0}")]
    Crash(Box<Crash>),
    #[error("guest exited with status {0}")]
    Exited(i32),
    #[error("instruction budget exhausted")]
    Budget,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendKind {
    Dynarmic,
    #[cfg(feature = "unicorn")]
    Unicorn,
}

pub struct Options {
    pub arch: Arch,
    pub backend: BackendKind,
    /// Directories searched for libraries by file name, in order.
    pub library_dirs: Vec<PathBuf>,
    /// Host directory served as the guest `/`.
    pub rootfs: Option<PathBuf>,
    pub kernel: KernelConfig,
    /// Instructions per call before the run stops with `Error::Budget`.
    pub budget: u64,
    pub env: Vec<String>,
}

impl Options {
    pub fn new(arch: Arch) -> Options {
        Options {
            arch,
            backend: BackendKind::Dynarmic,
            library_dirs: Vec::new(),
            rootfs: None,
            kernel: KernelConfig::default(),
            budget: u64::MAX,
            env: vec![
                "ANDROID_DATA=/data".into(),
                "ANDROID_ROOT=/system".into(),
                "PATH=/sbin:/vendor/bin:/system/sbin:/system/bin:/system/xbin".into(),
            ],
        }
    }
}

struct DirResolver {
    dirs: Vec<PathBuf>,
}

impl Resolver for DirResolver {
    fn find(&self, name: &str) -> Option<(String, Vec<u8>)> {
        let path = std::path::Path::new(name);
        if path.is_absolute() && path.is_file() {
            return std::fs::read(path).ok().map(|d| (name.to_string(), d));
        }
        let file = name.rsplit('/').next().unwrap_or(name);
        // unidbg stores libstdc++.so as libstdcpp.so; accept both spellings.
        let alternate = file.replace('+', "p");
        for dir in &self.dirs {
            for candidate in [dir.join(file), dir.join(&alternate)] {
                if candidate.is_file() {
                    return std::fs::read(&candidate).ok().map(|d| (candidate.to_string_lossy().into_owned(), d));
                }
            }
        }
        None
    }
}

pub struct Emulator {
    pub cpu: Box<dyn Cpu>,
    pub linker: Linker,
    pub kernel: Kernel,
    resolver: DirResolver,
    host: host::Host,
    budget: u64,
    depth: u32,
}

impl Emulator {
    pub fn new(options: Options) -> Result<Emulator, Error> {
        let arch = options.arch;
        let cpu: Box<dyn Cpu> = match options.backend {
            BackendKind::Dynarmic => Box::new(backend::Dynarmic::new(arch)?),
            #[cfg(feature = "unicorn")]
            BackendKind::Unicorn => Box::new(backend::Unicorn::new(arch)?),
        };
        let limit = if arch == Arch::Arm64 { layout::MMAP_LIMIT_64 } else { layout::MMAP_LIMIT_32 };
        let kernel = Kernel::new(
            options.kernel,
            kernel::vfs::Vfs::new(options.rootfs),
            layout::HEAP_BASE,
            layout::MMAP_BASE,
            limit,
        );
        let mut emulator = Emulator {
            cpu,
            linker: Linker::new(layout::MMAP_BASE, limit),
            kernel,
            resolver: DirResolver { dirs: options.library_dirs },
            host: host::Host::default(),
            budget: options.budget,
            depth: 0,
        };
        emulator.setup(&options.env)?;
        Ok(emulator)
    }

    pub fn arch(&self) -> Arch {
        self.cpu.arch()
    }

    pub fn word(&self) -> u64 {
        if self.arch() == Arch::Arm64 { 8 } else { 4 }
    }

    fn write_word(&mut self, at: u64, value: u64) -> Result<(), Error> {
        if self.arch() == Arch::Arm64 {
            self.cpu.memory_mut().write_u64(at, value)?;
        } else {
            self.cpu.memory_mut().write_u32(at, value as u32)?;
        }
        Ok(())
    }

    /// Stack, host pages and the initial thread: TLS slots and the argument block bionic's
    /// `__libc_preinit` reads from TLS_SLOT_BIONIC_PREINIT.
    fn setup(&mut self, env: &[String]) -> Result<(), Error> {
        use layout::*;
        self.cpu.map(STACK_BASE - STACK_SIZE, STACK_SIZE, Perms::RW, "[stack]")?;
        self.cpu.map(HOST_STUBS, HOST_STUBS_SIZE, Perms::RX, "[host]")?;
        self.cpu.map(HOST_DATA, HOST_DATA_SIZE, Perms::RW, "[host data]")?;
        self.host.data_next = HOST_DATA;

        let word = self.word();
        let mut sp = STACK_BASE;
        let mut push = |emu: &mut Emulator, bytes: &[u8]| -> Result<u64, Error> {
            sp = (sp - bytes.len() as u64) & !(word - 1);
            emu.cpu.memory_mut().write(sp, bytes)?;
            Ok(sp)
        };
        // pthread_internal_t: next, prev, tid.
        let thread = push(self, &[0u8; 0x400])?;
        let pid = self.kernel.config.pid;
        self.cpu.memory_mut().write_u32(thread + 2 * word, pid)?;
        let mut random = [0u8; 16];
        self.kernel.random_bytes(&mut random);
        let at_random = push(self, &random)?;
        let mut name = self.kernel.config.process_name.clone().into_bytes();
        name.push(0);
        let program = push(self, &name)?;
        let mut env_ptrs = Vec::new();
        for e in env {
            let mut bytes = e.clone().into_bytes();
            bytes.push(0);
            env_ptrs.push(push(self, &bytes)?);
        }
        let argv = push(self, &vec![0u8; 2 * word as usize])?;
        self.write_word(argv, program)?;
        let environ = push(self, &vec![0u8; (env_ptrs.len() + 1) * word as usize])?;
        for (i, p) in env_ptrs.iter().enumerate() {
            self.write_word(environ + i as u64 * word, *p)?;
        }
        // auxv: AT_PAGESZ, AT_RANDOM, AT_NULL.
        let auxv = push(self, &vec![0u8; 6 * word as usize])?;
        for (i, v) in [6u64, 4096, 25, at_random, 0, 0].iter().enumerate() {
            self.write_word(auxv + i as u64 * word, *v)?;
        }
        // KernelArgumentBlock: argc, argv, envp, auxv.
        let args = push(self, &vec![0u8; 8 * word as usize])?;
        self.write_word(args, 1)?;
        self.write_word(args + word, argv)?;
        self.write_word(args + 2 * word, environ)?;
        self.write_word(args + 3 * word, auxv)?;
        // TLS slots: 1 thread, 2 errno, 3 preinit argument block.
        let tls = push(self, &vec![0u8; 0x80 * word as usize])?;
        self.write_word(tls, tls)?;
        self.write_word(tls + word, thread)?;
        self.write_word(tls + 3 * word, args)?;
        self.host.environ = environ;
        self.host.argv = argv;
        self.cpu.set_reg(Reg::Tls, tls);
        self.cpu.set_reg(Reg::Sp, sp & !15);
        Ok(())
    }

    /// Loads a library and its dependencies, then runs their init functions (dependencies first).
    pub fn load(&mut self, name: &str) -> Result<usize, Error> {
        let mut hooks = host::LinkHooks { host: &mut self.host };
        let loaded = self.linker.load(self.cpu.as_mut(), &self.resolver, &mut hooks, name)?;
        self.run_init(&loaded.init_order)?;
        Ok(loaded.module)
    }

    fn run_init(&mut self, order: &[usize]) -> Result<(), Error> {
        for &index in order {
            let functions = self.linker.modules[index].init.clone();
            for function in functions {
                let (argv, environ) = (self.host.argv, self.host.environ);
                self.call(function, &[1, argv, environ])?;
            }
        }
        Ok(())
    }

    /// Address of an exported symbol in a loaded module.
    pub fn symbol(&self, module: usize, name: &str) -> Option<u64> {
        self.linker.modules[module].symbol(name)
    }

    /// Calls a guest function and returns its first result register.
    pub fn call(&mut self, function: u64, args: &[u64]) -> Result<u64, Error> {
        let saved_pc = self.cpu.pc();
        let saved_lr = self.cpu.reg(Reg::Lr);
        let saved_sp = self.cpu.reg(Reg::Sp);
        let registers = if self.arch() == Arch::Arm64 { 8 } else { 4 };
        let word = self.word();
        let mut sp = saved_sp;
        if args.len() > registers {
            let extra = &args[registers..];
            sp = (sp - extra.len() as u64 * word) & !15;
            for (i, v) in extra.iter().enumerate() {
                self.write_word(sp + i as u64 * word, *v)?;
            }
        }
        for (i, v) in args.iter().take(registers).enumerate() {
            self.cpu.set_reg(Reg::General(i as u8), *v);
        }
        self.cpu.set_reg(Reg::Sp, sp);
        self.cpu.set_reg(Reg::Lr, TRAMPOLINE);
        self.cpu.set_pc(function);
        self.depth += 1;
        let result = self.run_loop();
        self.depth -= 1;
        let value = self.cpu.reg(Reg::General(0));
        self.cpu.set_reg(Reg::Sp, saved_sp);
        self.cpu.set_reg(Reg::Lr, saved_lr);
        self.cpu.set_pc(saved_pc);
        result?;
        Ok(if self.arch() == Arch::Arm64 { value } else { value & 0xffff_ffff })
    }

    fn run_loop(&mut self) -> Result<(), Error> {
        // One budget per call, spread over the resumes between syscalls and host calls.
        let mut remaining = self.budget;
        loop {
            let stop = self.cpu.resume(remaining)?;
            remaining = remaining.saturating_sub(self.cpu.executed());
            if remaining == 0 && stop != Stop::Returned {
                return Err(Error::Budget);
            }
            match stop {
                Stop::Returned => return Ok(()),
                Stop::Svc(0) => match self.kernel.syscall(self.cpu.as_mut())? {
                    Outcome::Continue => {}
                    Outcome::Exit(code) => return Err(Error::Exited(code)),
                },
                Stop::Svc(imm) if imm >= HOST_BASE => {
                    let index = (imm - HOST_BASE) as usize;
                    let function = self.host.functions.get(index).cloned();
                    match function {
                        Some(f) => {
                            let value = self.host_call(&f)?;
                            self.cpu.set_reg(Reg::General(0), value);
                        }
                        None => return Err(self.crash(StopKind::UnknownSvc(imm))),
                    }
                }
                Stop::Svc(imm) => return Err(self.crash(StopKind::UnknownSvc(imm))),
                Stop::Budget => return Err(Error::Budget),
                Stop::Halted => return Err(self.crash(StopKind::Halted)),
                Stop::Fault { address, write } => return Err(self.crash(StopKind::Fault { address, write })),
                Stop::Exception { pc, kind } => return Err(self.crash(StopKind::Exception { pc, kind })),
            }
        }
    }

    pub(crate) fn crash(&mut self, kind: StopKind) -> Error {
        Error::Crash(Box::new(report::capture(self, kind)))
    }

    /// Everything the run could not satisfy, aggregated (PLAN.md F7).
    pub fn summary(&self) -> Summary {
        report::summary(self)
    }

}
