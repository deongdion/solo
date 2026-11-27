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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
    /// The handle that interrupts this CPU from another thread.
    fn interrupt(&self) -> Interrupt;

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

/// Guest address of the return trampoline page.
pub const TRAMPOLINE: u64 = 0xffff_0000;

/// Machine code for the return trampoline.
pub fn trampoline_code(arch: Arch) -> Vec<u8> {
    match arch {
        // svc #RETURN_SVC
        Arch::Arm64 => (0xd400_0001u32 | (RETURN_SVC << 5)).to_le_bytes().to_vec(),
        // svc #RETURN_SVC (A32 encoding); callers return here in ARM state.
        Arch::Arm32 => (0xef00_0000u32 | RETURN_SVC).to_le_bytes().to_vec(),
    }
}
