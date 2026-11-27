//! Instruction trace on the emulator (`inspect::trace`): started and stopped between or during
//! calls; while on, the run loop single-steps and records the instructions in the trace's ranges.

use backend::{Arch, Reg, Stop};
use inspect::trace::{Executed, Trace, TraceConfig};

use crate::{Emulator, Error};

impl Emulator {
    /// Starts an instruction trace (replacing a running one).
    pub fn start_trace(&mut self, config: TraceConfig) {
        self.trace = Some(Trace::new(config));
    }

    /// Stops the trace and returns what it recorded.
    pub fn stop_trace(&mut self) -> Option<Trace> {
        self.trace.take()
    }

    pub fn trace(&self) -> Option<&Trace> {
        self.trace.as_ref()
    }

    /// The address range of a loaded module, for `TraceConfig::ranges`.
    pub fn module_range(&self, module: usize) -> Option<(u64, u64)> {
        self.linker.modules.get(module).map(|m| (m.base, m.base + m.size))
    }

    /// One trace line: `[module offset] [bytes] address: instruction  changed registers`.
    pub fn format_executed(&self, e: &Executed) -> String {
        let arch = self.cpu.arch();
        let at = e.pc & !1;
        let place = match self.linker.module_at(at) {
            Some(m) => format!("{} {:#07x}", m.name, at - m.base),
            None => "?".into(),
        };
        let hex: String = e.bytes.iter().map(|b| format!("{b:02x}")).collect();
        let (_, text) = inspect::disasm::disassemble(arch, e.pc & 1 != 0, &e.bytes);
        let mut line = format!("[{place}] [{hex:8}] {at:#x}: {text}");
        if !e.changed.is_empty() {
            line.push_str(" ;");
            for (index, value) in &e.changed {
                let name = register_name(arch, *index);
                line.push_str(&format!(" {name}={value:#x}"));
            }
        }
        line
    }

    /// Resumes in single steps, recording what the trace asks for: (stop, instructions run).
    pub(crate) fn traced_resume(&mut self, budget: u64) -> Result<(Stop, u64), Error> {
        let interrupt = self.cpu.interrupt();
        let arch = self.cpu.arch();
        let mut run = 0u64;
        while run < budget {
            let pc = self.cpu.pc();
            let Some(trace) = self.trace.as_ref() else { break };
            if trace.wants(pc) {
                let want_registers = trace.config.registers;
                let at = pc & !1;
                let mut bytes = self.cpu.memory().read_vec(at, 2).unwrap_or_default();
                let wide = arch == Arch::Arm64 || pc & 1 == 0 || bytes.get(1).is_some_and(|b| b >> 3 >= 0b11101);
                if wide {
                    bytes = self.cpu.memory().read_vec(at, 4).unwrap_or(bytes);
                }
                let now = if want_registers { registers(self) } else { Vec::new() };
                let tid = self.kernel.threads.current;
                if let Some(trace) = self.trace.as_mut() {
                    let changed = if want_registers { trace.diff(now) } else { Vec::new() };
                    trace.push(Executed { pc, bytes, changed, tid });
                }
            }
            let stop = self.cpu.step()?;
            run += 1;
            if let Some(trace) = self.trace.as_mut() {
                trace.stepped += 1;
            }
            if stop != Stop::Budget {
                return Ok((stop, run));
            }
            if interrupt.is_requested() {
                return Ok((Stop::Halted, run));
            }
        }
        Ok((Stop::Budget, run))
    }
}

fn registers(emu: &Emulator) -> Vec<u64> {
    let count = if emu.cpu.arch() == Arch::Arm64 { 31 } else { 13 };
    let mut out: Vec<u64> = (0..count).map(|i| emu.cpu.reg(Reg::General(i))).collect();
    if emu.cpu.arch() == Arch::Arm32 {
        out.push(emu.cpu.reg(Reg::Sp));
        out.push(emu.cpu.reg(Reg::Lr));
    } else {
        out.push(emu.cpu.reg(Reg::Sp));
    }
    out
}

fn register_name(arch: Arch, index: u8) -> String {
    match (arch, index) {
        (Arch::Arm64, 31) => "sp".into(),
        (Arch::Arm64, 30) => "lr".into(),
        (Arch::Arm64, i) => format!("x{i}"),
        (Arch::Arm32, 13) => "sp".into(),
        (Arch::Arm32, 14) => "lr".into(),
        (Arch::Arm32, i) => format!("r{i}"),
    }
}
