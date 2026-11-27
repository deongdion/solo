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
const A64_TPIDRRO_EL0: i32 = 263;
const A32_R0: i32 = 66;
const A32_SP: i32 = 12;
const A32_LR: i32 = 10;
const A32_PC: i32 = 11;
const A32_C13_C0_3: i32 = 113;

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
        let mut cpu = Unicorn { arch, uc, memory: Memory::new(arch.address_bits()) };
        cpu.map(TRAMPOLINE, machine::PAGE_SIZE, Perms::RX)?;
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
            (Arch::Arm64, Reg::Tls) => A64_TPIDRRO_EL0,
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

    fn memory(&mut self) -> &mut Memory {
        &mut self.memory
    }

    fn map(&mut self, address: u64, length: u64, perms: Perms) -> Result<(), Error> {
        let host = self.memory.map(address, length, perms)?.host_ptr();
        // SAFETY: the region lives in self.memory, which outlives self.uc (field order).
        unsafe { self.uc.mem_map_ptr(address, length, prot(perms), host.cast()) }.map_err(uc_error)
    }

    fn run(&mut self, pc: u64, budget: u64) -> Result<Stop, Error> {
        self.uc.get_data_mut().stop = None;
        // count = 0 means unlimited; any other value makes unicorn instrument every instruction.
        let count = if budget == u64::MAX { 0 } else { budget as usize };
        let result = self.uc.emu_start(pc, 0, 0, count);
        if let Some(stop) = self.uc.get_data_mut().stop.take() {
            return Ok(stop);
        }
        result.map_err(uc_error)?;
        Ok(Stop::Budget)
    }

    fn invalidate(&mut self, address: u64, length: u64) {
        let _ = self.uc.ctl_remove_cache(address, address + length);
    }
}
