use std::ffi::c_void;

use dynarmicsys as sys;
use machine::{Memory, Perms};

use crate::{Arch, Cpu, Error, RETURN_SVC, Reg, Stop, TRAMPOLINE, trampoline_code};

/// Instructions per JIT entry; an interrupt is seen within one slice (a few milliseconds).
const INTERRUPT_SLICE: u64 = 1 << 22;

/// Code cache per instance. dynarmic defaults to 128 MiB.
const DEFAULT_CODE_CACHE: u64 = 0;

struct Events {
    stop: Option<Stop>,
    /// The boxed memory of the same `Dynarmic`; only read, and only while the JIT runs.
    memory: *const Memory,
    no_execute: u32,
}

/// dynarmic JIT on top of a `Memory` page table.
pub struct Dynarmic {
    arch: Arch,
    // Declared before `memory` and `events`: the JIT holds pointers into both and is dropped first.
    cpu: Handle,
    memory: Box<Memory>,
    executed: u64,
    events: Box<Events>,
    interrupt: crate::Interrupt,
    code_cache: u64,
    precise_faults: bool,
}

struct Handle(*mut sys::DynCpu);

/// The JIT pointer, for halting from another thread (dynarmic's HaltExecution is atomic).
struct HaltTarget(*mut sys::DynCpu);
// SAFETY: only used to call dyn_halt, which is thread-safe, and only while the Dynarmic that
// owns the JIT is alive (Drop clears it under the interrupt lock before freeing the JIT).
unsafe impl Send for HaltTarget {}

impl Drop for Dynarmic {
    fn drop(&mut self) {
        self.interrupt.set_halt(None);
    }
}

fn new_jit(arch: Arch, memory: &mut Memory, events: &mut Events, code_cache: u64, precise_faults: bool) -> Result<*mut sys::DynCpu, Error> {
    let hooks = sys::DynHooks {
        user: (events as *mut Events).cast(),
        on_svc: Some(on_svc),
        on_exception: Some(on_exception),
        on_fault: Some(on_fault),
        can_execute: Some(can_execute),
    };
    let is64 = (arch == Arch::Arm64) as i32;
    let flags = if precise_faults { sys::DYN_PRECISE_FAULTS } else { 0 };
    // SAFETY: the page table and events are boxed and outlive the handle (field order); dyn_new
    // copies the hooks.
    let raw = unsafe { sys::dyn_new(is64, memory.page_table(), arch.address_bits(), code_cache, flags, &hooks) };
    if raw.is_null() {
        return Err(Error::Backend("dyn_new failed".into()));
    }
    Ok(raw)
}

fn set_halt_target(interrupt: &crate::Interrupt, raw: *mut sys::DynCpu) {
    let target = HaltTarget(raw);
    interrupt.set_halt(Some(Box::new(move || {
        let target = &target;
        // SAFETY: see HaltTarget.
        unsafe { sys::dyn_halt(target.0) }
    })));
}

impl Drop for Handle {
    fn drop(&mut self) {
        // SAFETY: created by dyn_new and freed once.
        unsafe { sys::dyn_free(self.0) };
    }
}

// SAFETY: the JIT and its memory move together; dynarmic has no thread affinity.
unsafe impl Send for Dynarmic {}

unsafe extern "C" fn on_svc(user: *mut c_void, imm: u32) -> i32 {
    // SAFETY: `user` is the boxed Events owned by the Dynarmic value that is running.
    let events = unsafe { &mut *(user as *mut Events) };
    events.stop = Some(if imm == RETURN_SVC { Stop::Returned } else { Stop::Svc(imm) });
    1
}

unsafe extern "C" fn on_exception(user: *mut c_void, pc: u64, kind: u32) {
    // SAFETY: as in on_svc.
    let events = unsafe { &mut *(user as *mut Events) };
    // Reaching a page that may not execute is a fetch fault, the same as on hardware.
    events.stop = Some(if kind == events.no_execute { Stop::Fault { address: pc, write: false } } else { Stop::Exception { pc, kind } });
}

unsafe extern "C" fn on_fault(user: *mut c_void, address: u64, _size: u32, is_write: u32) {
    // SAFETY: as in on_svc.
    let events = unsafe { &mut *(user as *mut Events) };
    events.stop.get_or_insert(Stop::Fault { address, write: is_write != 0 });
}

unsafe extern "C" fn can_execute(user: *mut c_void, address: u64) -> i32 {
    // SAFETY: as in on_svc; `memory` points into the Box owned by the same Dynarmic and is not
    // mutated while the JIT runs.
    let events = unsafe { &*(user as *const Events) };
    let memory = unsafe { &*events.memory };
    memory.mapping_at(address).is_some_and(|m| m.perms.contains(Perms::EXEC)) as i32
}

