//! Signals: rt_sigaction, per-thread masks, delivery through a Linux signal frame, rt_sigreturn.
//!
//! A signal is delivered by building the kernel's `rt_sigframe` (siginfo, then ucontext with the
//! interrupted registers) below the thread's stack and entering the handler with
//! `(signo, &info, &ucontext)`; returning from the handler runs `rt_sigreturn` (through
//! `sa_restorer`, which bionic always sets, or the runtime's own stub), which loads the registers
//! back from the ucontext, including any the handler changed.

use std::collections::{BTreeMap, HashMap};

use backend::{Arch, Cpu, Reg};

pub const SIGTRAP: u32 = 5;
pub const SIGBUS: u32 = 7;
pub const SIGILL: u32 = 4;
pub const SIGSEGV: u32 = 11;
pub const SIGKILL: u32 = 9;
pub const SIGSTOP: u32 = 19;

const SIG_DFL: u64 = 0;
const SIG_IGN: u64 = 1;
const SA_SIGINFO: u64 = 0x4;
const SA_ONSTACK: u64 = 0x0800_0000;
const SA_RESTORER: u64 = 0x0400_0000;
const SA_NODEFER: u64 = 0x4000_0000;
const SA_RESETHAND: u64 = 0x8000_0000;
const SS_ONSTACK: u32 = 1;
const SS_DISABLE: u32 = 2;

/// si_code values.
pub const SI_TKILL: i32 = -6;
pub const SI_USER: i32 = 0;
pub const SEGV_MAPERR: i32 = 1;
pub const SEGV_ACCERR: i32 = 2;
pub const ILL_ILLOPC: i32 = 1;
pub const TRAP_BRKPT: i32 = 1;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Action {
    pub handler: u64,
    pub flags: u64,
    pub restorer: u64,
    pub mask: u64,
}

/// What `deliver` did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Delivery {
    /// The thread now runs the handler.
    Handler,
    /// SIG_IGN, or ignored by default.
    Ignored,
    /// Blocked by the thread's mask; kept pending.
    Pending,
    /// The default action terminates the process.
    Terminate,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct AltStack {
    pub sp: u64,
    pub size: u64,
    pub flags: u32,
}

#[derive(Debug, Clone, Default)]
pub struct Signals {
    actions: HashMap<u32, Action>,
    /// Blocked signals and pending signals per thread (bit `signo - 1`).
    masks: BTreeMap<u32, u64>,
    pending: BTreeMap<u32, u64>,
    alt: BTreeMap<u32, AltStack>,
    /// Where `rt_sigreturn` is called from when the handler has no `sa_restorer`.
    pub sigreturn_stub: u64,
    pub delivered: u64,
}

fn bit(signo: u32) -> u64 {
    1u64 << (signo - 1)
}

/// Signals whose default action is to do nothing.
fn ignored_by_default(signo: u32) -> bool {
    matches!(signo, 17 | 18 | 23 | 28) // SIGCHLD, SIGCONT, SIGURG, SIGWINCH
}

/// A CPSR as user code sees it: user mode, interrupts enabled.
fn user_cpsr(cpsr: u64) -> u64 {
    (cpsr & !0x1df) | 0x10
}

fn word(arch: Arch) -> u64 {
    if arch == Arch::Arm64 { 8 } else { 4 }
}

impl Signals {
    pub fn new(sigreturn_stub: u64) -> Signals {
        Signals { sigreturn_stub, ..Default::default() }
    }

    pub fn action(&self, signo: u32) -> Action {
        self.actions.get(&signo).copied().unwrap_or_default()
    }

    /// The kernel `struct sigaction` (handler, flags, restorer, mask) as the guest passes it.
    pub fn decode_action(arch: Arch, bytes: &[u8]) -> Action {
        let w = word(arch) as usize;
        let field = |i: usize| -> u64 {
            let mut v = [0u8; 8];
            v[..w].copy_from_slice(&bytes[i * w..i * w + w]);
            u64::from_le_bytes(v)
        };
        let mut mask = [0u8; 8];
        mask.copy_from_slice(&bytes[3 * w..3 * w + 8]);
        Action { handler: field(0), flags: field(1), restorer: field(2), mask: u64::from_le_bytes(mask) }
    }

    pub fn encode_action(arch: Arch, action: &Action) -> Vec<u8> {
        let w = word(arch) as usize;
        let mut out = Vec::with_capacity(3 * w + 8);
        for v in [action.handler, action.flags, action.restorer] {
            out.extend_from_slice(&v.to_le_bytes()[..w]);
        }
        out.extend_from_slice(&action.mask.to_le_bytes());
        out
    }

