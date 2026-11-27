//! unicorn backend for local parity runs. GPL-2.0: never part of a distributed build.

use machine::{Memory, Perms};
use unicorn_engine::unicorn_const::{Arch as UcArch, HookType, MemType, Mode, Prot};
use unicorn_engine::{RegisterARM, RegisterARM64};

use crate::{Arch, Cpu, Error, RETURN_SVC, Reg, Stop, TRAMPOLINE, trampoline_code};

// Register ids from the crate's generated enums, so they always match the linked unicorn.
const A64_X0: i32 = RegisterARM64::X0 as i32;
const A64_X29: i32 = RegisterARM64::X29 as i32;
const A64_X30: i32 = RegisterARM64::X30 as i32;
const A64_SP: i32 = RegisterARM64::SP as i32;
const A64_PC: i32 = RegisterARM64::PC as i32;
const A64_D0: i32 = RegisterARM64::D0 as i32;
const A64_TPIDR_EL0: i32 = RegisterARM64::TPIDR_EL0 as i32;
const A64_CPACR_EL1: i32 = RegisterARM64::CPACR_EL1 as i32;
const A32_R0: i32 = RegisterARM::R0 as i32;
const A32_SP: i32 = RegisterARM::SP as i32;
const A32_LR: i32 = RegisterARM::LR as i32;
const A32_PC: i32 = RegisterARM::PC as i32;
const A32_D0: i32 = RegisterARM::D0 as i32;
const A32_CPSR: i32 = RegisterARM::CPSR as i32;
const A32_FPSCR: i32 = RegisterARM::FPSCR as i32;
const A64_NZCV: i32 = RegisterARM64::NZCV as i32;
const A64_FPCR: i32 = RegisterARM64::FPCR as i32;
const A64_FPSR: i32 = RegisterARM64::FPSR as i32;
const A64_Q0: i32 = RegisterARM64::Q0 as i32;
/// Not a register: reads 0, writes are ignored.
const NONE: i32 = -1;
const A32_FPEXC: i32 = RegisterARM::FPEXC as i32;
const A32_C1_C0_2: i32 = RegisterARM::C1_C0_2 as i32;
const A32_C13_C0_3: i32 = RegisterARM::C13_C0_3 as i32;

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
    /// Stops a run with uc_emu_stop, which unicorn's own timeout thread also calls.
    interrupt: crate::Interrupt,
}

unsafe extern "C" {
    fn uc_emu_stop(uc: *mut std::ffi::c_void) -> i32;
}

/// The engine pointer, for stopping from another thread.
struct StopTarget(*mut std::ffi::c_void);
// SAFETY: only used to call uc_emu_stop, which only sets a request flag and kicks the vCPU, and
// only while the engine is alive (Drop clears it under the interrupt lock before closing).
unsafe impl Send for StopTarget {}

impl Drop for Unicorn {
    fn drop(&mut self) {
        self.interrupt.set_halt(None);
    }
}

// SAFETY: the unicorn handle is reference-counted internally (Rc), but every clone of it lives
// inside this value (the engine and its hooks), so the whole value moves between threads as one
// and is never used from two threads at once.
unsafe impl Send for Unicorn {}