impl Dynarmic {
    pub fn new(arch: Arch) -> Result<Dynarmic, Error> {
        Dynarmic::with_code_cache(arch, DEFAULT_CODE_CACHE)
    }

    /// `code_cache` in bytes; 0 keeps the dynarmic default. Fault PCs are accurate to the block
    /// (precise faults cost 7-27% on the backend benchmark kernels; see PLAN.md).
    pub fn with_code_cache(arch: Arch, code_cache: u64) -> Result<Dynarmic, Error> {
        Dynarmic::with_options(arch, code_cache, false)
    }

    /// `precise_faults`: a memory fault stops at the faulting instruction and reports its PC.
    /// Without it the PC is only accurate to the translated block.
    pub fn with_options(arch: Arch, code_cache: u64, precise_faults: bool) -> Result<Dynarmic, Error> {
        let mut memory = Box::new(Memory::new(arch.address_bits()));
        // dynarmic's Exception::NoExecuteFault, as declared in its A64 and A32 config headers.
        let no_execute = if arch == Arch::Arm64 { 9 } else { 12 };
        let mut events = Box::new(Events { stop: None, memory: &*memory, no_execute });
        let raw = new_jit(arch, &mut memory, &mut events, code_cache, precise_faults)?;
        let interrupt = crate::Interrupt::default();
        set_halt_target(&interrupt, raw);
        let mut cpu = Dynarmic { arch, cpu: Handle(raw), memory, executed: 0, events, interrupt, code_cache, precise_faults };
        if arch == Arch::Arm32 {
            // SAFETY: valid handle; user mode, ARM state.
            unsafe { sys::dyn_set_reg(cpu.cpu.0, 16, 0x10) };
        }
        cpu.map(TRAMPOLINE, machine::PAGE_SIZE, Perms::RX, "[trampoline]")?;
        cpu.memory.write(TRAMPOLINE, &trampoline_code(arch))?;
        Ok(cpu)
    }

    /// Replaces the JIT with one that has `precise_faults` set as given, keeping memory and
    /// registers. Translated code is discarded.
    pub fn set_precise_faults(&mut self, precise_faults: bool) -> Result<(), Error> {
        if precise_faults == self.precise_faults {
            return Ok(());
        }
        let context = crate::save_context(self);
        let raw = new_jit(self.arch, &mut self.memory, &mut self.events, self.code_cache, precise_faults)?;
        // The old JIT is freed only after nothing can halt it any more.
        self.interrupt.set_halt(None);
        self.cpu = Handle(raw);
        set_halt_target(&self.interrupt, raw);
        self.precise_faults = precise_faults;
        crate::load_context(self, &context);
        Ok(())
    }

    fn cpsr(&self) -> u64 {
        // SAFETY: valid handle; 16 is CPSR.
        unsafe { sys::dyn_reg(self.cpu.0, 16) }
    }

    fn index(&self, reg: Reg) -> u32 {
        match (self.arch, reg) {
            (Arch::Arm64, Reg::General(n)) => n as u32,
            (Arch::Arm64, Reg::Lr) => 30,
            (Arch::Arm64, Reg::Sp) => 31,
            (Arch::Arm64, Reg::Pc) => 32,
            (Arch::Arm64, Reg::Tls) => 34,
            (Arch::Arm64, Reg::Fp(n)) => 64 + n as u32,
            (Arch::Arm64, Reg::VecHigh(n)) => 96 + n as u32,
            (Arch::Arm64, Reg::Flags) => 33,
            (Arch::Arm64, Reg::FpControl) => 36,
            (Arch::Arm64, Reg::FpStatus) => 37,
            (Arch::Arm32, Reg::General(n)) => n as u32,
            (Arch::Arm32, Reg::Sp) => 13,
            (Arch::Arm32, Reg::Lr) => 14,
            (Arch::Arm32, Reg::Pc) => 15,
            (Arch::Arm32, Reg::Tls) => 18,
            (Arch::Arm32, Reg::Fp(n)) => 64 + n as u32,
            (Arch::Arm32, Reg::Flags) => 16,
            (Arch::Arm32, Reg::FpControl) => 17,
            // No such register: the shim reads 0 and ignores writes.
            (Arch::Arm32, Reg::VecHigh(_) | Reg::FpStatus) => 1000,
        }
    }
}

impl Cpu for Dynarmic {
    fn arch(&self) -> Arch {
        self.arch
    }

    fn reg(&self, reg: Reg) -> u64 {
        // SAFETY: valid handle.
        unsafe { sys::dyn_reg(self.cpu.0, self.index(reg)) }
    }