    /// The ARM `struct old_sigaction` of sigaction(2): handler, mask (32 bits), flags, restorer.
    pub fn decode_old_action(bytes: &[u8]) -> Action {
        let field = |i: usize| u32::from_le_bytes(bytes[i * 4..i * 4 + 4].try_into().expect("4 bytes")) as u64;
        Action { handler: field(0), mask: field(1), flags: field(2), restorer: field(3) }
    }

    pub fn encode_old_action(action: &Action) -> Vec<u8> {
        [action.handler, action.mask & 0xffff_ffff, action.flags, action.restorer].iter().flat_map(|v| (*v as u32).to_le_bytes()).collect()
    }

    /// Size of the kernel `struct sigaction`.
    pub fn action_size(arch: Arch) -> usize {
        3 * word(arch) as usize + 8
    }

    /// Fails for SIGKILL and SIGSTOP, as the kernel does.
    pub fn set_action(&mut self, signo: u32, action: Action) -> bool {
        if signo == SIGKILL || signo == SIGSTOP || !(1..=64).contains(&signo) {
            return false;
        }
        self.actions.insert(signo, action);
        true
    }

    pub fn mask(&self, tid: u32) -> u64 {
        self.masks.get(&tid).copied().unwrap_or(0)
    }

    pub fn set_mask(&mut self, tid: u32, mask: u64) {
        // SIGKILL and SIGSTOP cannot be blocked.
        self.masks.insert(tid, mask & !(bit(SIGKILL) | bit(SIGSTOP)));
    }

    pub fn pending(&self, tid: u32) -> u64 {
        self.pending.get(&tid).copied().unwrap_or(0)
    }

    pub fn alt_stack(&self, tid: u32) -> AltStack {
        self.alt.get(&tid).copied().unwrap_or(AltStack { sp: 0, size: 0, flags: SS_DISABLE })
    }

    pub fn set_alt_stack(&mut self, tid: u32, alt: AltStack) {
        self.alt.insert(tid, alt);
    }

    /// Whether a fault signal would reach a handler (installed and not blocked).
    pub fn catches(&self, tid: u32, signo: u32) -> bool {
        let a = self.action(signo);
        a.handler != SIG_DFL && a.handler != SIG_IGN && self.mask(tid) & bit(signo) == 0
    }

    pub fn raise_pending(&mut self, tid: u32, signo: u32) {
        *self.pending.entry(tid).or_default() |= bit(signo);
    }

    /// The lowest pending signal of `tid` that its mask lets through, removed from pending.
    pub fn take_deliverable(&mut self, tid: u32) -> Option<u32> {
        let ready = self.pending(tid) & !self.mask(tid);
        if ready == 0 {
            return None;
        }
        let signo = ready.trailing_zeros() + 1;
        if let Some(p) = self.pending.get_mut(&tid) {
            *p &= !bit(signo);
        }
        Some(signo)
    }

