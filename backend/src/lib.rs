//! CPU backend trait and implementations: dynarmic (default), unicorn (parity), HVF and KVM (later).
//!
//! Guest memory belongs to `machine::Memory`; a backend only executes on top of it. Every call into
//! guest code returns through a trampoline page that holds a supervisor call with `RETURN_SVC`, so all
//! backends stop the same way.

use machine::{Memory, Perms};

#[cfg(feature = "dynarmic")]
mod dynarmic;
#[cfg(feature = "unicorn")]
mod unicorn;

#[cfg(feature = "dynarmic")]
pub use dynarmic::Dynarmic;
#[cfg(feature = "unicorn")]
pub use unicorn::Unicorn;

/// Immediate of the supervisor call that marks a return to the host.
pub const RETURN_SVC: u32 = 0xff;

/// Instruction set of the guest.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Arch {
    Arm64,
    Arm32,
}

impl Arch {
    pub fn address_bits(self) -> u32 {
        match self {
            Arch::Arm64 => 36,
            Arch::Arm32 => 32,
        }
    }
}

/// Architecture-neutral register names used by the call path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Reg {
    /// Argument and general register `n` (x0..x30 on A64, r0..r12 on A32).
    General(u8),
    Sp,
    Lr,
    Pc,
    /// Thread pointer (TPIDR_EL0 on A64, TPIDRURO on A32).
    Tls,
    /// 64-bit floating-point register `n`: the low half of Vn on A64, Dn on A32. Writing clears
    /// the upper half of Vn on A64.
    Fp(u8),
    /// Upper 64 bits of Vn on A64 (0 and ignored on A32, whose Q registers are D pairs).
    VecHigh(u8),
    /// Condition flags and state: PSTATE (NZCV) on A64, CPSR on A32.
    Flags,
    /// FPCR on A64, FPSCR on A32.
    FpControl,
    /// FPSR on A64 (0 and ignored on A32, where FPSCR holds the status).
    FpStatus,
}

/// The register state of one guest thread, for switching threads on one CPU.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Context {
    regs: Vec<u64>,
}

fn context_regs(arch: Arch) -> Vec<Reg> {
    let general = if arch == Arch::Arm64 { 30 } else { 13 };
    let mut regs: Vec<Reg> = (0..general).map(Reg::General).collect();
    regs.extend([Reg::Sp, Reg::Lr, Reg::Tls, Reg::Flags, Reg::FpControl, Reg::FpStatus]);
    // Fp before VecHigh: writing Fp clears the upper half on A64.
    regs.extend((0..32).map(Reg::Fp));
    if arch == Arch::Arm64 {
        regs.extend((0..32).map(Reg::VecHigh));
    }
    // PC last: on A32 setting the PC selects ARM or Thumb, which writing Flags must not undo.
    regs.push(Reg::Pc);
    regs
}

/// Saves every register a thread switch must preserve.
pub fn save_context(cpu: &dyn Cpu) -> Context {
    let regs = context_regs(cpu.arch());
    Context { regs: regs.iter().map(|&r| if r == Reg::Pc { cpu.pc() } else { cpu.reg(r) }).collect() }
}

/// Loads a context saved by `save_context` on a CPU of the same architecture.
pub fn load_context(cpu: &mut dyn Cpu, context: &Context) {
    let regs = context_regs(cpu.arch());
    debug_assert_eq!(regs.len(), context.regs.len());
    cpu.clear_exclusive();
    for (r, &v) in regs.into_iter().zip(&context.regs) {
        if r == Reg::Pc {
            cpu.set_pc(v);
        } else {
            cpu.set_reg(r, v);
        }
    }
}

impl Context {
    pub fn get(&self, arch: Arch, reg: Reg) -> Option<u64> {
        context_regs(arch).iter().position(|&r| r == reg).map(|i| self.regs[i])
    }

    pub fn set(&mut self, arch: Arch, reg: Reg, value: u64) {
        if let Some(i) = context_regs(arch).iter().position(|&r| r == reg) {
            self.regs[i] = value;
        }
    }
}

/// Why a run returned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stop {
    /// The guest executed the return trampoline.
    Returned,
    /// A supervisor call other than the return trampoline.
    Svc(u32),
    /// The instruction budget ran out.
    Budget,
    /// Another thread requested a halt.
    Halted,
    /// Unmapped access.
    Fault { address: u64, write: bool },
    /// Undefined instruction, breakpoint or similar.
    Exception { pc: u64, kind: u32 },
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Memory(#[from] machine::Error),
    #[error("backend failure: {0}")]
    Backend(String),
}

/// Stops a running guest from another thread (a deadline or a cancellation).
///
/// `interrupt` sets a flag the runtime checks between resumes and, for backends that support
/// it, halts the JIT at the next block boundary. A halt that arrives while no guest code runs is
/// seen by the flag when the runtime next looks. Safe to call after the CPU is gone.
#[derive(Clone, Default)]
pub struct Interrupt {
    state: std::sync::Arc<InterruptState>,
}

#[derive(Default)]
struct InterruptState {
    requested: std::sync::atomic::AtomicBool,
    /// Halts the backend; cleared when the backend is dropped.
    halt: std::sync::Mutex<Option<Box<dyn Fn() + Send>>>,
}

