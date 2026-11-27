//! Crash reports and the end-of-run summary.
//!
//! A crash names what stopped the guest (PLAN.md F4: an unknown host-call immediate is reported as
//! such, never as a Linux syscall), where, and the registers, with addresses shown as module+offset.

use std::fmt;

use backend::{Arch, Reg};

use crate::Emulator;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StopKind {
    /// `svc #imm` with an immediate that is neither a syscall (0) nor a host function.
    UnknownSvc(u32),
    Halted,
    Fault { address: u64, write: bool },
    Exception { pc: u64, kind: u32 },
}

#[derive(Debug, Clone)]
pub struct Crash {
    pub kind: StopKind,
    pub arch: Arch,
    pub pc: String,
    pub lr: String,
    pub registers: Vec<(String, u64)>,
    pub fault_region: Option<String>,
    /// False when the backend only knows the PC of the translated block that faulted.
    pub precise_pc: bool,
    /// Return addresses from the frame pointer chain, innermost first (module+offset).
    pub backtrace: Vec<String>,
}

/// Walks the frame record chain: AArch64 x29 points at {previous x29, x30}; Thumb code built
/// with frame pointers keeps the same record in r7. Stops at the first record outside a writable
/// mapping, a non-increasing frame pointer, or 32 frames, so a corrupt stack ends the walk.
fn backtrace(emulator: &Emulator) -> Vec<String> {
    let cpu = &emulator.cpu;
    let arch = cpu.arch();
    let word = if arch == Arch::Arm64 { 8 } else { 4 };
    let mut fp = cpu.reg(Reg::General(if arch == Arch::Arm64 { 29 } else { 7 }));
    let mut frames = Vec::new();
    let memory = cpu.memory();
    for _ in 0..32 {
        let writable = memory.mapping_at(fp).is_some_and(|m| m.perms.contains(machine::Perms::WRITE) && fp + 2 * word <= m.end);
        if fp == 0 || !fp.is_multiple_of(word) || !writable {
            break;
        }
        let read = |at: u64| if word == 8 { memory.read_u64(at).ok() } else { memory.read_u32(at).ok().map(u64::from) };
        let (Some(next), Some(lr)) = (read(fp), read(fp + word)) else { break };
        if lr == 0 || lr == backend::TRAMPOLINE {
            break;
        }
        frames.push(locate(emulator, lr));
        if next <= fp {
            break;
        }
        fp = next;
    }
    frames
}

fn locate(emulator: &Emulator, address: u64) -> String {
    match emulator.linker.module_at(address) {
        Some(m) => {
            let symbol = m
                .closest_symbol(address)
                .map(|(name, at)| format!(" ({name}+{:#x})", address - at))
                .unwrap_or_default();
            format!("{address:#x} {}+{:#x}{symbol}", m.name, address - m.base)
        }
        None => match emulator.cpu.memory().mapping_at(address) {
            Some(region) if !region.label.is_empty() => format!("{address:#x} {}", region.label),
            _ => format!("{address:#x}"),
        },
    }
}

pub(crate) fn capture(emulator: &Emulator, kind: StopKind) -> Crash {
    let cpu = &emulator.cpu;
    let arch = cpu.arch();
    let count = if arch == Arch::Arm64 { 31 } else { 13 };
    let mut registers: Vec<(String, u64)> = (0..count)
        .map(|i| {
            let name = if arch == Arch::Arm64 { format!("x{i}") } else { format!("r{i}") };
            (name, cpu.reg(Reg::General(i)))
        })
        .collect();
    registers.push(("sp".into(), cpu.reg(Reg::Sp)));
    let fault_region = match kind {
        StopKind::Fault { address, .. } => Some(match cpu.memory().mapping_at(address) {
            Some(m) => format!("inside {} [{:#x}-{:#x}] perms {:?}", m.label, m.start, m.end, m.perms),
            None => "unmapped".into(),
        }),
        _ => None,
    };
    let pc = match kind {
        StopKind::Exception { pc, .. } => pc,
        _ => cpu.pc(),
    };
    // Data faults stop at the block boundary unless precise faults are on; a fetch fault is
    // reported at the address that could not run, which is exact.
    let precise_pc = emulator.cpu.precise_faults()
        || match kind {
            StopKind::Fault { address, .. } => address == pc,
            _ => true,
        };
    Crash {
        kind,
        arch,
        pc: locate(emulator, pc),
        lr: locate(emulator, cpu.reg(Reg::Lr)),
        registers,
        fault_region,
        backtrace: backtrace(emulator),
        precise_pc,
    }
}

impl fmt::Display for Crash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.kind {
            StopKind::UnknownSvc(imm) => writeln!(f, "guest stopped: svc #{imm:#x} is not a syscall or a host function")?,
            StopKind::Halted => writeln!(f, "guest stopped: halted")?,
            StopKind::Fault { address, write } => writeln!(
                f,
                "guest stopped: invalid {} at {address:#x} ({})",
                if *write { "write" } else { "read or fetch" },
                self.fault_region.as_deref().unwrap_or("unmapped")
            )?,
            StopKind::Exception { kind, .. } => writeln!(f, "guest stopped: CPU exception {kind}")?,
        }
        if self.precise_pc {
            writeln!(f, "  pc {}", self.pc)?;
        } else {
            writeln!(f, "  pc {} (block start; enable precise_faults for the exact instruction)", self.pc)?;
        }
        write!(f, "  lr {}", self.lr)?;
        for (i, (name, value)) in self.registers.iter().enumerate() {
            if i % 4 == 0 {
                write!(f, "\n ")?;
            }
            write!(f, " {name:>3}={value:016x}")?;
        }
        if !self.backtrace.is_empty() {
            write!(f, "
  backtrace (frame pointers):")?;
            for (i, frame) in self.backtrace.iter().enumerate() {
                write!(f, "
    #{i} {frame}")?;
            }
        }
        Ok(())
    }
}