    /// Delivers `signo` to the running thread `tid` (its registers are live in `cpu`).
    pub fn deliver(&mut self, cpu: &mut dyn Cpu, tid: u32, signo: u32, code: i32, address: u64, sender: u32) -> Result<Delivery, machine::Error> {
        let action = self.action(signo);
        if action.handler == SIG_IGN || (action.handler == SIG_DFL && ignored_by_default(signo)) {
            return Ok(Delivery::Ignored);
        }
        if action.handler == SIG_DFL {
            return Ok(Delivery::Terminate);
        }
        if self.mask(tid) & bit(signo) != 0 {
            self.raise_pending(tid, signo);
            return Ok(Delivery::Pending);
        }
        let old_mask = self.mask(tid);
        let alt = self.alt_stack(tid);
        let sp = cpu.reg(Reg::Sp);
        let on_alt = alt.flags & SS_DISABLE == 0 && sp > alt.sp && sp <= alt.sp + alt.size;
        let top = if action.flags & SA_ONSTACK != 0 && alt.flags & SS_DISABLE == 0 && !on_alt { alt.sp + alt.size } else { sp };
        // arm64 always builds an rt frame; arm32 builds the older frame (ucontext only, left by
        // sigreturn) for handlers without SA_SIGINFO, as Linux does, and bionic's restorer for
        // them calls sigreturn.
        let rt = cpu.arch() == Arch::Arm64 || action.flags & SA_SIGINFO != 0;
        let frame = match cpu.arch() {
            Arch::Arm64 => self.frame64(cpu, top, signo, code, address, sender, old_mask, alt)?,
            Arch::Arm32 => self.frame32(cpu, top, signo, code, address, sender, old_mask, alt, rt)?,
        };
        // Without sa_restorer the stub after the rt_sigreturn one calls sigreturn (arm32 only).
        let stub = if rt { self.sigreturn_stub } else { self.sigreturn_stub + 8 };
        let restorer = if action.flags & SA_RESTORER != 0 && action.restorer != 0 { action.restorer } else { stub };
        let (info, context) = if rt { (frame, frame + 128) } else { (0, frame) };
        if cpu.arch() == Arch::Arm32 {
            // Handlers start in ARM or Thumb state by their address, in user mode with no IT
            // state. CPSR first: changing the mode on a backend that models banked registers
            // (unicorn is in SVC mode during a syscall) swaps SP and LR.
            cpu.set_reg(Reg::Flags, user_cpsr(cpu.reg(Reg::Flags)) & !(0x0600_fc00 | 0x20));
        }
        cpu.set_reg(Reg::General(0), signo as u64);
        cpu.set_reg(Reg::General(1), info);
        cpu.set_reg(Reg::General(2), context);
        cpu.set_reg(Reg::Sp, frame);
        cpu.set_reg(Reg::Lr, restorer);
        cpu.set_pc(action.handler);
        let mut mask = old_mask | action.mask;
        if action.flags & SA_NODEFER == 0 {
            mask |= bit(signo);
        }
        self.set_mask(tid, mask);
        if action.flags & SA_RESETHAND != 0 {
            self.actions.insert(signo, Action::default());
        }
        self.delivered += 1;
        Ok(Delivery::Handler)
    }

    fn siginfo(signo: u32, code: i32, address: u64, sender: u32, arch: Arch) -> [u8; 128] {
        let mut info = [0u8; 128];
        info[0..4].copy_from_slice(&(signo as i32).to_le_bytes());
        info[8..12].copy_from_slice(&code.to_le_bytes());
        // The union starts after three ints, aligned to a pointer.
        let union = if arch == Arch::Arm64 { 16 } else { 12 };
        if matches!(signo, SIGSEGV | SIGBUS | SIGILL | SIGTRAP) && code > 0 {
            let w = word(arch) as usize;
            info[union..union + w].copy_from_slice(&address.to_le_bytes()[..w]);
        } else {
            info[union..union + 4].copy_from_slice(&sender.to_le_bytes());
        }
        info
    }

    fn stack_t(alt: AltStack, sp: u64, arch: Arch) -> Vec<u8> {
        let flags = if alt.flags & SS_DISABLE != 0 {
            SS_DISABLE
        } else if sp > alt.sp && sp <= alt.sp + alt.size {
            SS_ONSTACK
        } else {
            0
        };
        let mut out = Vec::new();
        match arch {
            Arch::Arm64 => {
                out.extend_from_slice(&alt.sp.to_le_bytes());
                out.extend_from_slice(&flags.to_le_bytes());
                out.extend_from_slice(&[0; 4]);
                out.extend_from_slice(&alt.size.to_le_bytes());
            }
            Arch::Arm32 => {
                out.extend_from_slice(&(alt.sp as u32).to_le_bytes());
                out.extend_from_slice(&flags.to_le_bytes());
                out.extend_from_slice(&(alt.size as u32).to_le_bytes());
            }
        }
        out
    }

    // arm64 rt_sigframe: siginfo (128), ucontext: uc_flags 0, uc_link 8, uc_stack 16 (24 bytes),
    // uc_sigmask 40 (8, padded to 128 bytes), uc_mcontext 176: fault_address, regs[31], sp, pc,
    // pstate, then __reserved (4096, 16-aligned, at 464) holding an fpsimd_context record.
    const UC64_MCONTEXT: u64 = 176;
    const UC64_RESERVED: u64 = 464;
    const UC64_SIZE: u64 = 464 + 4096;