impl Interrupt {
    pub fn interrupt(&self) {
        self.state.requested.store(true, std::sync::atomic::Ordering::SeqCst);
        if let Some(halt) = self.state.halt.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
            halt();
        }
    }

    /// Whether an interrupt arrived since the last `take`; clears it.
    pub fn take(&self) -> bool {
        self.state.requested.swap(false, std::sync::atomic::Ordering::SeqCst)
    }

    pub fn is_requested(&self) -> bool {
        self.state.requested.load(std::sync::atomic::Ordering::SeqCst)
    }

    pub(crate) fn set_halt(&self, halt: Option<Box<dyn Fn() + Send>>) {
        *self.state.halt.lock().unwrap_or_else(|e| e.into_inner()) = halt;
    }
}

/// Executes guest code on a `Memory`.
pub trait Cpu: Send {
    fn arch(&self) -> Arch;
    fn reg(&self, reg: Reg) -> u64;
    fn set_reg(&mut self, reg: Reg, value: u64);
    fn memory(&self) -> &Memory;
    fn memory_mut(&mut self) -> &mut Memory;
    /// Maps a zero-filled range and makes it visible to the backend.
    fn map(&mut self, address: u64, length: u64, perms: Perms, label: &str) -> Result<(), Error>;
    /// Unmaps every mapped page in a range.
    fn unmap(&mut self, address: u64, length: u64) -> Result<(), Error>;
    fn protect(&mut self, address: u64, length: u64, perms: Perms) -> Result<(), Error>;
    /// Sets the next instruction. On A32 the low bit selects Thumb.
    fn set_pc(&mut self, pc: u64);
    /// Current instruction address; on A32 the low bit is set in Thumb state.
    fn pc(&self) -> u64;
    /// Continues from the current state until a stop condition or until `budget` instructions ran.
    fn resume(&mut self, budget: u64) -> Result<Stop, Error>;
    /// Instructions the last `resume` ran. Backends that cannot count report 0 unless the budget
    /// ran out, so callers that spread one budget over several resumes stay conservative.
    fn executed(&self) -> u64;
    /// Discards translated code for a range after the guest code there was rewritten.
    fn invalidate(&mut self, address: u64, length: u64);
    /// Clears the exclusive monitor (LDREX/LDXR reservations), as a thread switch must.
    fn clear_exclusive(&mut self) {}
    /// From now on a memory fault must stop at the faulting instruction with the registers as
    /// they were before it, as delivering SIGSEGV to a guest handler needs. A no-op for backends
    /// that always stop there.
    fn require_precise_faults(&mut self) -> Result<(), Error> {
        Ok(())
    }
    /// Whether a memory fault stops at the faulting instruction.
    fn precise_faults(&self) -> bool {
        true
    }
    /// The handle that interrupts this CPU from another thread.
    fn interrupt(&self) -> Interrupt;
    /// A new CPU of the same kind and options with a copy of this one's memory and registers.
    /// Translated code is not copied; the copy translates again on first use.
    fn fork(&self) -> Result<Box<dyn Cpu>, Error>;

    /// Replaces this CPU's memory and registers with a copy of `from`'s, keeping the engine
    /// (creating a dynarmic JIT costs tens of milliseconds). Translated code is discarded.
    fn restore(&mut self, from: &dyn Cpu) -> Result<(), Error>;

    /// Executes exactly one instruction at the current PC: `Stop::Budget` after it, or what
    /// stopped it (a syscall, a fault).
    fn step(&mut self) -> Result<Stop, Error> {
        self.resume(1)
    }

    /// Runs from `pc`.
    fn run(&mut self, pc: u64, budget: u64) -> Result<Stop, Error> {
        self.set_pc(pc);
        self.resume(budget)
    }

    /// Calls a guest function with integer arguments and returns the first result register.
    /// Only usable for code that makes no supervisor calls; the runtime drives everything else.
    fn call(&mut self, function: u64, args: &[u64], budget: u64) -> Result<(Stop, u64), Error> {
        let stack = self.reg(Reg::Sp);
        for (i, value) in args.iter().enumerate() {
            self.set_reg(Reg::General(i as u8), *value);
        }
        self.set_reg(Reg::Lr, TRAMPOLINE);
        let stop = self.run(function, budget)?;
        let result = self.reg(Reg::General(0));
        self.set_reg(Reg::Sp, stack);
        Ok((stop, result))
    }
}

