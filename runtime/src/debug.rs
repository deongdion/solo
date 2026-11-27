//! Debugger: software breakpoints, single steps and a callback that sees every stop.
//!
//! A breakpoint replaces the instruction at its address by `svc #BREAK_SVC`, as unidbg's
//! dynarmic backend does (its unicorn backend uses a code hook instead, which unicorn retranslates
//! for). On a hit the run loop moves the PC back to the breakpoint and calls the installed
//! `Debugger`, which can read and change registers and memory (`read_memory` shows the original
//! bytes under breakpoints), add or remove breakpoints, and pick how to go on. Continuing from a
//! breakpoint runs its instruction displaced: copied to a stub followed by `svc #BREAK_SVC`, which
//! jumps back (no register is used for the jump); an A64 adr/adrp is computed instead. Other
//! instructions (PC-relative ones, most of arm32) and single steps put the original instruction
//! back for one step. Guest code reading its own text sees the patch, as with hooks.

use std::collections::BTreeMap;

use backend::{Arch, Stop};

use crate::{Emulator, Error};

/// The `svc` immediate of a breakpoint (0xff returns from calls, 0xfe is a hook).
pub const BREAK_SVC: u32 = 0xfd;

/// Why the guest stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DebugEvent {
    /// At a breakpoint, before its instruction runs.
    Breakpoint(u64),
    /// After a single step (or before the first instruction, see `Emulator::break_next`).
    Step(u64),
    /// A fault or exception the guest has no handler for; the call fails after the debugger
    /// returns, whatever it returns.
    Signal { pc: u64, signo: u32 },
}

impl DebugEvent {
    pub fn pc(&self) -> u64 {
        match *self {
            DebugEvent::Breakpoint(pc) | DebugEvent::Step(pc) | DebugEvent::Signal { pc, .. } => pc,
        }
    }
}

/// How to go on after a stop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resume {
    /// Run until the next breakpoint.
    Continue,
    /// Run one instruction and stop again.
    Step,
    /// Fail the running call with `Error::Aborted`.
    Abort,
}

/// Called on every stop, inside the run loop; the emulator is not running while it executes.
pub trait Debugger: Send {
    fn stopped(&mut self, emu: &mut Emulator, event: DebugEvent) -> Resume;
}

impl<F: FnMut(&mut Emulator, DebugEvent) -> Resume + Send> Debugger for F {
    fn stopped(&mut self, emu: &mut Emulator, event: DebugEvent) -> Resume {
        self(emu, event)
    }
}

#[derive(Debug, Clone)]
pub(crate) struct Breakpoint {
    thumb: bool,
    original: Vec<u8>,
}

/// How a breakpoint's instruction runs when the debugger continues.
#[derive(Debug, Clone, Copy)]
enum Displaced {
    /// A stub with the instruction and an `svc #BREAK_SVC` back (address, Thumb bit set).
    Stub(u64),
    /// A64 adr/adrp: the register and its value.
    Adr(u32, u64),
    /// Put the original back for one step.
    Step,
}

#[derive(Default)]
pub(crate) struct Debug {
    pub(crate) breakpoints: BTreeMap<u64, Breakpoint>,
    /// By site; kept after a breakpoint is removed so setting it again reuses the stub.
    displaced: BTreeMap<u64, Displaced>,
    /// The `svc` ending a displaced stub, and where it goes back to (Thumb bit set).
    returns: BTreeMap<u64, u64>,
    pub(crate) debugger: Option<Box<dyn Debugger>>,
    /// Stop after every instruction.
    pub(crate) stepping: bool,
    /// Report the PC before the next instruction runs (`break_next`).
    announce: bool,
}

impl Clone for Debug {
    /// Forks keep the breakpoints (they are in the copied memory) but no debugger.
    fn clone(&self) -> Debug {
        Debug {
            breakpoints: self.breakpoints.clone(),
            displaced: self.displaced.clone(),
            returns: self.returns.clone(),
            debugger: None,
            stepping: false,
            announce: false,
        }
    }
}

fn break_bytes(arch: Arch, thumb: bool) -> Vec<u8> {
    match arch {
        Arch::Arm64 => (0xd400_0001u32 | (BREAK_SVC << 5)).to_le_bytes().to_vec(),
        Arch::Arm32 if thumb => (0xdf00u16 | BREAK_SVC as u16).to_le_bytes().to_vec(),
        Arch::Arm32 => (0xef00_0000u32 | BREAK_SVC).to_le_bytes().to_vec(),
    }
}

impl Emulator {
    /// Installs the debugger that sees breakpoints and steps (replacing any other).
    pub fn set_debugger(&mut self, debugger: Box<dyn Debugger>) {
        self.debug.debugger = Some(debugger);
    }

    pub fn take_debugger(&mut self) -> Option<Box<dyn Debugger>> {
        self.debug.debugger.take()
    }