impl Unicorn {
    pub fn new(arch: Arch) -> Result<Unicorn, Error> {
        let (uc_arch, mode) = match arch {
            Arch::Arm64 => (UcArch::ARM64, Mode::ARM),
            Arch::Arm32 => (UcArch::ARM, Mode::ARM),
        };
        let mut uc = unicorn_engine::Unicorn::new_with_data(uc_arch, mode, Events::default()).map_err(uc_error)?;
        uc.add_intr_hook(move |uc, _intno| {
            let pc = uc.pc_read().unwrap_or(0);
            // The immediate comes from the svc just executed: 4 bytes on A64 and ARM, 2 in Thumb.
            let thumb = arch == Arch::Arm32 && uc.reg_read(A32_CPSR).unwrap_or(0) & 0x20 != 0;
            let mut word = [0u8; 4];
            let imm = if thumb {
                match uc.mem_read(pc.wrapping_sub(2), &mut word[..2]) {
                    Ok(()) => u16::from_le_bytes([word[0], word[1]]) as u32 & 0xff,
                    Err(_) => u32::MAX,
                }
            } else {
                match uc.mem_read(pc.wrapping_sub(4), &mut word) {
                    Ok(()) => {
                        let insn = u32::from_le_bytes(word);
                        match arch {
                            Arch::Arm64 => (insn >> 5) & 0xffff,
                            Arch::Arm32 => insn & 0x00ff_ffff,
                        }
                    }
                    Err(_) => u32::MAX,
                }
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
        // Enable the FP/SIMD unit, as Android's kernel does for user processes.
        match arch {
            Arch::Arm64 => uc.reg_write(A64_CPACR_EL1, 3 << 20).map_err(uc_error)?,
            Arch::Arm32 => {
                let cpacr = uc.reg_read(A32_C1_C0_2).map_err(uc_error)?;
                uc.reg_write(A32_C1_C0_2, cpacr | (0xf << 20)).map_err(uc_error)?;
                uc.reg_write(A32_FPEXC, 0x4000_0000).map_err(uc_error)?;
            }
        }
        let interrupt = crate::Interrupt::default();
        let target = StopTarget(uc.get_handle() as *mut std::ffi::c_void);
        interrupt.set_halt(Some(Box::new(move || {
            let target = &target;
            // SAFETY: see StopTarget.
            unsafe { uc_emu_stop(target.0) };
        })));
        let mut cpu = Unicorn { arch, uc, memory: Memory::new(arch.address_bits()), next: 0, executed: 0, interrupt };
        cpu.map(TRAMPOLINE, machine::PAGE_SIZE, Perms::RX, "[trampoline]")?;
        cpu.memory.write(TRAMPOLINE, &trampoline_code(arch))?;
        Ok(cpu)
    }

    /// Where unicorn stopped; on A32 the low bit is set in Thumb state.
    fn engine_pc(&self) -> u64 {
        match self.arch {
            Arch::Arm64 => self.uc.pc_read().unwrap_or(0),
            Arch::Arm32 => {
                let cpsr = self.uc.reg_read(A32_CPSR).unwrap_or(0);
                self.uc.pc_read().unwrap_or(0) | ((cpsr >> 5) & 1)
            }
        }
    }

    fn id(&self, reg: Reg) -> i32 {
        match (self.arch, reg) {
            (Arch::Arm64, Reg::General(n)) if n < 29 => A64_X0 + n as i32,
            (Arch::Arm64, Reg::General(29)) => A64_X29,
            (Arch::Arm64, Reg::General(_)) | (Arch::Arm64, Reg::Lr) => A64_X30,
            (Arch::Arm64, Reg::Sp) => A64_SP,
            (Arch::Arm64, Reg::Pc) => A64_PC,
            (Arch::Arm64, Reg::Tls) => A64_TPIDR_EL0,
            (Arch::Arm64, Reg::Fp(n)) => A64_D0 + n as i32,
            (Arch::Arm64, Reg::Flags) => A64_NZCV,
            (Arch::Arm64, Reg::FpControl) => A64_FPCR,
            (Arch::Arm64, Reg::FpStatus) => A64_FPSR,
            // Read and written through Qn in reg and set_reg.
            (Arch::Arm64, Reg::VecHigh(n)) => A64_Q0 + n as i32,
            (Arch::Arm32, Reg::Flags) => A32_CPSR,
            (Arch::Arm32, Reg::FpControl) => A32_FPSCR,
            (Arch::Arm32, Reg::VecHigh(_) | Reg::FpStatus) => NONE,
            (Arch::Arm32, Reg::General(n)) => A32_R0 + n as i32,
            (Arch::Arm32, Reg::Sp) => A32_SP,
            (Arch::Arm32, Reg::Lr) => A32_LR,
            (Arch::Arm32, Reg::Pc) => A32_PC,
            (Arch::Arm32, Reg::Tls) => A32_C13_C0_3,
            (Arch::Arm32, Reg::Fp(n)) => A32_D0 + n as i32,
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
        let id = self.id(reg);
        match reg {
            _ if id == NONE => 0,
            Reg::VecHigh(_) => self.uc.reg_read_long(id).map_or(0, |q| u64::from_le_bytes(q[8..16].try_into().expect("16 bytes"))),
            _ => self.uc.reg_read(id).unwrap_or(0),
        }
    }

    fn set_reg(&mut self, reg: Reg, value: u64) {
        let id = self.id(reg);
        match reg {
            _ if id == NONE => {}
            Reg::VecHigh(_) => {
                if let Ok(q) = self.uc.reg_read_long(id) {
                    let mut bytes = [0u8; 16];
                    bytes[..8].copy_from_slice(&q[..8]);
                    bytes[8..].copy_from_slice(&value.to_le_bytes());
                    let _ = self.uc.reg_write_long(id, &bytes);
                }
            }
            _ => {
                let _ = self.uc.reg_write(id, value);
            }
        }
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
        self.next
    }

    fn resume(&mut self, budget: u64) -> Result<Stop, Error> {
        self.uc.get_data_mut().stop = None;
        self.executed = 0;
        // count = 0 means unlimited; any other value makes unicorn instrument every instruction.
        let count = if budget == u64::MAX { 0 } else { budget as usize };
        let begin = self.next;
        let result = self.uc.emu_start(begin, 0, 0, count);
        self.next = self.engine_pc();
        if let Some(stop) = self.uc.get_data_mut().stop.take() {
            return Ok(stop);
        }
        result.map_err(uc_error)?;
        if self.interrupt.is_requested() {
            return Ok(Stop::Halted);
        }
        self.executed = budget;
        Ok(Stop::Budget)
    }

    fn executed(&self) -> u64 {
        self.executed
    }

    fn interrupt(&self) -> crate::Interrupt {
        self.interrupt.clone()
    }

    fn restore(&mut self, from: &dyn Cpu) -> Result<(), Error> {
        crate::restore_state(self, from)
    }

    fn fork(&self) -> Result<Box<dyn Cpu>, Error> {
        let mut copy = Unicorn::new(self.arch)?;
        crate::copy_state(self, &mut copy)?;
        Ok(Box::new(copy))
    }

    fn invalidate(&mut self, address: u64, length: u64) {
        let _ = self.uc.ctl_remove_cache(address, address + length);
    }
}
