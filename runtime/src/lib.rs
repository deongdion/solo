//! Entry point that assembles an emulator: CPU backend, linker, kernel and host functions.
//!
//! Guest code reaches the host in two ways, both supervisor calls:
//! - `svc #0` is a Linux syscall, handled by the kernel.
//! - `svc #HOST_BASE+n` sits in a stub on the host page; it implements a function the dynamic linker
//!   provides on a device (libdl) or a reporting stub for an unresolved symbol.
//!
//! A call into the guest returns through the trampoline page (`backend::TRAMPOLINE`).

pub mod hooks;
mod host;
mod jni;
pub mod pool;
pub mod report;

use std::path::PathBuf;

use backend::{Arch, Cpu, Reg, Stop, TRAMPOLINE};
use kernel::{Config as KernelConfig, Kernel, Outcome, signals};
use linker::{Linker, Resolver};
use machine::Perms;

pub use hooks::{Call, Flow, Hook, HookId};
pub use host::HostFunction;
pub use javavm::{self, JType, Jni, ObjectId, UnhandledPolicy, Value, Vm};
pub use jni::OnLoadEvent;
pub use report::{Crash, StopKind, Summary};
pub use pool::{Pool, PoolConfig, PoolError, PoolStats, Ticket};

// Emulators move to pool worker threads.
const _: fn() = || {
    fn send<T: Send>() {}
    send::<Emulator>();
};

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
    /// Import hook stubs and the hook return stub, mapped on first use.
    pub const HOOK_STUBS: u64 = 0xffef_0000;
    pub const HOOK_STUBS_SIZE: u64 = 0x1_0000;
}

/// First supervisor call immediate used for host functions.
pub const HOST_BASE: u32 = 0x100;

/// Instructions a guest thread runs before the next runnable thread gets the CPU.
const THREAD_SLICE: u64 = 200_000;

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
    #[error("hook: {0}")]
    Hook(String),
    #[error("{0}")]
    Crash(Box<Crash>),
    #[error("guest exited with status {0}")]
    Exited(i32),
    #[error("instruction budget exhausted")]
    Budget,
    /// Stopped by `Interrupt::interrupt` (a deadline or a cancellation).
    #[error("interrupted")]
    Interrupted,
    /// Every guest thread waits on a futex and none can time out.
    #[error("deadlock: every guest thread waits")]
    Deadlock,
    /// A signal whose default action ends the process (no handler installed).
    #[error("killed by signal {0}")]
    Killed(u32),
    #[error("{error} (called from {caller})")]
    Jni { error: javavm::JniError, caller: String },
    /// A Java exception was pending when a call from the host returned.
    #[error("Java exception: {description}")]
    JavaException { exception: ObjectId, description: String },
    #[error("no native implementation of {0} (not registered, no Java_ symbol exported)")]
    NoNative(String),
    #[error("{0} is not exported by the module")]
    NoSymbol(String),
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
    /// Stop memory faults at the faulting instruction so the crash report shows its PC. Off by
    /// default: it costs 7-27% on dynarmic (PLAN.md); without it the PC is the translated block's.
    /// unicorn faults are always precise.
    pub precise_faults: bool,
    /// Built-in java.lang/java.util models (the `java` crate), asked after the embedder's handler.
    pub java_models: bool,
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
            precise_faults: false,
            java_models: true,
        }
    }
}

#[derive(Clone)]
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
    /// Java side of JNI: objects, classes, references and handlers.
    pub vm: Vm,
    /// Heap size after the last collection; the next one runs at twice this.
    collect_at: usize,
    pub(crate) onload_events: Vec<jni::OnLoadEvent>,
    budget: u64,
    depth: u32,
    pub(crate) hooks: hooks::Hooks,
}

impl Emulator {
    pub fn new(options: Options) -> Result<Emulator, Error> {
        let arch = options.arch;
        let cpu: Box<dyn Cpu> = match options.backend {
            BackendKind::Dynarmic => Box::new(backend::Dynarmic::with_options(arch, 0, options.precise_faults)?),
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
            vm: Vm::new(layout::MMAP_BASE, limit),
            collect_at: 4096,
            onload_events: Vec::new(),
            budget: options.budget,
            depth: 0,
            hooks: hooks::Hooks::default(),
        };
        emulator.setup(&options.env)?;
        emulator.install_jni()?;
        if options.java_models {
            java::install(&mut emulator.vm);
        }
        Ok(emulator)
    }

    pub fn arch(&self) -> Arch {
        self.cpu.arch()
    }