    /// Stops before the next instruction that runs (the first one of the next call, or the one
    /// after the current stop when called from the debugger).
    pub fn break_next(&mut self) {
        self.debug.stepping = true;
        self.debug.announce = true;
    }

    /// Sets a breakpoint (Thumb bit set for Thumb code on arm32). Setting one twice is a no-op.
    pub fn add_breakpoint(&mut self, address: u64) -> Result<(), Error> {
        let arch = self.cpu.arch();
        let thumb = arch == Arch::Arm32 && address & 1 != 0;
        let site = address & !1;
        if self.debug.breakpoints.contains_key(&site) {
            return Ok(());
        }
        if self.hooked_at(site) {
            return Err(Error::Hook(format!("{site:#x} is hooked; break on the hook's callback instead")));
        }
        let patch = break_bytes(arch, thumb);
        let original = self.cpu.memory().read_vec(site, patch.len())?;
        self.cpu.memory_mut().write(site, &patch)?;
        self.cpu.invalidate(site, patch.len() as u64);
        self.debug.breakpoints.insert(site, Breakpoint { thumb, original });
        Ok(())
    }

    /// Removes a breakpoint; false if there was none at the address.
    pub fn remove_breakpoint(&mut self, address: u64) -> Result<bool, Error> {
        let site = address & !1;
        let Some(bp) = self.debug.breakpoints.remove(&site) else { return Ok(false) };
        self.cpu.memory_mut().write(site, &bp.original)?;
        self.cpu.invalidate(site, bp.original.len() as u64);
        Ok(true)
    }

