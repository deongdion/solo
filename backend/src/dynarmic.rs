use std::ffi::c_void;

use dynarmicsys as sys;
use machine::{Memory, Perms};

use crate::{Arch, Cpu, Error, RETURN_SVC, Reg, Stop, TRAMPOLINE, trampoline_code};

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
}

struct Handle(*mut sys::DynCpu);

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
        let hooks = sys::DynHooks {
            user: (&mut *events as *mut Events).cast(),
            on_svc: Some(on_svc),
            on_exception: Some(on_exception),
            on_fault: Some(on_fault),
            can_execute: Some(can_execute),
        };
        let is64 = (arch == Arch::Arm64) as i32;
        // SAFETY: the page table and hooks outlive the handle (field order).
        let raw = unsafe { sys::dyn_new(is64, memory.page_table(), arch.address_bits(), code_cache, if precise_faults { sys::DYN_PRECISE_FAULTS } else { 0 }, &hooks) };
        if raw.is_null() {
            return Err(Error::Backend("dyn_new failed".into()));
        }
        let mut cpu = Dynarmic { arch, cpu: Handle(raw), memory, executed: 0, events };
        if arch == Arch::Arm32 {
            // SAFETY: valid handle; user mode, ARM state.
            unsafe { sys::dyn_set_reg(cpu.cpu.0, 16, 0x10) };
        }
        cpu.map(TRAMPOLINE, machine::PAGE_SIZE, Perms::RX, "[trampoline]")?;
        cpu.memory.write(TRAMPOLINE, &trampoline_code(arch))?;
        Ok(cpu)
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
            (Arch::Arm32, Reg::General(n)) => n as u32,
            (Arch::Arm32, Reg::Sp) => 13,
            (Arch::Arm32, Reg::Lr) => 14,
            (Arch::Arm32, Reg::Pc) => 15,
            (Arch::Arm32, Reg::Tls) => 18,
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
        let mut executed = 0u64;
        // SAFETY: valid handle; hooks point at self.events which is boxed and alive.
        let reason = unsafe { sys::dyn_run(self.cpu.0, budget, &mut executed) };
        self.executed = executed;
        if reason & sys::DYN_HALT_ERROR != 0 {
            return Err(Error::Backend("dynarmic threw during execution".into()));
        }
        if let Some(stop) = self.events.stop.take() {
            return Ok(stop);
        }
        if reason & sys::DYN_HALT_EXTERNAL != 0 {
            return Ok(Stop::Halted);
        }
        Ok(Stop::Budget)
    }

    fn executed(&self) -> u64 {
        self.executed
    }

    fn invalidate(&mut self, address: u64, length: u64) {
        // SAFETY: valid handle.
        unsafe { sys::dyn_invalidate(self.cpu.0, address, length) }
    }
}
