//! unicorn backend for local parity runs. GPL-2.0: never part of a distributed build.

use machine::{Memory, Perms};
use unicorn_engine::unicorn_const::{Arch as UcArch, HookType, MemType, Mode, Prot};

use crate::{Arch, Cpu, Error, RETURN_SVC, Reg, Stop, TRAMPOLINE, trampoline_code};

// Register ids from unicorn's arm64.h and arm.h.
const A64_X0: i32 = 199;
const A64_X29: i32 = 1;
const A64_X30: i32 = 2;
const A64_SP: i32 = 4;
const A64_PC: i32 = 260;
const A64_TPIDR_EL0: i32 = 262;
const A32_R0: i32 = 66;
const A32_SP: i32 = 12;
const A32_LR: i32 = 10;
const A32_PC: i32 = 11;
const A32_C13_C0_3: i32 = 113;
const A32_CPSR: i32 = 3;

#[derive(Default)]
struct Events {
    stop: Option<Stop>,
}

/// unicorn on top of the same `Memory` pages, mapped with `uc_mem_map_ptr`.
pub struct Unicorn {
    arch: Arch,
    // Declared before `memory`: unicorn holds pointers into its pages and is dropped first.
    uc: unicorn_engine::Unicorn<'static, Events>,
    memory: Memory,
    // unicorn has no "continue": every run starts at an explicit address.
    next: u64,
    executed: u64,
}

impl Unicorn {
    pub fn new(arch: Arch) -> Result<Unicorn, Error> {
        let (uc_arch, mode) = match arch {
            Arch::Arm64 => (UcArch::ARM64, Mode::ARM),
            Arch::Arm32 => (UcArch::ARM, Mode::ARM),
        };
        let mut uc = unicorn_engine::Unicorn::new_with_data(uc_arch, mode, Events::default()).map_err(uc_error)?;
        uc.add_intr_hook(move |uc, _intno| {
            let pc = uc.pc_read().unwrap_or(0);
            let mut word = [0u8; 4];
            let imm = match uc.mem_read(pc.wrapping_sub(4), &mut word) {
                Ok(()) => {
                    let insn = u32::from_le_bytes(word);
                    match arch {
                        Arch::Arm64 => (insn >> 5) & 0xffff,
                        Arch::Arm32 => insn & 0x00ff_ffff,
                    }
                }
                Err(_) => u32::MAX,
            };
            uc.get_data_mut().stop = Some(if imm == RETURN_SVC { Stop::Returned } else { Stop::Svc(imm) });
            let _ = uc.emu_stop();
        })
        .map_err(uc_error)?;
        uc.add_mem_hook(HookType::MEM_UNMAPPED, 1, 0, |uc, kind, address, _size, _value| {
            let write = matches!(kind, MemType::WRITE_UNMAPPED);
            uc.get_data_mut().stop.get_or_insert(Stop::Fault { address, write });
            false
        })
        .map_err(uc_error)?;
        let mut cpu = Unicorn { arch, uc, memory: Memory::new(arch.address_bits()), next: 0, executed: 0 };
        cpu.map(TRAMPOLINE, machine::PAGE_SIZE, Perms::RX, "[trampoline]")?;
        cpu.memory.write(TRAMPOLINE, &trampoline_code(arch))?;
        Ok(cpu)
    }

    fn id(&self, reg: Reg) -> i32 {
        match (self.arch, reg) {
            (Arch::Arm64, Reg::General(n)) if n < 29 => A64_X0 + n as i32,
            (Arch::Arm64, Reg::General(29)) => A64_X29,
            (Arch::Arm64, Reg::General(_)) | (Arch::Arm64, Reg::Lr) => A64_X30,
            (Arch::Arm64, Reg::Sp) => A64_SP,
            (Arch::Arm64, Reg::Pc) => A64_PC,
            (Arch::Arm64, Reg::Tls) => A64_TPIDR_EL0,
            (Arch::Arm32, Reg::General(n)) => A32_R0 + n as i32,
            (Arch::Arm32, Reg::Sp) => A32_SP,
            (Arch::Arm32, Reg::Lr) => A32_LR,
            (Arch::Arm32, Reg::Pc) => A32_PC,
            (Arch::Arm32, Reg::Tls) => A32_C13_C0_3,
        }
    }
}

fn uc_error(e: unicorn_engine::unicorn_const::uc_error) -> Error {
    Error::Backend(format!("unicorn: {e:?}"))
}

fn prot(perms: Perms) -> Prot {
    Prot(perms.bits() as i32)
}

impl Cpu for Unicorn {
    fn arch(&self) -> Arch {
        self.arch
    }

    fn reg(&self, reg: Reg) -> u64 {
        self.uc.reg_read(self.id(reg)).unwrap_or(0)
    }

    fn set_reg(&mut self, reg: Reg, value: u64) {
        let id = self.id(reg);
        let _ = self.uc.reg_write(id, value);
    }

    fn memory(&self) -> &Memory {
        &self.memory
    }

    fn memory_mut(&mut self) -> &mut Memory {
        &mut self.memory
    }

    fn map(&mut self, address: u64, length: u64, perms: Perms, label: &str) -> Result<(), Error> {
        let host = self.memory.map(address, length, perms, label)?;
        // SAFETY: the allocation lives in self.memory, which outlives self.uc (field order), and is
        // removed from unicorn before it is released.
        unsafe { self.uc.mem_map_ptr(address, length, prot(perms), host.cast()) }.map_err(uc_error)
    }

    fn unmap(&mut self, address: u64, length: u64) -> Result<(), Error> {
        let pieces: Vec<(u64, u64)> = self
            .memory
            .mappings()
            .filter(|m| m.end > address && m.start < address + length)
            .map(|m| (m.start.max(address), m.end.min(address + length)))
            .collect();
        for (start, end) in pieces {
            self.uc.mem_unmap(start, end - start).map_err(uc_error)?;
        }
        self.memory.unmap(address, length)?;
        Ok(())
    }

    fn protect(&mut self, address: u64, length: u64, perms: Perms) -> Result<(), Error> {
        self.memory.protect(address, length, perms)?;
        self.uc.mem_protect(address, length, prot(perms)).map_err(uc_error)
    }

    fn set_pc(&mut self, pc: u64) {
        self.next = pc;
    }

    fn pc(&self) -> u64 {
        match self.arch {
            Arch::Arm64 => self.uc.pc_read().unwrap_or(0),
            Arch::Arm32 => {
                let cpsr = self.uc.reg_read(A32_CPSR).unwrap_or(0);
                self.uc.pc_read().unwrap_or(0) | ((cpsr >> 5) & 1)
            }
        }
    }

    fn resume(&mut self, budget: u64) -> Result<Stop, Error> {
        self.uc.get_data_mut().stop = None;
        self.executed = 0;
        // count = 0 means unlimited; any other value makes unicorn instrument every instruction.
        let count = if budget == u64::MAX { 0 } else { budget as usize };
        let begin = self.next;
        let result = self.uc.emu_start(begin, 0, 0, count);
        self.next = self.pc();
        if let Some(stop) = self.uc.get_data_mut().stop.take() {
            return Ok(stop);
        }
        result.map_err(uc_error)?;
        self.executed = budget;
        Ok(Stop::Budget)
    }

    fn executed(&self) -> u64 {
        self.executed
    }

    fn invalidate(&mut self, address: u64, length: u64) {
        let _ = self.uc.ctl_remove_cache(address, address + length);
    }
}
