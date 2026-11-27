//! Linux syscalls, virtual file system confined to the rootfs, memory mapping and process state.
//!
//! Syscalls the kernel does not implement return `-ENOSYS` and are counted with the first caller, so
//! the end-of-run summary lists what to add next. Every error is an errno for the guest; the kernel
//! never turns a guest mistake into a host exception (see PLAN.md F12).

pub mod numbers;
pub mod signals;
pub mod threads;
pub mod vfs;

use std::collections::BTreeMap;
use std::time::{SystemTime, UNIX_EPOCH};

use backend::{Arch, Cpu, Reg};
use machine::{Perms, page_ceil};
use numbers::Sys;
use vfs::{Node, Vfs};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Memory(#[from] machine::Error),
    #[error(transparent)]
    Backend(#[from] backend::Error),
}

pub mod errno {
    pub const EPERM: i64 = 1;
    pub const ENOENT: i64 = 2;
    pub const EBADF: i64 = 9;
    pub const EAGAIN: i64 = 11;
    pub const ENOMEM: i64 = 12;
    pub const EACCES: i64 = 13;
    pub const EFAULT: i64 = 14;
    pub const EEXIST: i64 = 17;
    pub const ENOTDIR: i64 = 20;
    pub const EISDIR: i64 = 21;
    pub const EINVAL: i64 = 22;
    pub const ESRCH: i64 = 3;
    pub const ENOTTY: i64 = 25;
    pub const ESPIPE: i64 = 29;
    pub const ERANGE: i64 = 34;
    pub const ENOSYS: i64 = 38;
    pub const ETIMEDOUT: i64 = 110;
    pub const EAFNOSUPPORT: i64 = 97;
}
use errno::*;

const AT_FDCWD: i64 = -100;
const AT_EMPTY_PATH: u64 = 0x1000;
const O_ACCMODE: u64 = 3;
const O_CREAT: u64 = 0x40;
const O_EXCL: u64 = 0x80;
const O_TRUNC: u64 = 0x200;
const O_APPEND: u64 = 0x400;
const O_DIRECTORY: u64 = 0x4000;
const MAP_FIXED: u64 = 0x10;
const MAP_ANONYMOUS: u64 = 0x20;
const PR_SET_VMA: u64 = 0x5356_4d41;
const S_IFREG: u32 = 0o100000;
const S_IFDIR: u32 = 0o040000;
const S_IFCHR: u32 = 0o020000;

/// What the runtime does after a syscall.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// The value was written to the result register; continue.
    Continue,
    /// The process exited.
    Exit(i32),
    /// Every guest thread waits and none can time out.
    Deadlock,
    /// A signal whose default action ends the process.
    Killed(u32),
}

#[derive(Debug, Clone)]
enum Target {
    Stdin,
    Stdout,
    Stderr,
    File { path: String, data: Vec<u8>, pos: u64, writable: bool, append: bool, dirty: bool },
    Directory { path: String },
    Null,
    Random,
}

/// Process-wide settings that unidbg exposes as emulator options.
#[derive(Debug, Clone)]
pub struct Config {
    pub pid: u32,
    pub uid: u32,
    pub process_name: String,
    pub executable: String,
    /// Print each file operation as one line (PLAN.md F5).
    pub trace_files: bool,
    /// Print each syscall with its arguments and result.
    pub trace_syscalls: bool,
    /// Echo guest stdout and stderr to the host.
    pub echo_output: bool,
    /// Seed for getrandom and AT_RANDOM; None uses host time.
    pub random_seed: Option<u64>,
    /// Source of every clock (PLAN.md F19); `ClockMode::Fixed` with `random_seed` makes runs
    /// repeatable.
    pub clock: device::ClockMode,
    /// Monotonic time (time since boot) when the process starts.
    pub uptime_ns: u64,
}

impl Default for Config {
    fn default() -> Config {
        Config {
            pid: 2048,
            uid: 10000,
            process_name: "app".into(),
            executable: "/system/bin/app_process".into(),
            trace_files: false,
            trace_syscalls: false,
            echo_output: true,
            random_seed: None,
            clock: device::ClockMode::Host,
            uptime_ns: device::clock::DEFAULT_UPTIME_NS,
        }
    }
}

/// Unhandled syscall statistics for the end-of-run summary.
#[derive(Debug, Clone, Default)]
pub struct Unhandled {
    pub count: u64,
    pub first_pc: u64,
}

#[derive(Clone)]
pub struct Kernel {
    pub config: Config,
    pub vfs: Vfs,
    fds: BTreeMap<i64, Target>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub file_trace: Vec<String>,
    pub unhandled: BTreeMap<String, Unhandled>,
    /// Paths the guest tried to open or stat that did not exist, with a count (PLAN.md F16): a
    /// library probing for files it expects on a real device. Not every miss is a bug, so this is
    /// a report, not an error.
    pub missing_files: BTreeMap<String, u64>,
    /// Properties the guest read that the device profile had no value for, with a count.
    pub missing_properties: BTreeMap<String, u64>,
    brk_base: u64,
    brk: u64,
    mmap_low: u64,
    mmap_high: u64,
    random: u64,
    /// Shared with the Java side (`Vm::clock`) by the runtime.
    pub clock: device::Clock,
    pub signals: signals::Signals,
    cwd: String,
    pub threads: threads::Threads,
    /// Set by a syscall that switched threads: the result register belongs to another thread.
    switched: bool,
    deadlock: bool,
    killed: Option<u32>,
}