/// What the run needed and did not get (PLAN.md F7), and the dlopen timeline (F8).
#[derive(Debug, Clone, Default)]
pub struct Summary {
    pub unhandled_syscalls: Vec<(String, u64, u64)>,
    pub missing_libraries: Vec<(String, String)>,
    pub unresolved_symbols: Vec<(String, String, u64)>,
    pub linker_warnings: Vec<String>,
    pub dlopen: Vec<crate::host::DlopenEvent>,
    /// JNI calls no handler answered: signature, count, first native caller.
    pub unhandled_jni: Vec<(String, u64, String)>,
    /// JNI_OnLoad, RegisterNatives and FatalError, in order of the calls that made them.
    pub jni_timeline: Vec<String>,
    pub jni_warnings: Vec<String>,
}

impl Summary {
    pub fn is_clean(&self) -> bool {
        self.unhandled_syscalls.is_empty()
            && self.missing_libraries.is_empty()
            && self.unresolved_symbols.is_empty()
            && self.unhandled_jni.is_empty()
    }
}

pub(crate) fn summary(emulator: &Emulator) -> Summary {
    let report = &emulator.linker.report;
    let mut unresolved: Vec<(String, String, u64)> = report
        .unresolved_symbols
        .iter()
        .map(|(m, s)| {
            let calls = emulator.host.unresolved_calls.get(&(m.clone(), s.clone())).copied().unwrap_or(0);
            (m.clone(), s.clone(), calls)
        })
        .collect();
    unresolved.sort_by(|a, b| b.2.cmp(&a.2).then_with(|| a.cmp(b)));
    Summary {
        unhandled_syscalls: emulator.kernel.unhandled.iter().map(|(n, u)| (n.clone(), u.count, u.first_pc)).collect(),
        missing_libraries: report.missing_libraries.clone(),
        unresolved_symbols: unresolved,
        linker_warnings: report.warnings.clone(),
        dlopen: emulator.host.dlopen_events.clone(),
        unhandled_jni: emulator
            .vm
            .unhandled
            .iter()
            .map(|(s, u)| (s.clone(), u.count, emulator.locate(u.first_caller)))
            .collect(),
        jni_timeline: jni_timeline(emulator),
        jni_warnings: emulator.vm.warnings.clone(),
    }
}

fn jni_timeline(emulator: &Emulator) -> Vec<String> {
    let mut lines: Vec<String> = emulator
        .onload_events
        .iter()
        .map(|e| format!("JNI_OnLoad {} -> {:#x} ({:.2} ms)", e.module, e.version, e.millis))
        .collect();
    for event in &emulator.vm.events {
        lines.push(match event {
            javavm::JniEvent::RegisterNatives { class, methods, caller } => {
                let list: Vec<String> = methods.iter().map(|(n, d, f)| format!("{n}{d} -> {}", emulator.locate(*f))).collect();
                format!("RegisterNatives {class} from {}: {}", emulator.locate(*caller), list.join(", "))
            }
            javavm::JniEvent::UnregisterNatives { class, caller } => {
                format!("UnregisterNatives {class} from {}", emulator.locate(*caller))
            }
            javavm::JniEvent::FatalError { message, caller } => format!("FatalError {message:?} from {}", emulator.locate(*caller)),
        });
    }
    lines
}

impl fmt::Display for Summary {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "== run summary ==")?;
        writeln!(f, "unhandled syscalls: {}", self.unhandled_syscalls.len())?;
        for (name, count, pc) in &self.unhandled_syscalls {
            writeln!(f, "  {name} x{count} (first at {pc:#x})")?;
        }
        writeln!(f, "missing libraries: {}", self.missing_libraries.len())?;
        for (by, name) in &self.missing_libraries {
            writeln!(f, "  {name} (needed by {by})")?;
        }
        writeln!(f, "unresolved symbols: {}", self.unresolved_symbols.len())?;
        for (module, symbol, calls) in &self.unresolved_symbols {
            writeln!(f, "  {module}: {symbol} (called {calls}x)")?;
        }
        if !self.linker_warnings.is_empty() {
            writeln!(f, "linker warnings: {}", self.linker_warnings.len())?;
            for w in &self.linker_warnings {
                writeln!(f, "  {w}")?;
            }
        }
        writeln!(f, "unhandled JNI: {}", self.unhandled_jni.len())?;
        for (signature, count, caller) in &self.unhandled_jni {
            writeln!(f, "  {signature} x{count} (first from {caller})")?;
        }
        if !self.jni_timeline.is_empty() {
            writeln!(f, "JNI timeline:")?;
            for line in &self.jni_timeline {
                writeln!(f, "  {line}")?;
            }
        }
        for w in &self.jni_warnings {
            writeln!(f, "JNI warning: {w}")?;
        }
        writeln!(f, "dlopen: {}", self.dlopen.len())?;
        for e in &self.dlopen {
            writeln!(f, "  {} from {} -> {:#x} ({:.2} ms)", e.name, e.caller, e.handle, e.millis)?;
        }
        Ok(())
    }
}