/// Copies every mapping (contents, permissions, labels) and the registers that matter between
/// calls (general, SP, LR, PC, TLS, the low 64 bits of each FP/SIMD register) from `from` into
/// `to`. Ranges `to` already maps (the trampoline) are overwritten in place.
pub fn copy_state(from: &dyn Cpu, to: &mut dyn Cpu) -> Result<(), Error> {
    let mappings: Vec<machine::Mapping> = from.memory().mappings().cloned().collect();
    let mut page = vec![0u8; machine::PAGE_SIZE as usize];
    for m in &mappings {
        let length = m.end - m.start;
        let fresh = to.memory().is_free(m.start, length);
        if fresh {
            to.map(m.start, length, m.perms, &m.label)?;
        }
        let mut at = m.start;
        while at < m.end {
            from.memory().read(at, &mut page)?;
            // Fresh mappings are zero-filled; most of a process image is.
            if !fresh || page.iter().any(|&b| b != 0) {
                to.memory_mut().write(at, &page)?;
            }
            at += machine::PAGE_SIZE;
        }
        to.protect(m.start, length, m.perms)?;
    }
    let general = if from.arch() == Arch::Arm64 { 30 } else { 13 };
    for i in 0..general {
        to.set_reg(Reg::General(i), from.reg(Reg::General(i)));
    }
    for i in 0..32 {
        to.set_reg(Reg::Fp(i), from.reg(Reg::Fp(i)));
    }
    for r in [Reg::Sp, Reg::Lr, Reg::Tls] {
        to.set_reg(r, from.reg(r));
    }
    to.set_pc(from.pc());
    Ok(())
}

/// `Cpu::restore` for any backend: unmaps everything but the trampoline, drops translated code,
/// then copies `from`.
pub fn restore_state(to: &mut dyn Cpu, from: &dyn Cpu) -> Result<(), Error> {
    let own: Vec<(u64, u64)> = to.memory().mappings().filter(|m| m.start != TRAMPOLINE).map(|m| (m.start, m.end)).collect();
    for (start, end) in own {
        to.unmap(start, end - start)?;
    }
    let bits = to.memory().address_bits();
    to.invalidate(0, 1u64 << bits);
    copy_state(from, to)
}

/// `Stop::Exception` kind of a breakpoint instruction (BRK, BKPT), as dynarmic numbers it on both
/// architectures.
pub const EXCEPTION_BREAKPOINT: u32 = 8;

/// Offset in the trampoline page of the rt_sigreturn stub (ARM state on A32).
pub const SIGRETURN_STUB_OFFSET: u64 = 0x20;

/// Guest address of the return trampoline page.
pub const TRAMPOLINE: u64 = 0xffff_0000;

/// Contents of the return trampoline page.
///
/// On A32 the page is also Linux's vector page, so it carries the kernel user helpers at their
/// fixed addresses (Documentation/arch/arm/kernel_user_helpers.rst), which bionic's 32-bit
/// pthread code calls.
pub fn trampoline_code(arch: Arch) -> Vec<u8> {
    match arch {
        Arch::Arm64 => {
            let mut code = vec![0u8; SIGRETURN_STUB_OFFSET as usize + 8];
            // svc #RETURN_SVC
            code[..4].copy_from_slice(&(0xd400_0001u32 | (RETURN_SVC << 5)).to_le_bytes());
            // Signal return for handlers without sa_restorer: mov x8, #139 (rt_sigreturn); svc #0
            code[SIGRETURN_STUB_OFFSET as usize..].copy_from_slice(&[0x68, 0x11, 0x80, 0xd2, 0x01, 0x00, 0x00, 0xd4]);
            code
        }
        Arch::Arm32 => {
            let mut page = vec![0u8; machine::PAGE_SIZE as usize];
            let mut put = |offset: usize, words: &[u32]| {
                for (i, w) in words.iter().enumerate() {
                    page[offset + i * 4..offset + i * 4 + 4].copy_from_slice(&w.to_le_bytes());
                }
            };
            // svc #RETURN_SVC (A32 encoding); callers return here in ARM state.
            put(0, &[0xef00_0000 | RETURN_SVC]);
            // Signal return for handlers without sa_restorer: mov r7, #173 (rt_sigreturn); svc #0
            put(SIGRETURN_STUB_OFFSET as usize, &[0xe3a0_70ad, 0xef00_0000]);
            // The same for the older frame of handlers without SA_SIGINFO: mov r7, #119 (sigreturn); svc #0
            put(SIGRETURN_STUB_OFFSET as usize + 8, &[0xe3a0_7077, 0xef00_0000]);
            // 0xffff0fa0 __kuser_memory_barrier: dmb ish; bx lr
            put(0xfa0, &[0xf57f_f05b, 0xe12f_ff1e]);
            // 0xffff0fc0 __kuser_cmpxchg(oldval r0, newval r1, ptr r2): r0 = 0 and C set on success.
            put(
                0xfc0,
                &[
                    0xe192_3f9f, // 1: ldrex r3, [r2]
                    0xe053_3000, //    subs r3, r3, r0
                    0x0182_3f91, //    strexeq r3, r1, [r2]
                    0x0333_0001, //    teqeq r3, #1
                    0x0aff_fffa, //    beq 1b
                    0xe273_0000, //    rsbs r0, r3, #0
                    0xe12f_ff1e, //    bx lr
                ],
            );
            // 0xffff0fe0 __kuser_get_tls: mrc p15, 0, r0, c13, c0, 3; bx lr
            put(0xfe0, &[0xee1d_0f70, 0xe12f_ff1e]);
            // 0xffff0ffc __kuser_helper_version: the helpers above (version 3 adds get_tls).
            put(0xffc, &[3]);
            page
        }
    }
}