impl Kernel {
    pub fn new(config: Config, vfs: Vfs, brk_base: u64, mmap_low: u64, mmap_high: u64) -> Kernel {
        let random = config.random_seed.unwrap_or_else(|| {
            SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos() as u64).unwrap_or(1)
        }) | 1;
        let pid = config.pid;
        let clock = device::Clock::new(config.clock, config.uptime_ns);
        let mut fds = BTreeMap::new();
        fds.insert(0, Target::Stdin);
        fds.insert(1, Target::Stdout);
        fds.insert(2, Target::Stderr);
        Kernel {
            config,
            vfs,
            fds,
            stdout: Vec::new(),
            stderr: Vec::new(),
            file_trace: Vec::new(),
            unhandled: BTreeMap::new(),
            missing_files: BTreeMap::new(),
            missing_properties: BTreeMap::new(),
            brk_base,
            brk: brk_base,
            mmap_low,
            mmap_high,
            random,
            clock,
            signals: signals::Signals::new(backend::TRAMPOLINE + backend::SIGRETURN_STUB_OFFSET),
            cwd: "/".into(),
            threads: threads::Threads::new(pid),
            switched: false,
            deadlock: false,
            killed: None,
        }
    }

    pub fn random_bytes(&mut self, out: &mut [u8]) {
        for b in out {
            // xorshift64*: deterministic when seeded.
            self.random ^= self.random >> 12;
            self.random ^= self.random << 25;
            self.random ^= self.random >> 27;
            *b = (self.random.wrapping_mul(0x2545_f491_4f6c_dd1d) >> 56) as u8;
        }
    }

    fn trace(&mut self, line: String) {
        if self.config.trace_files {
            eprintln!("[file] {line}");
            self.file_trace.push(line);
        }
    }

    /// Handles the syscall the guest just made with `svc #0`.
    pub fn syscall(&mut self, cpu: &mut dyn Cpu) -> Result<Outcome, Error> {
        let arch = cpu.arch();
        let number = match arch {
            Arch::Arm64 => cpu.reg(Reg::General(8)),
            Arch::Arm32 => cpu.reg(Reg::General(7)),
        };
        let args: [u64; 7] = std::array::from_fn(|i| cpu.reg(Reg::General(i as u8)));
        let sys = match arch {
            Arch::Arm64 => numbers::arm64(number),
            Arch::Arm32 => numbers::arm32(number),
        };
        self.switched = false;
        self.deadlock = false;
        self.killed = None;
        let result = match sys {
            Some(sys) => self.dispatch(cpu, sys, &args)?,
            None => Err(ENOSYS),
        };
        if self.deadlock {
            return Ok(Outcome::Deadlock);
        }
        if let Some(signo) = self.killed {
            return Ok(Outcome::Killed(signo));
        }
        if self.switched {
            if self.config.trace_syscalls {
                eprintln!("[syscall] {sys:?} switched to thread {}", self.threads.current);
            }
            return Ok(Outcome::Continue);
        }
        let result = match result {
            Ok(Some(value)) => value,
            Ok(None) => return Ok(Outcome::Exit(args[0] as i32)),
            Err(e) => {
                if e == ENOSYS {
                    let name = sys.map(|s| format!("{s:?}")).unwrap_or_else(|| format!("#{number}"));
                    let pc = cpu.pc();
                    let entry = self.unhandled.entry(name).or_insert(Unhandled { count: 0, first_pc: pc });
                    entry.count += 1;
                }
                -e
            }
        };
        if self.config.trace_syscalls {
            let name = sys.map(|s| format!("{s:?}")).unwrap_or_else(|| format!("#{number}"));
            eprintln!(
                "[syscall] {name}({:#x}, {:#x}, {:#x}, {:#x}) = {result:#x} at {:#x}",
                args[0], args[1], args[2], args[3], cpu.pc()
            );
        }
        let value = match arch {
            Arch::Arm64 => result as u64,
            Arch::Arm32 => result as u64 & 0xffff_ffff,
        };
        cpu.set_reg(Reg::General(0), value);
        Ok(Outcome::Continue)
    }

    fn after_switch(&mut self, switch: threads::Switch) {
        match switch {
            threads::Switch::Stay => self.switched = true,
            threads::Switch::Switched => self.switched = true,
            threads::Switch::Deadlock => self.deadlock = true,
        }
    }

    /// A handler for a memory fault signal resumes from the faulting instruction's state (a
    /// probe's siglongjmp, a guard page handler), so the backend must stop exactly there from now
    /// on. It stays so after the handler is removed.
    fn fault_handler_installed(cpu: &mut dyn Cpu, signo: u32, action: &signals::Action) -> Result<(), Error> {
        if matches!(signo, signals::SIGSEGV | signals::SIGBUS) && action.handler > 1 {
            cpu.require_precise_faults()?;
        }
        Ok(())
    }

    /// Delivers the lowest pending, unblocked signal of the running thread, if any.
    pub fn deliver_pending(&mut self, cpu: &mut dyn Cpu) -> Result<Option<Outcome>, Error> {
        let tid = self.threads.current;
        let Some(signo) = self.signals.take_deliverable(tid) else { return Ok(None) };
        Ok(match self.signals.deliver(cpu, tid, signo, signals::SI_TKILL, 0, self.config.pid)? {
            signals::Delivery::Terminate => Some(Outcome::Killed(signo)),
            _ => None,
        })
    }

    /// Sends a fault signal to the running thread if it has a handler for it; false otherwise
    /// (the runtime then reports a crash).
    pub fn fault_signal(&mut self, cpu: &mut dyn Cpu, signo: u32, code: i32, address: u64) -> Result<bool, Error> {
        let tid = self.threads.current;
        if self.config.trace_syscalls {
            eprintln!("[signal] {signo} code {code} address {address:#x} at {:#x} sp {:#x}", cpu.pc(), cpu.reg(Reg::Sp));
        }
        if !self.signals.catches(tid, signo) {
            return Ok(false);
        }
        Ok(self.signals.deliver(cpu, tid, signo, code, address, 0)? == signals::Delivery::Handler)
    }

    /// Gives the CPU to another runnable thread after an instruction slice.
    pub fn preempt(&mut self, cpu: &mut dyn Cpu) {
        if self.threads.count() > 1 {
            self.threads.yield_now(cpu);
        }
    }

    /// Calls from the host run on the main thread.
    pub fn begin_call(&mut self, cpu: &mut dyn Cpu) {
        let main = self.threads.main;
        self.threads.run(cpu, main);
    }

    fn is64(cpu: &dyn Cpu) -> bool {
        cpu.arch() == Arch::Arm64
    }

    fn signed(cpu: &dyn Cpu, value: u64) -> i64 {
        if Self::is64(cpu) { value as i64 } else { value as u32 as i32 as i64 }
    }

    fn path(&self, cpu: &dyn Cpu, dirfd: i64, address: u64) -> Result<Option<String>, i64> {
        if address == 0 {
            return Ok(None);
        }
        let raw = cpu.memory().read_cstr(address, 4096).map_err(|_| EFAULT)?;
        let text = String::from_utf8_lossy(&raw).into_owned();
        if text.is_empty() {
            return Ok(Some(String::new()));
        }
        let base = if text.starts_with('/') || dirfd == AT_FDCWD {
            self.cwd.clone()
        } else {
            match self.fds.get(&dirfd) {
                Some(Target::Directory { path }) | Some(Target::File { path, .. }) => path.clone(),
                _ => return Err(EBADF),
            }
        };
        Ok(Some(vfs::join(&base, &text)))
    }

    /// Resolves the path argument of an `*at` syscall: a null or empty path with AT_EMPTY_PATH names
    /// the descriptor itself.
    fn at_target(&self, cpu: &dyn Cpu, dirfd: u64, address: u64, flags: u64) -> Result<Result<String, i64>, i64> {
        let dirfd = Self::signed(cpu, dirfd);
        match self.path(cpu, dirfd, address)? {
            Some(p) if !p.is_empty() => Ok(Ok(p)),
            _ if flags & AT_EMPTY_PATH != 0 => Ok(Err(dirfd)),
            _ => Err(ENOENT),
        }
    }

    fn dispatch(&mut self, cpu: &mut dyn Cpu, sys: Sys, a: &[u64; 7]) -> Result<Result<Option<i64>, i64>, Error> {
        let ok = |v: i64| Ok(Ok(Some(v)));
        let is64 = Self::is64(cpu);
        let sa: [i64; 7] = a.map(|v| if is64 { v as i64 } else { v as u32 as i32 as i64 });
        match sys {
            Sys::Getpid => ok(self.config.pid as i64),
            Sys::Gettid => ok(self.threads.current as i64),
            Sys::Getppid => ok(1),
            Sys::Getuid | Sys::Geteuid | Sys::Getgid | Sys::Getegid => ok(self.config.uid as i64),
            Sys::SetTidAddress => {
                self.threads.set_clear_tid(a[0]);
                ok(self.threads.current as i64)
            }
            Sys::Clone => {
                let flags = a[0];
                if flags & threads::CLONE_THREAD == 0 || flags & threads::CLONE_VM == 0 {
                    // A new process: not emulated.
                    return Ok(Err(EAGAIN));
                }
                let tid = self.threads.spawn(cpu, flags, a[1], a[2], a[3], a[4])?;
                self.signals.inherit(self.threads.current, tid);
                ok(tid as i64)
            }
            Sys::SchedYield | Sys::Nanosleep if self.threads.count() > 1 => {
                cpu.set_reg(Reg::General(0), 0);
                let switch = self.threads.yield_now(cpu);
                self.after_switch(switch);
                ok(0)
            }
            Sys::Exit if self.threads.current != self.threads.main => {
                self.signals.forget_thread(self.threads.current);
                let switch = self.threads.exit(cpu)?;
                self.after_switch(switch);
                ok(0)
            }
            Sys::ArmCacheflush => {
                // cacheflush(start, end, flags): the guest wrote code (a packer decrypting itself,
                // a trampoline). dynarmic keeps a translation cache that only `protect`/`unmap`
                // invalidate, so without this a rewritten-then-rerun address runs its stale
                // translation; unicorn re-translates on its own. Invalidate the flushed range.
                let (start, end) = (a[0], a[1]);
                if end > start {
                    cpu.invalidate(start, end - start);
                }
                ok(0)
            }
            Sys::SetRobustList | Sys::SchedYield | Sys::Setpriority | Sys::Msync
            | Sys::Fsync | Sys::Setrlimit => ok(0),
            Sys::Getpriority => ok(20),
            Sys::Umask => ok(0o022),
            Sys::Madvise => ok(0),
            Sys::Exit | Sys::ExitGroup => Ok(Ok(None)),
            Sys::Kill | Sys::Tgkill | Sys::Tkill => {
                let (target, signo) = match sys {
                    // A process-directed signal to this process goes to the calling thread.
                    Sys::Kill => {
                        let pid = sa[0];
                        if pid != self.config.pid as i64 && pid != 0 && pid != -1 {
                            return Ok(Err(ESRCH));
                        }
                        (self.threads.current, a[1] as u32)
                    }
                    Sys::Tgkill => {
                        if sa[0] != self.config.pid as i64 {
                            return Ok(Err(ESRCH));
                        }
                        (a[1] as u32, a[2] as u32)
                    }
                    _ => (a[0] as u32, a[1] as u32),
                };
                if !self.threads.threads.contains_key(&target) {
                    return Ok(Err(ESRCH));
                }
                if signo > 64 {
                    return Ok(Err(EINVAL));
                }
                if signo == 0 {
                    return ok(0);
                }
                let code = if sys == Sys::Kill { signals::SI_USER } else { signals::SI_TKILL };
                if target != self.threads.current {
                    self.signals.raise_pending(target, signo);
                    return ok(0);
                }
                // The interrupted context returns 0 from this syscall.
                cpu.set_reg(Reg::General(0), 0);
                match self.signals.deliver(cpu, target, signo, code, 0, self.config.pid)? {
                    signals::Delivery::Handler => self.switched = true,
                    signals::Delivery::Terminate => self.killed = Some(signo),
                    signals::Delivery::Ignored | signals::Delivery::Pending => {}
                }
                ok(0)
            }
            Sys::RtSigaction => {
                let arch = cpu.arch();
                let size = signals::Signals::action_size(arch);
                let signo = a[0] as u32;
                if !(1..=64).contains(&signo) {
                    return Ok(Err(EINVAL));
                }
                if a[2] != 0 {
                    let old = signals::Signals::encode_action(arch, &self.signals.action(signo));
                    cpu.memory_mut().write(a[2], &old)?;
                }
                if a[1] != 0 {
                    let new = cpu.memory().read_vec(a[1], size)?;
                    let action = signals::Signals::decode_action(arch, &new);
                    if !self.signals.set_action(signo, action) {
                        return Ok(Err(EINVAL));
                    }
                    Self::fault_handler_installed(cpu, signo, &action)?;
                }
                ok(0)
            }
            Sys::RtSigprocmask => {
                let tid = self.threads.current;
                let old = self.signals.mask(tid);
                if a[2] != 0 {
                    cpu.memory_mut().write_u64(a[2], old)?;
                }
                if a[1] != 0 {
                    let set = cpu.memory().read_u64(a[1])?;
                    let mask = match a[0] {
                        0 => old | set,
                        1 => old & !set,
                        2 => set,
                        _ => return Ok(Err(EINVAL)),
                    };
                    self.signals.set_mask(tid, mask);
                }
                ok(0)
            }
            Sys::RtSigpending => {
                let pending = self.signals.pending(self.threads.current);
                cpu.memory_mut().write_u64(a[0], pending)?;
                ok(0)
            }
            Sys::Sigaltstack => {
                let tid = self.threads.current;
                let w = if is64 { 8 } else { 4 };
                if a[1] != 0 {
                    let old = self.signals.alt_stack(tid);
                    let sp = cpu.reg(Reg::Sp);
                    let on = old.flags & 2 == 0 && sp > old.sp && sp <= old.sp + old.size;
                    let flags = if old.flags & 2 != 0 { 2 } else if on { 1 } else { 0 };
                    let m = cpu.memory_mut();
                    if is64 {
                        m.write_u64(a[1], old.sp)?;
                        m.write_u32(a[1] + 8, flags)?;
                        m.write_u64(a[1] + 16, old.size)?;
                    } else {
                        m.write_u32(a[1], old.sp as u32)?;
                        m.write_u32(a[1] + 4, flags)?;
                        m.write_u32(a[1] + 8, old.size as u32)?;
                    }
                }
                if a[0] != 0 {
                    let m = cpu.memory();
                    let (sp, flags, size) = if is64 {
                        (m.read_u64(a[0])?, m.read_u32(a[0] + 8)?, m.read_u64(a[0] + 16)?)
                    } else {
                        (m.read_u32(a[0])? as u64, m.read_u32(a[0] + w)?, m.read_u32(a[0] + 2 * w)? as u64)
                    };
                    self.signals.set_alt_stack(tid, signals::AltStack { sp, size, flags });
                }
                ok(0)
            }
            Sys::RtSigreturn | Sys::Sigreturn => {
                self.signals.sigreturn(cpu, self.threads.current, sys == Sys::RtSigreturn)?;
                if self.config.trace_syscalls {
                    eprintln!("[signal] return to {:#x} sp {:#x} lr {:#x} flags {:#x} r0 {:#x}", cpu.pc(), cpu.reg(Reg::Sp), cpu.reg(Reg::Lr), cpu.reg(Reg::Flags), cpu.reg(Reg::General(0)));
                }
                self.switched = true;
                ok(0)
            }
            // The ARM sigaction(2), sigprocmask(2) and sigpending(2) with 32-bit masks.
            Sys::Sigaction => {
                let signo = a[0] as u32;
                if !(1..=64).contains(&signo) {
                    return Ok(Err(EINVAL));
                }
                if a[2] != 0 {
                    let old = signals::Signals::encode_old_action(&self.signals.action(signo));
                    cpu.memory_mut().write(a[2], &old)?;
                }
                if a[1] != 0 {
                    let new = cpu.memory().read_vec(a[1], 16)?;
                    let action = signals::Signals::decode_old_action(&new);
                    if !self.signals.set_action(signo, action) {
                        return Ok(Err(EINVAL));
                    }
                    Self::fault_handler_installed(cpu, signo, &action)?;
                }
                ok(0)
            }
            Sys::Sigprocmask => {
                let tid = self.threads.current;
                let old = self.signals.mask(tid);
                if a[2] != 0 {
                    cpu.memory_mut().write_u32(a[2], old as u32)?;
                }
                if a[1] != 0 {
                    let set = cpu.memory().read_u32(a[1])? as u64;
                    let mask = match a[0] {
                        0 => old | set,
                        1 => old & !set,
                        2 => (old & !0xffff_ffff) | set,
                        _ => return Ok(Err(EINVAL)),
                    };
                    self.signals.set_mask(tid, mask);
                }
                ok(0)
            }
            Sys::Sigpending => {
                let pending = self.signals.pending(self.threads.current);
                cpu.memory_mut().write_u32(a[0], pending as u32)?;
                ok(0)
            }
            Sys::ArmSetTls => {
                cpu.set_reg(Reg::Tls, a[0]);
                ok(0)
            }
            Sys::Prctl => {
                if a[0] == PR_SET_VMA && a[1] == 0 {
                    let label = cpu.memory().read_cstr(a[4], 256).map(|l| format!("[anon:{}]", String::from_utf8_lossy(&l)));
                    if let Ok(label) = label {
                        cpu.memory_mut().label(machine::page_floor(a[2]), page_ceil(a[3]), &label);
                    }
                }
                ok(0)
            }
            Sys::Getrandom => {
                let mut buf = vec![0u8; a[1] as usize];
                self.random_bytes(&mut buf);
                cpu.memory_mut().write(a[0], &buf)?;
                ok(a[1] as i64)
            }
            Sys::Uname => {
                let fields: [&str; 6] = ["Linux", "localhost", "3.18.91", "#1 SMP PREEMPT", if is64 { "aarch64" } else { "armv8l" }, "localdomain"];
                let mut buf = vec![0u8; 65 * 6];
                for (i, f) in fields.iter().enumerate() {
                    buf[i * 65..i * 65 + f.len()].copy_from_slice(f.as_bytes());
                }
                cpu.memory_mut().write(a[0], &buf)?;
                ok(0)
            }
            Sys::Sysinfo => {
                let size = if is64 { 112 } else { 64 };
                cpu.memory_mut().write(a[0], &vec![0u8; size])?;
                ok(0)
            }
            Sys::Getrlimit | Sys::Prlimit64 => {
                let (resource, out) = if sys == Sys::Prlimit64 { (a[1], a[3]) } else { (a[0], a[1]) };
                if out != 0 {
                    // RLIMIT_STACK 8 MiB, everything else unlimited.
                    let current: u64 = if resource == 3 { 8 << 20 } else { u64::MAX };
                    if is64 || sys == Sys::Prlimit64 {
                        cpu.memory_mut().write_u64(out, current)?;
                        cpu.memory_mut().write_u64(out + 8, u64::MAX)?;
                    } else {
                        cpu.memory_mut().write_u32(out, current as u32)?;
                        cpu.memory_mut().write_u32(out + 4, u32::MAX)?;
                    }
                }
                ok(0)
            }
            Sys::Getrusage => {
                cpu.memory_mut().write(a[1], &vec![0u8; if is64 { 144 } else { 72 }])?;
                ok(0)
            }
            Sys::SchedGetaffinity => {
                cpu.memory_mut().write(a[2], &[1])?;
                ok(if is64 { 8 } else { 4 })
            }
            Sys::ClockGettime | Sys::Gettimeofday => {
                let (ns, out) = if sys == Sys::ClockGettime {
                    let Some(ns) = self.clock.by_id(a[0] as u32) else { return Ok(Err(EINVAL)) };
                    (ns, a[1])
                } else {
                    (self.clock.realtime_ns(), a[0])
                };
                let sub = ns % 1_000_000_000;
                let sub = if sys == Sys::ClockGettime { sub } else { sub / 1000 };
                if out != 0 {
                    self.write_pair(cpu, out, ns / 1_000_000_000, sub)?;
                }
                ok(0)
            }
            Sys::ClockGetres => {
                if a[1] != 0 {
                    self.write_pair(cpu, a[1], 0, 1)?;
                }
                ok(0)
            }
            Sys::Nanosleep => ok(0),
            Sys::Futex => {
                let op = a[1] & 0x7f;
                let bitset = if op == 9 || op == 10 { a[5] as u32 } else { u32::MAX };
                match op {
                    0 | 9 => {
                        let current = cpu.memory().read_u32(a[0])?;
                        if current != a[2] as u32 {
                            return Ok(Err(EAGAIN));
                        }
                        // A single guest thread can never be woken.
                        if self.threads.count() == 1 {
                            return Ok(Err(ETIMEDOUT));
                        }
                        let timed = a[3] != 0;
                        let switch = self.threads.wait(cpu, a[0], bitset, timed);
                        self.after_switch(switch);
                        ok(0)
                    }
                    1 | 10 => ok(self.threads.wake(cpu, a[0], bitset, a[2]) as i64),
                    // FUTEX_REQUEUE and FUTEX_CMP_REQUEUE: wake `a[2]`, move up to `a[3]` to `a[4]`.
                    3 | 4 => {
                        if op == 4 && cpu.memory().read_u32(a[0])? != a[5] as u32 {
                            return Ok(Err(EAGAIN));
                        }
                        let woken = self.threads.wake(cpu, a[0], u32::MAX, a[2]);
                        let moved = self.threads.requeue(a[0], a[4], a[3]);
                        ok((woken + if op == 4 { moved } else { 0 }) as i64)
                    }
                    _ => Ok(Err(ENOSYS)),
                }
            }
            Sys::Brk => {
                let wanted = a[0];
                if wanted > self.brk {
                    let start = page_ceil(self.brk);
                    let end = page_ceil(wanted);
                    if end > start && cpu.map(start, end - start, Perms::RW, "[heap]").is_err() {
                        return Ok(Ok(Some(self.brk as i64)));
                    }
                    self.brk = wanted;
                }
                ok(self.brk as i64)
            }
            Sys::Mmap | Sys::Mmap2 => {
                let offset = if sys == Sys::Mmap2 { a[5] * 4096 } else { a[5] };
                self.mmap(cpu, a[0], a[1], a[2], a[3], sa[4], offset)
            }
            Sys::Munmap => {
                cpu.unmap(machine::page_floor(a[0]), page_ceil(a[1]))?;
                ok(0)
            }
            Sys::Mprotect => {
                let start = machine::page_floor(a[0]);
                match cpu.protect(start, page_ceil(a[1] + (a[0] - start)), Perms::from_bits(a[2] as u8)) {
                    Ok(()) => ok(0),
                    Err(_) => Ok(Err(ENOMEM)),
                }
            }
            Sys::Mremap => Ok(Err(ENOMEM)),
            Sys::Mincore => ok(0),
            Sys::Open => self.open(cpu, AT_FDCWD as u64, a[0], a[1]),
            Sys::Openat => self.open(cpu, a[0], a[1], a[2]),
            Sys::Close => {
                let fd = sa[0];
                match self.fds.remove(&fd) {
                    Some(Target::File { path, data, dirty: true, .. }) => {
                        self.trace(format!("close fd={fd} {path} (written {} bytes)", data.len()));
                        self.vfs.store(&path, data);
                        ok(0)
                    }
                    Some(_) => {
                        self.trace(format!("close fd={fd}"));
                        ok(0)
                    }
                    None => Ok(Err(EBADF)),
                }
            }
            Sys::Read => self.read(cpu, sa[0], a[1], a[2], None),
            Sys::Pread64 => {
                let offset = if is64 { a[3] } else { a[4] | (a[5] << 32) };
                self.read(cpu, sa[0], a[1], a[2], Some(offset))
            }
            Sys::Write => {
                let data = cpu.memory().read_vec(a[1], a[2] as usize)?;
                self.write(sa[0], &data)
            }
            Sys::Writev | Sys::Readv => {
                let (word, entry) = if is64 { (8, 16) } else { (4, 8) };
                let mut total = 0i64;
                for i in 0..a[2] {
                    let at = a[1] + i * entry;
                    let (base, len) = if is64 {
                        (cpu.memory().read_u64(at)?, cpu.memory().read_u64(at + word)?)
                    } else {
                        (cpu.memory().read_u32(at)? as u64, cpu.memory().read_u32(at + word)? as u64)
                    };
                    let r = if sys == Sys::Writev {
                        let data = cpu.memory().read_vec(base, len as usize)?;
                        self.write(sa[0], &data)?
                    } else {
                        self.read(cpu, sa[0], base, len, None)?
                    };
                    match r {
                        Ok(Some(n)) => total += n,
                        other => return Ok(other),
                    }
                }
                ok(total)
            }
            Sys::Lseek | Sys::Llseek => {
                let fd = sa[0];
                let (offset, whence) = if sys == Sys::Llseek { (((a[1] << 32) | (a[2] & 0xffff_ffff)) as i64, a[4]) } else { (sa[1], a[2]) };
                let Some(Target::File { data, pos, .. }) = self.fds.get_mut(&fd) else { return Ok(Err(ESPIPE)) };
                let next = match whence {
                    0 => offset,
                    1 => *pos as i64 + offset,
                    2 => data.len() as i64 + offset,
                    _ => return Ok(Err(EINVAL)),
                };
                if next < 0 {
                    return Ok(Err(EINVAL));
                }
                *pos = next as u64;
                if sys == Sys::Llseek {
                    cpu.memory_mut().write_u64(a[3], next as u64)?;
                    ok(0)
                } else {
                    ok(next)
                }
            }
            Sys::Fstat => {
                let fd = sa[0];
                self.stat_fd(cpu, fd, a[1])
            }
            Sys::Fstatat => match self.at_target(cpu, a[0], a[1], a[3]) {
                Ok(Ok(path)) => self.stat_path(cpu, &path, a[2]),
                Ok(Err(fd)) => self.stat_fd(cpu, fd, a[2]),
                Err(e) => Ok(Err(e)),
            },
            Sys::Stat | Sys::Lstat => match self.path(cpu, AT_FDCWD, a[0]) {
                Ok(Some(path)) if !path.is_empty() => self.stat_path(cpu, &path, a[1]),
                Ok(_) => Ok(Err(ENOENT)),
                Err(e) => Ok(Err(e)),
            },
            Sys::Faccessat | Sys::Access => {
                let (dirfd, address) = if sys == Sys::Access { (AT_FDCWD as u64, a[0]) } else { (a[0], a[1]) };
                match self.path(cpu, Self::signed(cpu, dirfd), address) {
                    Ok(Some(path)) if !path.is_empty() => {
                        let found = self.vfs.lookup(&path).is_some();
                        self.trace(format!("access {path} -> {}", if found { "0" } else { "ENOENT" }));
                        if found { ok(0) } else { Ok(Err(ENOENT)) }
                    }
                    Ok(_) => Ok(Err(ENOENT)),
                    Err(e) => Ok(Err(e)),
                }
            }
            Sys::Readlinkat | Sys::Readlink => {
                let (dirfd, address, buf, size) =
                    if sys == Sys::Readlink { (AT_FDCWD as u64, a[0], a[1], a[2]) } else { (a[0], a[1], a[2], a[3]) };
                let path = match self.path(cpu, Self::signed(cpu, dirfd), address) {
                    Ok(Some(p)) => p,
                    Ok(None) => return Ok(Err(EFAULT)),
                    Err(e) => return Ok(Err(e)),
                };
                let target = if path == "/proc/self/exe" {
                    Some(self.config.executable.clone())
                } else if let Some(fd) = path.strip_prefix("/proc/self/fd/").and_then(|f| f.parse::<i64>().ok()) {
                    match self.fds.get(&fd) {
                        Some(Target::File { path, .. }) | Some(Target::Directory { path }) => Some(path.clone()),
                        _ => None,
                    }
                } else {
                    None
                };
                match target {
                    Some(t) => {
                        let bytes = &t.as_bytes()[..t.len().min(size as usize)];
                        cpu.memory_mut().write(buf, bytes)?;
                        ok(bytes.len() as i64)
                    }
                    None => Ok(Err(EINVAL)),
                }
            }
            Sys::Getcwd => {
                let mut bytes = self.cwd.clone().into_bytes();
                bytes.push(0);
                if bytes.len() as u64 > a[1] {
                    return Ok(Err(ERANGE));
                }
                cpu.memory_mut().write(a[0], &bytes)?;
                ok(bytes.len() as i64)
            }
            Sys::Chdir => match self.path(cpu, AT_FDCWD, a[0]) {
                Ok(Some(p)) if matches!(self.vfs.lookup(&p), Some(Node::Directory)) => {
                    self.cwd = p;
                    ok(0)
                }
                Ok(_) => Ok(Err(ENOENT)),
                Err(e) => Ok(Err(e)),
            },
            Sys::Mkdirat => ok(0),
            Sys::Unlinkat => match self.path(cpu, sa[0], a[1]) {
                Ok(Some(p)) => {
                    let existed = self.vfs.remove(&p);
                    self.trace(format!("unlink {p}"));
                    if existed { ok(0) } else { Ok(Err(ENOENT)) }
                }
                Ok(None) => Ok(Err(EFAULT)),
                Err(e) => Ok(Err(e)),
            },
            Sys::Fcntl => {
                let fd = sa[0];
                if !self.fds.contains_key(&fd) {
                    return Ok(Err(EBADF));
                }
                match a[1] {
                    0 | 1030 => self.dup(fd, a[2] as i64),
                    _ => ok(0),
                }
            }
            Sys::Dup => self.dup(sa[0], 0),
            Sys::Dup3 => {
                let (old, new) = (sa[0], sa[1]);
                match self.fds.get(&old).cloned() {
                    Some(t) => {
                        self.fds.insert(new, t);
                        ok(new)
                    }
                    None => Ok(Err(EBADF)),
                }
            }
            Sys::Ioctl => Ok(Err(ENOTTY)),
            Sys::Ftruncate => {
                let fd = sa[0];
                match self.fds.get_mut(&fd) {
                    Some(Target::File { data, dirty, .. }) => {
                        data.resize(a[1] as usize, 0);
                        *dirty = true;
                        ok(0)
                    }
                    _ => Ok(Err(EBADF)),
                }
            }
            Sys::Socket => Ok(Err(EAFNOSUPPORT)),
            Sys::Statfs | Sys::Fstatfs => Ok(Err(ENOSYS)),
            _ => Ok(Err(ENOSYS)),
        }
    }

    fn write_pair(&self, cpu: &mut dyn Cpu, at: u64, a: u64, b: u64) -> Result<(), Error> {
        if Self::is64(cpu) {
            cpu.memory_mut().write_u64(at, a)?;
            cpu.memory_mut().write_u64(at + 8, b)?;
        } else {
            cpu.memory_mut().write_u32(at, a as u32)?;
            cpu.memory_mut().write_u32(at + 4, b as u32)?;
        }
        Ok(())
    }

    fn next_fd(&self) -> i64 {
        (0..).find(|fd| !self.fds.contains_key(fd)).expect("free descriptor")
    }

    fn dup(&mut self, fd: i64, lowest: i64) -> Result<Result<Option<i64>, i64>, Error> {
        let Some(target) = self.fds.get(&fd).cloned() else { return Ok(Err(EBADF)) };
        let new = (lowest.max(0)..).find(|f| !self.fds.contains_key(f)).expect("free descriptor");
        self.fds.insert(new, target);
        Ok(Ok(Some(new)))
    }

    fn open(&mut self, cpu: &mut dyn Cpu, dirfd: u64, address: u64, flags: u64) -> Result<Result<Option<i64>, i64>, Error> {
        let path = match self.path(cpu, Self::signed(cpu, dirfd), address) {
            Ok(Some(p)) if !p.is_empty() => p,
            Ok(_) => return Ok(Err(ENOENT)),
            Err(e) => return Ok(Err(e)),
        };
        let writable = flags & O_ACCMODE != 0;
        if path.starts_with("/proc/") {
            self.refresh_proc(cpu.memory());
        }
        let target = match path.as_str() {
            "/dev/null" => Some(Target::Null),
            "/dev/urandom" | "/dev/random" => Some(Target::Random),
            _ => match self.vfs.lookup(&path) {
                Some(Node::Directory) => {
                    if writable {
                        self.trace(format!("open {path} flags={flags:#x} -> EISDIR"));
                        return Ok(Err(EISDIR));
                    }
                    Some(Target::Directory { path: path.clone() })
                }
                Some(Node::File(_)) if flags & O_CREAT != 0 && flags & O_EXCL != 0 => {
                    self.trace(format!("open {path} flags={flags:#x} -> EEXIST"));
                    return Ok(Err(EEXIST));
                }
                Some(Node::File(_)) if flags & O_DIRECTORY != 0 => {
                    return Ok(Err(ENOTDIR));
                }
                Some(Node::File(data)) => {
                    let data = if flags & O_TRUNC != 0 && writable { Vec::new() } else { data };
                    Some(Target::File {
                        path: path.clone(),
                        pos: 0,
                        writable,
                        append: flags & O_APPEND != 0,
                        dirty: flags & O_TRUNC != 0 && writable,
                        data,
                    })
                }
                None if flags & O_CREAT != 0 => Some(Target::File {
                    path: path.clone(),
                    data: Vec::new(),
                    pos: 0,
                    writable: true,
                    append: flags & O_APPEND != 0,
                    dirty: true,
                }),
                None => None,
            },
        };
        match target {
            Some(t) => {
                let fd = self.next_fd();
                self.fds.insert(fd, t);
                self.trace(format!("open {path} flags={flags:#x} -> {fd}"));
                Ok(Ok(Some(fd)))
            }
            None => {
                self.trace(format!("open {path} flags={flags:#x} -> ENOENT"));
                self.note_missing_file(&path);
                Ok(Err(ENOENT))
            }
        }
    }

    fn read(&mut self, cpu: &mut dyn Cpu, fd: i64, buf: u64, len: u64, at: Option<u64>) -> Result<Result<Option<i64>, i64>, Error> {
        let mut random = vec![0u8; 0];
        let bytes: Vec<u8> = match self.fds.get_mut(&fd) {
            Some(Target::File { data, pos, .. }) => {
                let start = at.unwrap_or(*pos) as usize;
                let end = (start + len as usize).min(data.len());
                let chunk = if start < end { data[start..end].to_vec() } else { Vec::new() };
                if at.is_none() {
                    *pos += chunk.len() as u64;
                }
                chunk
            }
            Some(Target::Random) => {
                random.resize(len as usize, 0);
                self.random_bytes(&mut random);
                random
            }
            Some(Target::Null) | Some(Target::Stdin) => Vec::new(),
            Some(Target::Directory { .. }) => return Ok(Err(EISDIR)),
            Some(_) => return Ok(Err(EBADF)),
            None => return Ok(Err(EBADF)),
        };
        cpu.memory_mut().write(buf, &bytes)?;
        Ok(Ok(Some(bytes.len() as i64)))
    }

    fn write(&mut self, fd: i64, bytes: &[u8]) -> Result<Result<Option<i64>, i64>, Error> {
        let echo = self.config.echo_output;
        match self.fds.get_mut(&fd) {
            Some(Target::Stdout) => {
                if echo {
                    print!("{}", String::from_utf8_lossy(bytes));
                }
                self.stdout.extend_from_slice(bytes);
            }
            Some(Target::Stderr) => {
                if echo {
                    eprint!("{}", String::from_utf8_lossy(bytes));
                }
                self.stderr.extend_from_slice(bytes);
            }
            Some(Target::File { data, pos, writable, append, dirty, .. }) => {
                if !*writable {
                    return Ok(Err(EBADF));
                }
                if *append {
                    *pos = data.len() as u64;
                }
                let start = *pos as usize;
                if data.len() < start + bytes.len() {
                    data.resize(start + bytes.len(), 0);
                }
                data[start..start + bytes.len()].copy_from_slice(bytes);
                *pos += bytes.len() as u64;
                *dirty = true;
            }
            Some(Target::Null) => {}
            Some(_) => return Ok(Err(EBADF)),
            None => return Ok(Err(EBADF)),
        }
        Ok(Ok(Some(bytes.len() as i64)))
    }

    #[allow(clippy::too_many_arguments)]
    fn mmap(
        &mut self,
        cpu: &mut dyn Cpu,
        hint: u64,
        length: u64,
        prot: u64,
        flags: u64,
        fd: i64,
        offset: u64,
    ) -> Result<Result<Option<i64>, i64>, Error> {
        if length == 0 {
            return Ok(Err(EINVAL));
        }
        let size = page_ceil(length);
        let address = if flags & MAP_FIXED != 0 {
            if hint & (machine::PAGE_SIZE - 1) != 0 {
                return Ok(Err(EINVAL));
            }
            cpu.unmap(hint, size)?;
            hint
        } else if hint != 0 && hint >= self.mmap_low && cpu.memory().is_free(machine::page_floor(hint), size) {
            machine::page_floor(hint)
        } else {
            match cpu.memory().find_free(size, self.mmap_low, self.mmap_high) {
                Ok(a) => a,
                Err(_) => return Ok(Err(ENOMEM)),
            }
        };
        let label = if flags & MAP_ANONYMOUS != 0 || fd < 0 {
            String::new()
        } else {
            match self.fds.get(&fd) {
                Some(Target::File { path, .. }) => path.clone(),
                _ => return Ok(Err(EBADF)),
            }
        };
        // Write the file contents first, then apply the requested protection.
        cpu.map(address, size, Perms::RW, &label)?;
        if flags & MAP_ANONYMOUS == 0 && fd >= 0
            && let Some(Target::File { data, .. }) = self.fds.get(&fd) {
                let start = (offset as usize).min(data.len());
                let end = (start + length as usize).min(data.len());
                let chunk = data[start..end].to_vec();
                cpu.memory_mut().write(address, &chunk)?;
                self.trace(format!("mmap {label} offset={offset:#x} length={length:#x} -> {address:#x}"));
            }
        cpu.protect(address, size, Perms::from_bits(prot as u8))?;
        Ok(Ok(Some(address as i64)))
    }

    fn stat_fd(&mut self, cpu: &mut dyn Cpu, fd: i64, out: u64) -> Result<Result<Option<i64>, i64>, Error> {
        let (mode, size) = match self.fds.get(&fd) {
            Some(Target::File { data, .. }) => (S_IFREG | 0o644, data.len() as u64),
            Some(Target::Directory { .. }) => (S_IFDIR | 0o755, 4096),
            Some(_) => (S_IFCHR | 0o666, 0),
            None => return Ok(Err(EBADF)),
        };
        self.trace(format!("fstat fd={fd} -> size={size}"));
        self.write_stat(cpu, out, mode, size)?;
        Ok(Ok(Some(0)))
    }

    fn stat_path(&mut self, cpu: &mut dyn Cpu, path: &str, out: u64) -> Result<Result<Option<i64>, i64>, Error> {
        let (mode, size) = match path {
            "/dev/null" | "/dev/urandom" | "/dev/random" => (S_IFCHR | 0o666, 0),
            _ => match self.vfs.lookup(path) {
                Some(Node::File(d)) => (S_IFREG | 0o644, d.len() as u64),
                Some(Node::Directory) => (S_IFDIR | 0o755, 4096),
                None => {
                    self.trace(format!("stat {path} -> ENOENT"));
                    self.note_missing_file(path);
                    return Ok(Err(ENOENT));
                }
            },
        };
        self.trace(format!("stat {path} -> size={size}"));
        self.write_stat(cpu, out, mode, size)?;
        Ok(Ok(Some(0)))
    }

    fn write_stat(&self, cpu: &mut dyn Cpu, out: u64, mode: u32, size: u64) -> Result<(), Error> {
        let now = self.clock.realtime_ns() / 1_000_000_000;
        let mut b = Vec::new();
        if Self::is64(cpu) {
            // struct stat (AArch64, 128 bytes)
            b.extend_from_slice(&0x803u64.to_le_bytes()); // st_dev
            b.extend_from_slice(&1u64.to_le_bytes()); // st_ino
            b.extend_from_slice(&mode.to_le_bytes());
            b.extend_from_slice(&1u32.to_le_bytes()); // st_nlink
            b.extend_from_slice(&self.config.uid.to_le_bytes());
            b.extend_from_slice(&self.config.uid.to_le_bytes());
            b.extend_from_slice(&0u64.to_le_bytes()); // st_rdev
            b.extend_from_slice(&0u64.to_le_bytes()); // pad
            b.extend_from_slice(&size.to_le_bytes());
            b.extend_from_slice(&4096u32.to_le_bytes()); // st_blksize
            b.extend_from_slice(&0u32.to_le_bytes());
            b.extend_from_slice(&size.div_ceil(512).to_le_bytes());
            for _ in 0..3 {
                b.extend_from_slice(&now.to_le_bytes());
                b.extend_from_slice(&0u64.to_le_bytes());
            }
            b.extend_from_slice(&[0u8; 8]);
        } else {
            // struct stat64 (ARM EABI, 104 bytes)
            b.extend_from_slice(&0x803u64.to_le_bytes());
            b.extend_from_slice(&[0u8; 4]);
            b.extend_from_slice(&1u32.to_le_bytes()); // __st_ino
            b.extend_from_slice(&mode.to_le_bytes());
            b.extend_from_slice(&1u32.to_le_bytes());
            b.extend_from_slice(&self.config.uid.to_le_bytes());
            b.extend_from_slice(&self.config.uid.to_le_bytes());
            b.extend_from_slice(&0u64.to_le_bytes()); // st_rdev
            b.extend_from_slice(&[0u8; 4]);
            b.extend_from_slice(&[0u8; 4]); // align st_size to 8
            b.extend_from_slice(&size.to_le_bytes());
            b.extend_from_slice(&4096u32.to_le_bytes());
            b.extend_from_slice(&[0u8; 4]);
            b.extend_from_slice(&size.div_ceil(512).to_le_bytes());
            for _ in 0..3 {
                b.extend_from_slice(&(now as u32).to_le_bytes());
                b.extend_from_slice(&0u32.to_le_bytes());
            }
            b.extend_from_slice(&1u64.to_le_bytes()); // st_ino
        }
        cpu.memory_mut().write(out, &b)?;
        Ok(())
    }

    /// Records a path the guest looked for and did not find. Skips the process-mirror files the
    /// runtime regenerates (always present) so only genuine misses are reported.
    fn note_missing_file(&mut self, path: &str) {
        *self.missing_files.entry(path.to_string()).or_insert(0) += 1;
    }

    /// `/proc/self/maps` text for the current memory map.
    pub fn proc_maps(memory: &machine::Memory) -> String {
        let mut out = String::new();
        for m in memory.mappings() {
            let p = m.perms;
            out.push_str(&format!(
                "{:08x}-{:08x} {}{}{}p 00000000 00:00 0          {}\n",
                m.start,
                m.end,
                if p.contains(Perms::READ) { 'r' } else { '-' },
                if p.contains(Perms::WRITE) { 'w' } else { '-' },
                if p.contains(Perms::EXEC) { 'x' } else { '-' },
                m.label
            ));
        }
        out
    }

    /// Refreshes the virtual files that mirror process state.
    pub fn refresh_proc(&mut self, memory: &machine::Memory) {
        self.vfs.add_file("/proc/self/maps", Self::proc_maps(memory).into_bytes());
        let pid = self.config.pid;
        self.vfs.add_file(&format!("/proc/{pid}/maps"), Self::proc_maps(memory).into_bytes());
        let mut cmdline = self.config.process_name.clone().into_bytes();
        cmdline.push(0);
        self.vfs.add_file("/proc/self/cmdline", cmdline.clone());
        self.vfs.add_file(&format!("/proc/{pid}/cmdline"), cmdline);
        let status = format!(
            "Name:	{}
State:	R (running)
Tgid:	{pid}
Pid:	{pid}
PPid:	1
TracerPid:	0
Uid:	{uid}	{uid}	{uid}	{uid}
",
            self.config.process_name,
            uid = self.config.uid
        );
        self.vfs.add_file("/proc/self/status", status.clone().into_bytes());
        self.vfs.add_file(&format!("/proc/{pid}/status"), status.into_bytes());
    }

    pub fn brk_base(&self) -> u64 {
        self.brk_base
    }
}