    /// An independent copy of this emulator: guest memory and registers, loaded modules, kernel
    /// state (files, brk, mmap cursor, random state), and the Java side with each JNI handler's
    /// `Jni::fork`. Fork after `JNI_OnLoad` to start pool workers without loading again.
    /// Translated code is not copied, so the copy's first calls translate again.
    pub fn fork(&self) -> Result<Emulator, Error> {
        if self.depth != 0 {
            return Err(Error::Backend(backend::Error::Backend("fork during a call".into())));
        }
        let vm = self.vm.fork().map_err(|error| Error::Jni { error, caller: String::new() })?;
        Ok(Emulator {
            cpu: self.cpu.fork()?,
            linker: self.linker.clone(),
            kernel: self.kernel.clone(),
            resolver: self.resolver.clone(),
            host: self.host.clone(),
            vm,
            collect_at: self.collect_at,
            onload_events: self.onload_events.clone(),
            budget: self.budget,
            depth: 0,
            hooks: self.hooks.clone(),
        })
    }

    /// Makes this emulator a copy of `template`, as `fork` would, but keeps this one's CPU
    /// engine. Used to reset pool workers.
    pub fn restore(&mut self, template: &Emulator) -> Result<(), Error> {
        if self.depth != 0 || template.depth != 0 {
            return Err(Error::Backend(backend::Error::Backend("restore during a call".into())));
        }
        if self.cpu.arch() != template.cpu.arch() {
            return Err(Error::Backend(backend::Error::Backend("restore across architectures".into())));
        }
        self.vm = template.vm.fork().map_err(|error| Error::Jni { error, caller: String::new() })?;
        self.cpu.restore(&*template.cpu)?;
        self.linker = template.linker.clone();
        self.kernel = template.kernel.clone();
        self.resolver = template.resolver.clone();
        self.host = template.host.clone();
        self.collect_at = template.collect_at;
        self.onload_events = template.onload_events.clone();
        self.budget = template.budget;
        self.hooks = template.hooks.clone();
        Ok(())
    }

    /// A handle that stops the running call from another thread with `Error::Interrupted`.
    /// An interrupt that arrives between calls stops the next call unless `clear_interrupt` runs
    /// first.
    pub fn interrupt(&self) -> backend::Interrupt {
        self.cpu.interrupt()
    }

    pub fn clear_interrupt(&self) {
        self.cpu.interrupt().take();
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
        let slots: Vec<javavm::args::Slot> = args.iter().map(|&a| javavm::args::Slot::Word(a)).collect();
        let returned = self.call_slots(function, &slots)?;
        Ok(if self.arch() == Arch::Arm64 { returned.r0 } else { returned.r0 & 0xffff_ffff })
    }

    /// Calls a guest function with typed arguments laid out per the platform ABI (FP registers
    /// on AArch64, aligned register pairs for 64-bit values on 32-bit) and returns its result
    /// registers. The caller's PC, LR and SP are restored, so this nests inside host calls.
    pub fn call_slots(&mut self, function: u64, slots: &[javavm::args::Slot]) -> Result<javavm::args::Returned, Error> {
        // Calls from the host run on the main thread; nested calls (from a JNI callback) run on
        // the thread that made the callback.
        if self.depth == 0 {
            self.kernel.begin_call(self.cpu.as_mut());
        }
        let saved_pc = self.cpu.pc();
        let saved_lr = self.cpu.reg(Reg::Lr);
        let saved_sp = self.cpu.reg(Reg::Sp);
        let layout = javavm::args::layout(self.arch(), slots);
        let mut sp = saved_sp;
        if !layout.stack.is_empty() {
            sp = (sp - layout.stack.len() as u64) & !15;
            self.cpu.memory_mut().write(sp, &layout.stack)?;
        }
        for (i, v) in layout.general.iter().enumerate() {
            self.cpu.set_reg(Reg::General(i as u8), *v);
        }
        for (i, v) in layout.fp.iter().enumerate() {
            self.cpu.set_reg(Reg::Fp(i as u8), *v);
        }
        self.cpu.set_reg(Reg::Sp, sp);
        self.cpu.set_reg(Reg::Lr, TRAMPOLINE);
        self.cpu.set_pc(function);
        self.depth += 1;
        self.kernel.threads.enter();
        let result = self.run_loop();
        self.kernel.threads.leave();
        self.depth -= 1;
        let returned = javavm::args::Returned::read(self.cpu.as_ref());
        self.cpu.set_reg(Reg::Sp, saved_sp);
        self.cpu.set_reg(Reg::Lr, saved_lr);
        self.cpu.set_pc(saved_pc);
        result?;
        Ok(returned)
    }

