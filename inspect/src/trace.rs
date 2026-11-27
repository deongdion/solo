//! Instruction trace: a filter by address range, a bounded ring of the last traced instructions
//! and an optional callback per instruction. The run loop single-steps while a trace is on, so a
//! trace costs nothing when it is off.

use std::collections::VecDeque;
use std::sync::Arc;

/// One traced instruction, before it executes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Executed {
    /// Address (Thumb bit set in Thumb state).
    pub pc: u64,
    /// The instruction's bytes (2 or 4).
    pub bytes: Vec<u8>,
    /// General registers the previous traced instruction changed, as (index, new value), when
    /// the trace records registers. Index 31 is SP on arm64, 13 on arm32.
    pub changed: Vec<(u8, u64)>,
    /// Thread that ran it.
    pub tid: u32,
}

pub type Sink = dyn Fn(&Executed) + Send + Sync;

#[derive(Clone)]
pub struct TraceConfig {
    /// Half-open address ranges to record; empty records everything.
    pub ranges: Vec<(u64, u64)>,
    /// Instructions kept in the ring (oldest dropped first).
    pub capacity: usize,
    /// Record which general registers each instruction changed.
    pub registers: bool,
    /// Called for every recorded instruction.
    pub sink: Option<Arc<Sink>>,
}

impl Default for TraceConfig {
    fn default() -> TraceConfig {
        TraceConfig { ranges: Vec::new(), capacity: 4096, registers: false, sink: None }
    }
}

#[derive(Clone)]
pub struct Trace {
    pub config: TraceConfig,
    pub ring: VecDeque<Executed>,
    /// Instructions recorded since the trace started (including those dropped from the ring).
    pub recorded: u64,
    /// Instructions stepped (recorded or not).
    pub stepped: u64,
    /// Register values after the last recorded instruction, for `changed`.
    pub last_registers: Vec<u64>,
}

impl Trace {
    pub fn new(config: TraceConfig) -> Trace {
        Trace { ring: VecDeque::with_capacity(config.capacity.min(1 << 16)), config, recorded: 0, stepped: 0, last_registers: Vec::new() }
    }

    pub fn wants(&self, pc: u64) -> bool {
        let at = pc & !1;
        self.config.ranges.is_empty() || self.config.ranges.iter().any(|(start, end)| at >= *start && at < *end)
    }

    pub fn push(&mut self, entry: Executed) {
        if let Some(sink) = &self.config.sink {
            sink(&entry);
        }
        self.recorded += 1;
        if self.config.capacity == 0 {
            return;
        }
        if self.ring.len() == self.config.capacity {
            self.ring.pop_front();
        }
        self.ring.push_back(entry);
    }

    /// The registers that differ from the last recorded values, and remembers `now`.
    pub fn diff(&mut self, now: Vec<u64>) -> Vec<(u8, u64)> {
        let changed = if self.last_registers.len() == now.len() {
            now.iter().zip(&self.last_registers).enumerate().filter(|(_, (a, b))| a != b).map(|(i, (a, _))| (i as u8, *a)).collect()
        } else {
            Vec::new()
        };
        self.last_registers = now;
        changed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ring_keeps_the_last_entries_and_filters_by_range() {
        let mut trace = Trace::new(TraceConfig { ranges: vec![(0x1000, 0x2000)], capacity: 2, ..Default::default() });
        assert!(trace.wants(0x1000) && trace.wants(0x1fff) && trace.wants(0x1001));
        assert!(!trace.wants(0x2000) && !trace.wants(0xfff));
        for pc in [0x1000, 0x1004, 0x1008] {
            trace.push(Executed { pc, bytes: vec![0; 4], changed: Vec::new(), tid: 1 });
        }
        assert_eq!(trace.recorded, 3);
        assert_eq!(trace.ring.iter().map(|e| e.pc).collect::<Vec<_>>(), vec![0x1004, 0x1008]);
        assert_eq!(trace.diff(vec![1, 2, 3]), vec![]);
        assert_eq!(trace.diff(vec![1, 5, 3]), vec![(1, 5)]);
    }
}