    #[allow(clippy::too_many_arguments)]
    fn frame64(&self, cpu: &mut dyn Cpu, top: u64, signo: u32, code: i32, address: u64, sender: u32, mask: u64, alt: AltStack) -> Result<u64, machine::Error> {
        let frame = (top - 128 - Self::UC64_SIZE) & !15;
        let uc = frame + 128;
        let m = cpu.memory_mut();
        m.write(frame, &Self::siginfo(signo, code, address, sender, Arch::Arm64))?;
        m.write(uc, &[0u8; Self::UC64_RESERVED as usize + 16 + 528 + 8])?;
        let sp = cpu.reg(Reg::Sp);
        cpu.memory_mut().write(uc + 16, &Self::stack_t(alt, sp, Arch::Arm64))?;
        cpu.memory_mut().write_u64(uc + 40, mask)?;
        let mc = uc + Self::UC64_MCONTEXT;
        cpu.memory_mut().write_u64(mc, address)?;
        for i in 0..30u8 {
            let v = cpu.reg(Reg::General(i));
            cpu.memory_mut().write_u64(mc + 8 + i as u64 * 8, v)?;
        }
        let (lr, pc, pstate) = (cpu.reg(Reg::Lr), cpu.pc(), cpu.reg(Reg::Flags));
        let m = cpu.memory_mut();
        m.write_u64(mc + 8 + 30 * 8, lr)?;
        m.write_u64(mc + 256, sp)?;
        m.write_u64(mc + 264, pc)?;
        m.write_u64(mc + 272, pstate)?;
        // fpsimd_context: magic, size, fpsr, fpcr, vregs[32].
        let fp = uc + Self::UC64_RESERVED;
        let (fpsr, fpcr) = (cpu.reg(Reg::FpStatus), cpu.reg(Reg::FpControl));
        let m = cpu.memory_mut();
        m.write_u32(fp, 0x4650_8001)?;
        m.write_u32(fp + 4, 528)?;
        m.write_u32(fp + 8, fpsr as u32)?;
        m.write_u32(fp + 12, fpcr as u32)?;
        for i in 0..32u8 {
            let (low, high) = (cpu.reg(Reg::Fp(i)), cpu.reg(Reg::VecHigh(i)));
            let m = cpu.memory_mut();
            m.write_u64(fp + 16 + i as u64 * 16, low)?;
            m.write_u64(fp + 24 + i as u64 * 16, high)?;
        }
        // The terminating null record follows (already zero).
        Ok(frame)
    }

    // arm32 rt_sigframe: siginfo (128), ucontext: uc_flags 0, uc_link 4, uc_stack 8 (12),
    // uc_mcontext 20 (trap_no, error_code, oldmask, r0-r10, fp, ip, sp, lr, pc, cpsr,
    // fault_address), uc_sigmask 104 (128 bytes), uc_regspace 232 (512) holding the VFP frame.
    const UC32_MCONTEXT: u64 = 20;
    const UC32_SIGMASK: u64 = 104;
    const UC32_REGSPACE: u64 = 232;
    const UC32_SIZE: u64 = 232 + 512;

    #[allow(clippy::too_many_arguments)]
    fn frame32(&self, cpu: &mut dyn Cpu, top: u64, signo: u32, code: i32, address: u64, sender: u32, mask: u64, alt: AltStack, rt: bool) -> Result<u64, machine::Error> {
        let info_size = if rt { 128 } else { 0 };
        let frame = (top - info_size - Self::UC32_SIZE) & !7;
        let uc = frame + info_size;
        if rt {
            cpu.memory_mut().write(frame, &Self::siginfo(signo, code, address, sender, Arch::Arm32))?;
        }
        cpu.memory_mut().write(uc, &vec![0u8; Self::UC32_SIZE as usize])?;
        let sp = cpu.reg(Reg::Sp);
        cpu.memory_mut().write(uc + 8, &Self::stack_t(alt, sp, Arch::Arm32))?;
        let mc = uc + Self::UC32_MCONTEXT;
        let pc = cpu.pc();
        let mut words: Vec<u32> = vec![0, 0, mask as u32];
        for i in 0..13u8 {
            words.push(cpu.reg(Reg::General(i)) as u32);
        }
        // CPSR.T follows the PC's low bit.
        let cpsr = (user_cpsr(cpu.reg(Reg::Flags)) as u32 & !0x20) | ((pc as u32 & 1) << 5);
        words.extend([sp as u32, cpu.reg(Reg::Lr) as u32, pc as u32 & !1, cpsr, address as u32]);
        for (i, w) in words.iter().enumerate() {
            cpu.memory_mut().write_u32(mc + i as u64 * 4, *w)?;
        }
        cpu.memory_mut().write_u64(uc + Self::UC32_SIGMASK, mask)?;
        // vfp_sigframe: magic, size, fpregs[32], fpscr.
        let vfp = uc + Self::UC32_REGSPACE;
        cpu.memory_mut().write_u32(vfp, 0x5646_5001)?;
        cpu.memory_mut().write_u32(vfp + 4, 280)?;
        for i in 0..32u8 {
            let v = cpu.reg(Reg::Fp(i));
            cpu.memory_mut().write_u64(vfp + 8 + i as u64 * 8, v)?;
        }
        let fpscr = cpu.reg(Reg::FpControl);
        cpu.memory_mut().write_u32(vfp + 8 + 256, fpscr as u32)?;
        Ok(frame)
    }