    fn run_loop(&mut self) -> Result<(), Error> {
        // One budget per call, spread over the resumes between syscalls and host calls.
        let mut remaining = self.budget;
        let interrupt = self.cpu.interrupt();
        // A stop that came out of a hook's single step, handled before resuming.
        let mut next: Option<Stop> = None;
        loop {
            if interrupt.take() {
                return Err(Error::Interrupted);
            }
            if let Some(kernel::Outcome::Killed(signo)) = self.kernel.deliver_pending(self.cpu.as_mut())? {
                return Err(Error::Killed(signo));
            }
            // With several guest threads, run in slices and rotate between them.
            let threaded = self.kernel.threads.count() > 1;
            let slice = if threaded { remaining.min(THREAD_SLICE) } else { remaining };
            let stop = match next.take() {
                Some(stop) => stop,
                None => {
                    let stop = self.cpu.resume(slice)?;
                    remaining = remaining.saturating_sub(self.cpu.executed());
                    if remaining == 0 && stop != Stop::Returned {
                        return Err(Error::Budget);
                    }
                    stop
                }
            };
            match stop {
                Stop::Returned if self.kernel.threads.owner().is_none_or(|o| o == self.kernel.threads.current) => return Ok(()),
                // Another thread finished an outer call: it waits at its trampoline until this
                // call returns.
                Stop::Returned => {
                    if self.kernel.threads.park(self.cpu.as_mut(), TRAMPOLINE) == kernel::threads::Switch::Deadlock {
                        return Err(Error::Deadlock);
                    }
                }
                Stop::Svc(0) => match self.kernel.syscall(self.cpu.as_mut())? {
                    Outcome::Continue => {}
                    Outcome::Exit(code) => return Err(Error::Exited(code)),
                    Outcome::Deadlock => return Err(Error::Deadlock),
                    Outcome::Killed(signo) => return Err(Error::Killed(signo)),
                },
                Stop::Svc(hooks::HOOK_SVC) => match self.hook_hit(self.cpu.pc())? {
                    Some(stop) => next = stop,
                    // Linux ignores the immediate: any other `svc #0xfe` is a syscall.
                    None => match self.kernel.syscall(self.cpu.as_mut())? {
                        Outcome::Continue => {}
                        Outcome::Exit(code) => return Err(Error::Exited(code)),
                        Outcome::Deadlock => return Err(Error::Deadlock),
                        Outcome::Killed(signo) => return Err(Error::Killed(signo)),
                    },
                },
                Stop::Svc(imm) if imm >= HOST_BASE => {
                    let index = (imm - HOST_BASE) as usize;
                    let function = self.host.functions.get(index).cloned();
                    match function {
                        Some(HostFunction::Jni(slot)) => self.jni_slot(slot as usize)?,
                        Some(HostFunction::JavaVm(slot)) => self.java_vm_slot(slot as usize)?,
                        Some(f) => {
                            let value = self.host_call(&f)?;
                            self.cpu.set_reg(Reg::General(0), value);
                        }
                        None => return Err(self.crash(StopKind::UnknownSvc(imm))),
                    }
                }
                Stop::Svc(imm) => return Err(self.crash(StopKind::UnknownSvc(imm))),
                Stop::Budget if remaining > 0 => self.kernel.preempt(self.cpu.as_mut()),
                Stop::Budget => return Err(Error::Budget),
                // An interrupt halts the JIT; the flag says whether this one was it. A halt left
                // over from an interrupt already handled just resumes.
                Stop::Halted if interrupt.take() => return Err(Error::Interrupted),
                Stop::Halted => {}
                // A fault goes to the guest's handler for it, if it installed one.
                Stop::Fault { address, write } => {
                    let code = if self.cpu.memory().mapping_at(address).is_some() { signals::SEGV_ACCERR } else { signals::SEGV_MAPERR };
                    if !self.kernel.fault_signal(self.cpu.as_mut(), signals::SIGSEGV, code, address)? {
                        return Err(self.crash(StopKind::Fault { address, write }));
                    }
                }
                Stop::Exception { pc, kind } => {
                    let (signo, code) =
                        if kind == backend::EXCEPTION_BREAKPOINT { (signals::SIGTRAP, signals::TRAP_BRKPT) } else { (signals::SIGILL, signals::ILL_ILLOPC) };
                    self.cpu.set_pc(pc);
                    if !self.kernel.fault_signal(self.cpu.as_mut(), signo, code, pc)? {
                        return Err(self.crash(StopKind::Exception { pc, kind }));
                    }
                }
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
