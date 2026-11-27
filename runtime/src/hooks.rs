//! Hooks on guest functions, one API for what unidbg spreads over xHook, Dobby, HookZz and Whale.
//!
//! - Inline hook (`Emulator::hook`): the first instruction of the function is replaced by
//!   `svc #HOOK_SVC`. On a hit the enter callback runs. To run the original, an instruction that
//!   does not depend on its address runs from a trampoline (a copy followed by a jump back), as
//!   does an A64 adr/adrp, rewritten as a load of the address it computes; any other PC-relative
//!   one (branches, literal loads, and on arm32 anything outside a short list of prologue forms)
//!   is put back, executed alone (single step) and the hook put back again.
//! - Import hook (`Emulator::hook_import`): a module's GOT entries and absolute pointers for a
//!   symbol are pointed at a stub; running the original jumps to the address they held.
//!
//! Callbacks get the emulator (they may read and write registers and memory, or call guest
//! functions) and a `Call`. The enter callback returns `Flow::Continue` to run the original or
//! `Flow::Return(value)` to return to the caller at once. The leave callback runs when the
//! original returns, with the return value in `Call::ret`, which it may change.

use std::collections::BTreeMap;
use std::sync::Arc;

use backend::{Arch, Reg, Stop};
use machine::Perms;

use crate::{Emulator, Error, layout};

/// Supervisor call immediate of hook sites, import stubs and the return stub (fits Thumb's 8 bits).
pub const HOOK_SVC: u32 = 0xfe;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flow {
    /// Run the original function.
    Continue,
    /// Return this value to the caller without running the original.
    Return(u64),
}

/// The call a hook sees.
#[derive(Debug, Clone)]
pub struct Call {
    pub hook: HookId,
    /// The hooked function (Thumb bit set for Thumb code).
    pub function: u64,
    /// Return address.
    pub caller: u64,
    /// Return value; meaningful in the leave callback, which may change it.
    pub ret: u64,
    /// Stack pointer at entry, where stack arguments start.
    pub sp: u64,
}

impl Call {
    /// Integer argument `index` at entry: x0-x7 then the stack on arm64, r0-r3 then the stack on
    /// arm32. Valid in the enter callback (the registers change once the original runs).
    pub fn arg(&self, emu: &Emulator, index: usize) -> Result<u64, Error> {
        let (in_regs, word) = if emu.cpu.arch() == Arch::Arm64 { (8, 8) } else { (4, 4) };
        if index < in_regs {
            return Ok(emu.cpu.reg(Reg::General(index as u8)));
        }
        let at = self.sp + ((index - in_regs) as u64) * word;
        Ok(if word == 8 { emu.cpu.memory().read_u64(at)? } else { emu.cpu.memory().read_u32(at)? as u64 })
    }
}

pub type EnterFn = dyn Fn(&mut Emulator, &mut Call) -> Result<Flow, Error> + Send + Sync;
pub type LeaveFn = dyn Fn(&mut Emulator, &mut Call) -> Result<(), Error> + Send + Sync;

/// Callbacks of one hook.
#[derive(Clone, Default)]
pub struct Hook {
    pub enter: Option<Arc<EnterFn>>,
    pub leave: Option<Arc<LeaveFn>>,
}

impl Hook {
    pub fn on_enter(enter: impl Fn(&mut Emulator, &mut Call) -> Result<Flow, Error> + Send + Sync + 'static) -> Hook {
        Hook { enter: Some(Arc::new(enter)), leave: None }
    }

    pub fn on_leave(leave: impl Fn(&mut Emulator, &mut Call) -> Result<(), Error> + Send + Sync + 'static) -> Hook {
        Hook { enter: None, leave: Some(Arc::new(leave)) }
    }

    pub fn and_leave(mut self, leave: impl Fn(&mut Emulator, &mut Call) -> Result<(), Error> + Send + Sync + 'static) -> Hook {
        self.leave = Some(Arc::new(leave));
        self
    }