    fn set_reg(&mut self, reg: Reg, value: u64) {
        let index = self.index(reg);
        // SAFETY: valid handle.
        unsafe { sys::dyn_set_reg(self.cpu.0, index, value) }
    }

    fn memory(&self) -> &Memory {
        &self.memory
    }

    fn memory_mut(&mut self) -> &mut Memory {
        &mut self.memory
    }

    // dynarmic reads guest memory through the page table only, so permissions are bookkeeping here
    // (the same as unidbg's dynarmic backend).
    fn map(&mut self, address: u64, length: u64, perms: Perms, label: &str) -> Result<(), Error> {
        self.memory.map(address, length, perms, label)?;
        Ok(())
    }

    fn unmap(&mut self, address: u64, length: u64) -> Result<(), Error> {
        self.memory.unmap(address, length)?;
        self.invalidate(address, length);
        Ok(())
    }

    fn protect(&mut self, address: u64, length: u64, perms: Perms) -> Result<(), Error> {
        self.memory.protect(address, length, perms)?;
        // Translations depend on the execute bit.
        self.invalidate(address, length);
        Ok(())
    }

    fn set_pc(&mut self, pc: u64) {
        match self.arch {
            Arch::Arm64 => self.set_reg(Reg::Pc, pc),
            Arch::Arm32 => {
                // Keep the mode and flags; the T bit follows the low address bit.
                let cpsr = self.cpsr();
                let cpsr = if pc & 1 != 0 { cpsr | 0x20 } else { cpsr & !0x20 };
                // SAFETY: valid handle; 16 is CPSR.
                unsafe { sys::dyn_set_reg(self.cpu.0, 16, cpsr) };
                self.set_reg(Reg::Pc, pc & !1);
            }
        }
    }

    fn pc(&self) -> u64 {
        match self.arch {
            Arch::Arm64 => self.reg(Reg::Pc),
            Arch::Arm32 => self.reg(Reg::Pc) | ((self.cpsr() >> 5) & 1),
        }
    }

    fn resume(&mut self, budget: u64) -> Result<Stop, Error> {
        self.events.stop = None;
        self.executed = 0;
        // With cycle counting on, dynarmic checks for a halt only when a slice of cycles runs
        // out, not between linked blocks, so a guest loop would never see an interrupt. Running in
        // slices bounds the interrupt latency to one slice.
        let mut remaining = budget;
        loop {
            let slice = remaining.min(INTERRUPT_SLICE);
            let mut executed = 0u64;
            // SAFETY: valid handle; hooks point at self.events which is boxed and alive.
            let reason = unsafe { sys::dyn_run(self.cpu.0, slice, &mut executed) };
            self.executed = self.executed.saturating_add(executed);
            remaining = remaining.saturating_sub(executed);
            if reason & sys::DYN_HALT_ERROR != 0 {
                return Err(Error::Backend("dynarmic threw during execution".into()));
            }
            if let Some(stop) = self.events.stop.take() {
                return Ok(stop);
            }
            if reason & sys::DYN_HALT_EXTERNAL != 0 || self.interrupt.is_requested() {
                return Ok(Stop::Halted);
            }
            if remaining == 0 || executed == 0 {
                return Ok(Stop::Budget);
            }
        }
    }

    fn executed(&self) -> u64 {
        self.executed
    }

    fn step(&mut self) -> Result<Stop, Error> {
        self.events.stop = None;
        // SAFETY: valid handle; hooks point at self.events which is boxed and alive.
        let reason = unsafe { sys::dyn_step(self.cpu.0) };
        self.executed = 1;
        if reason & sys::DYN_HALT_ERROR != 0 {
            return Err(Error::Backend("dynarmic threw during execution".into()));
        }
        Ok(self.events.stop.take().unwrap_or(Stop::Budget))
    }

    fn interrupt(&self) -> crate::Interrupt {
        self.interrupt.clone()
    }

    fn restore(&mut self, from: &dyn Cpu) -> Result<(), Error> {
        crate::restore_state(self, from)
    }

    fn fork(&self) -> Result<Box<dyn Cpu>, Error> {
        let mut copy = Dynarmic::with_options(self.arch, self.code_cache, self.precise_faults)?;
        crate::copy_state(self, &mut copy)?;
        Ok(Box::new(copy))
    }

    fn require_precise_faults(&mut self) -> Result<(), Error> {
        self.set_precise_faults(true)
    }

    fn precise_faults(&self) -> bool {
        self.precise_faults
    }

    fn clear_exclusive(&mut self) {
        // SAFETY: valid handle.
        unsafe { sys::dyn_clear_exclusive(self.cpu.0) }
    }

    fn invalidate(&mut self, address: u64, length: u64) {
        // SAFETY: valid handle.
        unsafe { sys::dyn_invalidate(self.cpu.0, address, length) }
    }
}