    pub fn breakpoints(&self) -> impl Iterator<Item = u64> + '_ {
        self.debug.breakpoints.iter().map(|(site, bp)| site | bp.thumb as u64)
    }

    /// Guest memory as the program was loaded: breakpoint patches show their original bytes.
    pub fn read_memory(&self, address: u64, length: usize) -> Result<Vec<u8>, Error> {
        let mut bytes = self.cpu.memory().read_vec(address, length)?;
        let end = address + length as u64;
        for (site, bp) in self.debug.breakpoints.range(address.saturating_sub(4)..end) {
            for (i, b) in bp.original.iter().enumerate() {
                let at = site + i as u64;
                if at >= address && at < end {
                    bytes[(at - address) as usize] = *b;
                }
            }
        }
        Ok(bytes)
    }

    /// Writes guest memory under breakpoints too (their original bytes take the write).
    pub fn write_memory(&mut self, address: u64, data: &[u8]) -> Result<(), Error> {
        let end = address + data.len() as u64;
        let sites: Vec<u64> = self.debug.breakpoints.range(address.saturating_sub(4)..end).map(|(s, _)| *s).collect();
        self.cpu.memory_mut().write(address, data)?;
        for site in sites {
            // The stub holds the old instruction.
            self.debug.displaced.remove(&site);
            let bp = self.debug.breakpoints.get_mut(&site).expect("breakpoint");
            for (i, b) in bp.original.iter_mut().enumerate() {
                let at = site + i as u64;
                if at >= address && at < end {
                    *b = data[(at - address) as usize];
                }
            }
            let patch = break_bytes(self.cpu.arch(), bp.thumb);
            self.cpu.memory_mut().write(site, &patch)?;
        }
        self.cpu.invalidate(address, data.len() as u64);
        Ok(())
    }

    /// The breakpoint an `svc #BREAK_SVC` that stopped with the PC after it belongs to.
    fn break_site(&self, pc: u64) -> Option<u64> {
        let thumb = self.cpu.arch() == Arch::Arm32 && pc & 1 != 0;
        let site = (pc & !1).checked_sub(if thumb { 2 } else { 4 })?;
        self.debug.breakpoints.get(&site).filter(|bp| bp.thumb == thumb).map(|_| site)
    }

    /// Runs the debugger for one stop and applies its answer.
    fn notify(&mut self, event: DebugEvent) -> Result<(), Error> {
        let Some(mut debugger) = self.debug.debugger.take() else {
            return Err(Error::Breakpoint(event.pc()));
        };
        self.debug.stepping = false;
        let resume = debugger.stopped(self, event);
        // From inside the debugger, break_next means "after this stop", already the case.
        self.debug.announce = false;
        // The callback may have installed another debugger; it wins.
        if self.debug.debugger.is_none() {
            self.debug.debugger = Some(debugger);
        }
        match resume {
            Resume::Continue => Ok(()),
            Resume::Step => {
                self.debug.stepping = true;
                Ok(())
            }
            Resume::Abort => {
                self.debug.stepping = false;
                Err(Error::Aborted)
            }
        }
    }

    /// Handles `svc #BREAK_SVC`. `None`: not a breakpoint (a syscall then); otherwise the stop
    /// of the instruction run over the breakpoint, if it stopped on something.
    pub(crate) fn break_hit(&mut self, pc: u64) -> Result<Option<Option<Stop>>, Error> {
        let thumb = self.cpu.arch() == Arch::Arm32 && pc & 1 != 0;
        if let Some(back) = (pc & !1).checked_sub(if thumb { 2 } else { 4 }).and_then(|s| self.debug.returns.get(&s)) {
            self.cpu.set_pc(*back);
            return Ok(Some(None));
        }
        let Some(site) = self.break_site(pc) else { return Ok(None) };
        let at = site | (pc & 1);
        self.cpu.set_pc(at);
        self.notify(DebugEvent::Breakpoint(at))?;
        if self.debug.stepping {
            // The step itself runs from the loop (`debug_step`), over the breakpoint.
            return Ok(Some(None));
        }
        // The debugger may have moved the PC or removed the breakpoint.
        if self.cpu.pc() == at && self.debug.breakpoints.contains_key(&site) {
            match self.displaced(site)? {
                Displaced::Stub(stub) => {
                    self.cpu.set_pc(stub);
                    return Ok(Some(None));
                }
                Displaced::Adr(rd, value) => {
                    self.cpu.set_reg(backend::Reg::General(rd as u8), value);
                    self.cpu.set_pc(site + 4);
                    return Ok(Some(None));
                }
                Displaced::Step => {}
            }
        }
        Ok(Some(match self.step_over()? {
            Stop::Budget => None,
            other => Some(other),
        }))
    }

    /// How the instruction at a breakpoint runs on continue (made on the first hit).
    fn displaced(&mut self, site: u64) -> Result<Displaced, Error> {
        if let Some(d) = self.debug.displaced.get(&site) {
            return Ok(*d);
        }
        let arch = self.cpu.arch();
        let bp = self.debug.breakpoints.get(&site).cloned().expect("breakpoint");
        let mut head = self.read_memory(site, 4)?;
        let length = match arch {
            Arch::Arm64 => {
                let insn = u32::from_le_bytes([head[0], head[1], head[2], head[3]]);
                if let Some((rd, value)) = crate::hooks::a64_adr(insn, site) {
                    self.debug.displaced.insert(site, Displaced::Adr(rd, value));
                    return Ok(Displaced::Adr(rd, value));
                }
                crate::hooks::a64_movable(insn).then_some(4)
            }
            Arch::Arm32 => crate::hooks::a32_movable(&head, bp.thumb),
        };
        let Some(length) = length else {
            self.debug.displaced.insert(site, Displaced::Step);
            return Ok(Displaced::Step);
        };
        head.truncate(length);
        let back = break_bytes(arch, bp.thumb);
        let mut code = head;
        code.extend_from_slice(&back);
        let stub = self.alloc_stub(code.len() as u64)?;
        self.cpu.memory_mut().write(stub, &code)?;
        self.cpu.invalidate(stub, code.len() as u64);
        let ret = stub + length as u64;
        self.debug.returns.insert(ret, (site + length as u64) | bp.thumb as u64);
        let entry = Displaced::Stub(stub | bp.thumb as u64);
        self.debug.displaced.insert(site, entry);
        Ok(entry)
    }

    /// One instruction; a breakpoint at the PC runs its original instruction.
    fn step_over(&mut self) -> Result<Stop, Error> {
        let pc = self.cpu.pc();
        let site = pc & !1;
        let Some(bp) = self.debug.breakpoints.get(&site).cloned() else {
            return Ok(self.cpu.step()?);
        };
        let len = bp.original.len() as u64;
        self.cpu.memory_mut().write(site, &bp.original)?;
        self.cpu.invalidate(site, len);
        let stop = self.cpu.step();
        if self.debug.breakpoints.contains_key(&site) {
            let patch = break_bytes(self.cpu.arch(), bp.thumb);
            self.cpu.memory_mut().write(site, &patch)?;
            self.cpu.invalidate(site, len);
        }
        Ok(stop?)
    }

    /// While stepping: one instruction, then the debugger (when it completed without a stop of
    /// its own). After `break_next` the PC is reported before anything runs.
    pub(crate) fn debug_step(&mut self) -> Result<Stop, Error> {
        if self.debug.announce {
            self.debug.announce = false;
            self.notify(DebugEvent::Step(self.cpu.pc()))?;
            if !self.debug.stepping {
                return Ok(Stop::Budget);
            }
        }
        let stop = self.step_over()?;
        if stop == Stop::Budget {
            self.notify(DebugEvent::Step(self.cpu.pc()))?;
        }
        Ok(stop)
    }

    /// Tells the debugger about a fatal signal before the call fails.
    pub(crate) fn debug_signal(&mut self, pc: u64, signo: u32) {
        if self.debug.debugger.is_some() {
            let _ = self.notify(DebugEvent::Signal { pc, signo });
            self.debug.stepping = false;
        }
    }
}