    /// Replaces the function: `f` gives the return value and the original never runs.
    pub fn replace(f: impl Fn(&mut Emulator, &mut Call) -> Result<u64, Error> + Send + Sync + 'static) -> Hook {
        Hook::on_enter(move |emu, call| Ok(Flow::Return(f(emu, call)?)))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct HookId(pub u64);

#[derive(Clone)]
enum Site {
    /// The function's own first instruction, replaced; `original` holds its bytes, `trampoline`
    /// runs it and jumps back when it does not depend on its address.
    Inline { original: Vec<u8>, thumb: bool, trampoline: Option<u64> },
    /// A stub the import slots point at; `target` is where they pointed before.
    Import { target: u64, slots: Vec<u64> },
}

#[derive(Clone)]
struct Entry {
    id: HookId,
    hook: Hook,
    site: Site,
}

/// A call whose leave callback is due when it returns.
#[derive(Clone)]
struct Pending {
    tid: u32,
    sp: u64,
    call: Call,
    leave: Arc<LeaveFn>,
}

#[derive(Clone, Default)]
pub struct Hooks {
    /// By the address of the `svc` (inline sites: the function without the Thumb bit; import
    /// hooks: their stub).
    sites: BTreeMap<u64, Entry>,
    pending: Vec<Pending>,
    next_id: u64,
    /// Stubs page mapped, and the next free stub.
    stubs_next: u64,
    pub hits: u64,
}

fn svc_bytes(arch: Arch, thumb: bool) -> Vec<u8> {
    match arch {
        Arch::Arm64 => (0xd400_0001u32 | (HOOK_SVC << 5)).to_le_bytes().to_vec(),
        Arch::Arm32 if thumb => (0xdf00u16 | HOOK_SVC as u16).to_le_bytes().to_vec(),
        Arch::Arm32 => (0xef00_0000u32 | HOOK_SVC).to_le_bytes().to_vec(),
    }
}

/// The return stub: the first stub of the stubs page.
const RETURN_STUB: u64 = layout::HOOK_STUBS;

/// The register and value of an A64 adr/adrp at `pc`.
pub(crate) fn a64_adr(insn: u32, pc: u64) -> Option<(u32, u64)> {
    if insn & 0x1f00_0000 != 0x1000_0000 {
        return None;
    }
    let imm = ((((insn >> 5) & 0x7_ffff) << 2) | ((insn >> 29) & 3)) as u64;
    // Sign-extend the 21-bit immediate.
    let imm = ((imm << 43) as i64 >> 43) as u64;
    let value = if insn & 0x8000_0000 != 0 { (pc & !0xfff).wrapping_add(imm << 12) } else { pc.wrapping_add(imm) };
    Some((insn & 0x1f, value))
}

/// Whether an A64 instruction computes nothing from its own address.
pub(crate) fn a64_movable(insn: u32) -> bool {
    let pc_relative = insn & 0x1f00_0000 == 0x1000_0000 // adr, adrp
        || insn & 0x7c00_0000 == 0x1400_0000 // b, bl
        || insn & 0xff00_0010 == 0x5400_0000 // b.cond
        || insn & 0x7e00_0000 == 0x3400_0000 // cbz, cbnz
        || insn & 0x7e00_0000 == 0x3600_0000 // tbz, tbnz
        || insn & 0x3b00_0000 == 0x1800_0000 // ldr (literal), ldrsw, prfm, simd literal
        || insn & 0xff00_0000 == 0xd800_0000;
    !pc_relative
}

/// The arm32 prologue forms that run anywhere: push/stmdb sp! without PC, sub sp, mov ip, sp
/// (ARM), and push without PC and sub sp (Thumb, 16 and 32 bits). Everything else is stepped.
pub(crate) fn a32_movable(bytes: &[u8], thumb: bool) -> Option<usize> {
    if thumb {
        let first = u16::from_le_bytes([bytes[0], bytes[1]]);
        if first & 0xfe00 == 0xb400 || first & 0xff80 == 0xb080 {
            return Some(2);
        }
        let second = u16::from_le_bytes([bytes[2], bytes[3]]);
        // push.w / stmdb sp!, {...} without PC or SP in the list.
        if first == 0xe92d && second & 0xa000 == 0 {
            return Some(4);
        }
        None
    } else {
        let insn = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
        let movable = (insn & 0xffff_0000 == 0xe92d_0000 && insn & 0xa000 == 0) // stmdb sp!, {...}
            || insn & 0xffff_f000 == 0xe24d_d000 // sub sp, sp, #imm
            || insn == 0xe1a0_c00d; // mov ip, sp
        movable.then_some(4)
    }
}

impl Emulator {
    /// Hooks the function at `function` (Thumb bit set for Thumb code on arm32).
    pub fn hook(&mut self, function: u64, hook: Hook) -> Result<HookId, Error> {
        let arch = self.cpu.arch();
        let thumb = arch == Arch::Arm32 && function & 1 != 0;
        let site = function & !1;
        if self.hooks.sites.contains_key(&site) {
            return Err(Error::Hook(format!("{site:#x} is already hooked")));
        }
        let patch = svc_bytes(arch, thumb);
        let head = self.cpu.memory().read_vec(site, 4)?;
        let movable = match arch {
            Arch::Arm64 => a64_movable(u32::from_le_bytes([head[0], head[1], head[2], head[3]])).then_some(4),
            Arch::Arm32 => a32_movable(&head, thumb),
        };
        let adr = if arch == Arch::Arm64 { a64_adr(u32::from_le_bytes([head[0], head[1], head[2], head[3]]), site) } else { None };
        let trampoline = match (movable, adr) {
            (_, Some((rd, value))) => Some(self.adr_trampoline(rd, value, site + 4)?),
            (Some(len), None) => Some(self.trampoline(&head[..len], site + len as u64, thumb)?),
            (None, None) => None,
        };
        let original = head[..patch.len()].to_vec();
        self.cpu.memory_mut().write(site, &patch)?;
        self.cpu.invalidate(site, patch.len() as u64);
        let id = self.next_hook_id();
        self.hooks.sites.insert(site, Entry { id, hook, site: Site::Inline { original, thumb, trampoline } });
        Ok(id)
    }

    /// Hooks an exported function of a loaded module by name.
    pub fn hook_symbol(&mut self, module: usize, symbol: &str, hook: Hook) -> Result<HookId, Error> {
        let address = self.symbol(module, symbol).ok_or_else(|| Error::Hook(format!("{symbol} not exported by module {module}")))?;
        self.hook(address, hook)
    }

    /// Redirects every call `module` makes to `symbol` through its GOT (and absolute pointers to
    /// it) to `hook`, leaving other callers and `dlsym` alone. Fails when the module imports no
    /// such symbol.
    pub fn hook_import(&mut self, module: usize, symbol: &str, hook: Hook) -> Result<HookId, Error> {
        let slots: Vec<u64> = self.linker.modules.get(module).map(|m| m.imports.iter().filter(|(_, n)| n == symbol).map(|(at, _)| *at).collect()).unwrap_or_default();
        let Some(&first) = slots.first() else {
            return Err(Error::Hook(format!("module {module} does not import {symbol}")));
        };
        let is64 = self.cpu.arch() == Arch::Arm64;
        let read = |emu: &Emulator, at: u64| -> Result<u64, Error> { Ok(if is64 { emu.cpu.memory().read_u64(at)? } else { emu.cpu.memory().read_u32(at)? as u64 }) };
        let target = read(self, first)?;
        let stub = self.new_stub()?;
        for &slot in &slots {
            if is64 {
                self.cpu.memory_mut().write_u64(slot, stub)?;
            } else {
                self.cpu.memory_mut().write_u32(slot, stub as u32)?;
            }
        }
        let id = self.next_hook_id();
        self.hooks.sites.insert(stub, Entry { id, hook, site: Site::Import { target, slots } });
        Ok(id)
    }

    /// Removes a hook: the instruction or the import slots get their original contents back.
    pub fn unhook(&mut self, id: HookId) -> Result<(), Error> {
        let Some((&at, _)) = self.hooks.sites.iter().find(|(_, e)| e.id == id) else {
            return Err(Error::Hook(format!("no hook {}", id.0)));
        };
        let entry = self.hooks.sites.remove(&at).expect("present");
        match entry.site {
            Site::Inline { original, .. } => {
                self.cpu.memory_mut().write(at, &original)?;
                self.cpu.invalidate(at, original.len() as u64);
            }
            Site::Import { target, slots } => {
                for slot in slots {
                    if self.cpu.arch() == Arch::Arm64 {
                        self.cpu.memory_mut().write_u64(slot, target)?;
                    } else {
                        self.cpu.memory_mut().write_u32(slot, target as u32)?;
                    }
                }
            }
        }
        Ok(())
    }

    /// For an A64 adr/adrp: loads its result into `rd` (xzr: nothing) and jumps to `back`.
    fn adr_trampoline(&mut self, rd: u32, value: u64, back: u64) -> Result<u64, Error> {
        // ldr x<rd>, #16; ldr x<scratch>, #20; br x<scratch>; nop; .quad value; .quad back
        let scratch = if rd == 16 { 17 } else { 16 };
        let mut code = Vec::with_capacity(32);
        for word in [0x5800_0080 | rd, 0x5800_00a0 | scratch, 0xd61f_0000 | (scratch << 5), 0xd503_201f] {
            code.extend_from_slice(&word.to_le_bytes());
        }
        code.extend_from_slice(&value.to_le_bytes());
        code.extend_from_slice(&back.to_le_bytes());
        let at = self.alloc_stub(code.len() as u64)?;
        self.cpu.memory_mut().write(at, &code)?;
        self.cpu.invalidate(at, code.len() as u64);
        Ok(at)
    }

    /// Code that runs `instruction` and jumps to `back`, in the stubs page.
    fn trampoline(&mut self, instruction: &[u8], back: u64, thumb: bool) -> Result<u64, Error> {
        let mut code = instruction.to_vec();
        match (self.cpu.arch(), thumb) {
            (Arch::Arm64, _) => {
                // ldr x16, #8; br x16; .quad back
                code.extend_from_slice(&0x5800_0050u32.to_le_bytes());
                code.extend_from_slice(&0xd61f_0200u32.to_le_bytes());
                code.extend_from_slice(&back.to_le_bytes());
            }
            (Arch::Arm32, false) => {
                // ldr pc, [pc, #-4]; .word back
                code.extend_from_slice(&0xe51f_f004u32.to_le_bytes());
                code.extend_from_slice(&(back as u32).to_le_bytes());
            }
            (Arch::Arm32, true) => {
                // The trampoline starts 4-aligned; ldr.w pc, [pc, #0] must be 4-aligned too so its
                // literal follows it: pad a 2-byte instruction with a nop.
                if code.len() == 2 {
                    code.extend_from_slice(&0xbf00u16.to_le_bytes());
                }
                code.extend_from_slice(&[0xdf, 0xf8, 0x00, 0xf0]);
                code.extend_from_slice(&((back | 1) as u32).to_le_bytes());
            }
        }
        let at = self.alloc_stub(code.len() as u64)?;
        self.cpu.memory_mut().write(at, &code)?;
        self.cpu.invalidate(at, code.len() as u64);
        Ok(at)
    }

    fn next_hook_id(&mut self) -> HookId {
        self.hooks.next_id += 1;
        HookId(self.hooks.next_id)
    }

    /// A new `svc #HOOK_SVC` stub (ARM state on arm32) in the stubs page, mapped on first use
    /// with the return stub first.
    fn new_stub(&mut self) -> Result<u64, Error> {
        let bytes = svc_bytes(self.cpu.arch(), false);
        let stub = self.alloc_stub(bytes.len() as u64)?;
        self.cpu.memory_mut().write(stub, &bytes)?;
        self.cpu.invalidate(stub, bytes.len() as u64);
        Ok(stub)
    }

    /// `size` bytes, 8-aligned, in the stubs page (mapped on first use, return stub first).
    pub(crate) fn alloc_stub(&mut self, size: u64) -> Result<u64, Error> {
        if self.hooks.stubs_next == 0 {
            let bytes = svc_bytes(self.cpu.arch(), false);
            self.cpu.map(layout::HOOK_STUBS, layout::HOOK_STUBS_SIZE, Perms::RX, "[hooks]")?;
            self.cpu.memory_mut().write(RETURN_STUB, &bytes)?;
            self.hooks.stubs_next = RETURN_STUB + 8;
        }
        let at = self.hooks.stubs_next;
        if at + size > layout::HOOK_STUBS + layout::HOOK_STUBS_SIZE {
            return Err(Error::Hook("no room for more hooks in the stubs page".into()));
        }
        self.hooks.stubs_next = (at + size + 7) & !7;
        Ok(at)
    }

    /// Whether an inline hook patched the instruction at `site`.
    pub(crate) fn hooked_at(&self, site: u64) -> bool {
        self.hooks.sites.contains_key(&site)
    }

    /// The hook site an `svc #HOOK_SVC` that stopped with the PC after it belongs to.
    fn hook_site(&self, pc: u64) -> Option<u64> {
        let at = pc & !1;
        let thumb = self.cpu.arch() == Arch::Arm32 && pc & 1 != 0;
        let site = at.checked_sub(if thumb { 2 } else { 4 })?;
        let found = match self.hooks.sites.get(&site) {
            Some(Entry { site: Site::Inline { thumb: t, .. }, .. }) => *t == thumb,
            Some(Entry { site: Site::Import { .. }, .. }) => !thumb,
            None => false,
        };
        (found || (site == RETURN_STUB && self.hooks.stubs_next != 0 && !thumb)).then_some(site)
    }

    /// Handles `svc #HOOK_SVC`. `None`: not a hook site (the caller treats it as a syscall);
    /// `Some(stop)`: a stop the run loop handles next (the original instruction's own, when it
    /// was stepped and stopped on something).
    pub(crate) fn hook_hit(&mut self, pc: u64) -> Result<Option<Option<Stop>>, Error> {
        let Some(site) = self.hook_site(pc) else { return Ok(None) };
        self.hooks.hits += 1;
        if site == RETURN_STUB {
            self.hook_return()?;
            return Ok(Some(None));
        }
        let entry = self.hooks.sites.get(&site).cloned().expect("site");
        let function = match &entry.site {
            Site::Inline { thumb: true, .. } => site | 1,
            _ => site,
        };
        let caller = self.cpu.reg(Reg::Lr);
        let mut call = Call { hook: entry.id, function, caller, ret: 0, sp: self.cpu.reg(Reg::Sp) };
        // Callbacks see the state at the function's entry.
        self.cpu.set_pc(function);
        let flow = match &entry.hook.enter {
            Some(enter) => enter(self, &mut call)?,
            None => Flow::Continue,
        };
        if let Flow::Return(value) = flow {
            self.cpu.set_reg(Reg::General(0), value);
            self.cpu.set_pc(caller);
            return Ok(Some(None));
        }
        if let Some(leave) = &entry.hook.leave {
            if self.hooks.stubs_next == 0 {
                self.alloc_stub(0)?;
            }
            let tid = self.kernel.threads.current;
            self.hooks.pending.push(Pending { tid, sp: call.sp, call: call.clone(), leave: leave.clone() });
            self.cpu.set_reg(Reg::Lr, RETURN_STUB);
        }
        match entry.site {
            Site::Import { target, .. } => {
                self.cpu.set_pc(target);
                Ok(Some(None))
            }
            Site::Inline { trampoline: Some(trampoline), thumb, .. } => {
                self.cpu.set_pc(trampoline | thumb as u64);
                Ok(Some(None))
            }
            Site::Inline { original, .. } => {
                // The hook may have been removed by its own callback.
                let patched = self.hooks.sites.contains_key(&site);
                let len = original.len() as u64;
                if patched {
                    self.cpu.memory_mut().write(site, &original)?;
                    self.cpu.invalidate(site, len);
                }
                self.cpu.set_pc(function);
                let stop = self.cpu.step();
                if patched && self.hooks.sites.contains_key(&site) {
                    let patch = svc_bytes(self.cpu.arch(), function & 1 != 0);
                    self.cpu.memory_mut().write(site, &patch)?;
                    self.cpu.invalidate(site, len);
                }
                Ok(Some(match stop? {
                    Stop::Budget => None,
                    other => Some(other),
                }))
            }
        }
    }

    /// The return stub: the leave callback of the innermost pending call of this thread. Calls
    /// skipped by a longjmp (deeper stack) are dropped.
    fn hook_return(&mut self) -> Result<(), Error> {
        let tid = self.kernel.threads.current;
        let sp = self.cpu.reg(Reg::Sp);
        let index = loop {
            let Some(i) = self.hooks.pending.iter().rposition(|p| p.tid == tid) else {
                return Err(Error::Hook("return stub reached with no hooked call pending".into()));
            };
            if self.hooks.pending[i].sp < sp {
                self.hooks.pending.remove(i);
                continue;
            }
            break i;
        };
        let pending = self.hooks.pending.remove(index);
        let mut call = pending.call;
        call.ret = self.cpu.reg(Reg::General(0));
        (pending.leave)(self, &mut call)?;
        self.cpu.set_reg(Reg::General(0), call.ret);
        self.cpu.set_pc(call.caller);
        Ok(())
    }
}