    /// rt_sigreturn (`rt`) or sigreturn: loads the registers and mask saved in the frame at the
    /// stack pointer.
    pub fn sigreturn(&mut self, cpu: &mut dyn Cpu, tid: u32, rt: bool) -> Result<(), machine::Error> {
        let frame = cpu.reg(Reg::Sp);
        let uc = if rt { frame + 128 } else { frame };
        match cpu.arch() {
            Arch::Arm64 => {
                let mc = uc + Self::UC64_MCONTEXT;
                let mask = cpu.memory().read_u64(uc + 40)?;
                let mut regs = [0u64; 31];
                for (i, r) in regs.iter_mut().enumerate() {
                    *r = cpu.memory().read_u64(mc + 8 + i as u64 * 8)?;
                }
                let sp = cpu.memory().read_u64(mc + 256)?;
                let pc = cpu.memory().read_u64(mc + 264)?;
                let pstate = cpu.memory().read_u64(mc + 272)?;
                let fp = uc + Self::UC64_RESERVED;
                let fp_valid = cpu.memory().read_u32(fp)? == 0x4650_8001;
                if fp_valid {
                    let fpsr = cpu.memory().read_u32(fp + 8)?;
                    let fpcr = cpu.memory().read_u32(fp + 12)?;
                    for i in 0..32u8 {
                        let low = cpu.memory().read_u64(fp + 16 + i as u64 * 16)?;
                        let high = cpu.memory().read_u64(fp + 24 + i as u64 * 16)?;
                        cpu.set_reg(Reg::Fp(i), low);
                        cpu.set_reg(Reg::VecHigh(i), high);
                    }
                    cpu.set_reg(Reg::FpStatus, fpsr as u64);
                    cpu.set_reg(Reg::FpControl, fpcr as u64);
                }
                for (i, r) in regs.iter().enumerate().take(30) {
                    cpu.set_reg(Reg::General(i as u8), *r);
                }
                cpu.set_reg(Reg::Lr, regs[30]);
                cpu.set_reg(Reg::Sp, sp);
                // Only the condition flags are the guest's to change.
                cpu.set_reg(Reg::Flags, pstate & 0xf000_0000);
                cpu.set_pc(pc);
                self.set_mask(tid, mask);
            }
            Arch::Arm32 => {
                let mc = uc + Self::UC32_MCONTEXT;
                let mut words = [0u32; 21];
                for (i, w) in words.iter_mut().enumerate() {
                    *w = cpu.memory().read_u32(mc + i as u64 * 4)?;
                }
                let mask = cpu.memory().read_u64(uc + Self::UC32_SIGMASK)?;
                let vfp = uc + Self::UC32_REGSPACE;
                if cpu.memory().read_u32(vfp)? == 0x5646_5001 {
                    for i in 0..32u8 {
                        let v = cpu.memory().read_u64(vfp + 8 + i as u64 * 8)?;
                        cpu.set_reg(Reg::Fp(i), v);
                    }
                    let fpscr = cpu.memory().read_u32(vfp + 8 + 256)?;
                    cpu.set_reg(Reg::FpControl, fpscr as u64);
                }
                let (sp, lr, pc, cpsr) = (words[16], words[17], words[18], words[19]);
                // Flags (NZCVQ, GE) and Thumb state from the saved CPSR; user mode always. CPSR
                // first, as in deliver.
                let thumb = (cpsr >> 5) & 1;
                cpu.set_reg(Reg::Flags, ((cpsr & 0xf80f_0000) | 0x10) as u64);
                for i in 0..13u8 {
                    cpu.set_reg(Reg::General(i), words[3 + i as usize] as u64);
                }
                cpu.set_reg(Reg::Sp, sp as u64);
                cpu.set_reg(Reg::Lr, lr as u64);
                cpu.set_pc((pc & !1 | thumb) as u64);
                self.set_mask(tid, mask);
            }
        }
        Ok(())
    }

    pub fn forget_thread(&mut self, tid: u32) {
        self.masks.remove(&tid);
        self.pending.remove(&tid);
        self.alt.remove(&tid);
    }

    /// A new thread inherits the creator's mask.
    pub fn inherit(&mut self, parent: u32, child: u32) {
        let mask = self.mask(parent);
        self.masks.insert(child, mask);
    }
}
