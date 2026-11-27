//! The process clock. The kernel (`clock_gettime`, `gettimeofday`, file times) and the Java
//! models (`System.currentTimeMillis`, `System.nanoTime`) read the same `Clock`, so native and
//! Java time agree in every mode.

use std::sync::{Arc, Mutex};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

/// Where time comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClockMode {
    /// Host time.
    Host,
    /// Host time moved by `ns` (another date, the same rate).
    Offset { ns: i64 },
    /// Deterministic: wall time starts at `start_ns` (since the Unix epoch) and every read of
    /// any clock moves time on by `step_ns`, so runs with the same inputs read the same times
    /// and a loop that waits for time to pass still ends.
    Fixed { start_ns: u64, step_ns: u64 },
}

/// Uptime at process start in the monotonic clocks: a device is not booted the instant the app
/// starts (an uptime near zero, or monotonic time equal to wall time, both stand out).
pub const DEFAULT_UPTIME_NS: u64 = 3 * 86_400 * 1_000_000_000 + 4_321_000_000_000;

#[derive(Debug)]
struct State {
    mode: ClockMode,
    uptime_ns: u64,
    started: Instant,
    reads: u64,
}

/// A clock shared by the parts of one emulated process. `Clone` shares it; `detach` copies it for
/// a forked process.
#[derive(Debug, Clone)]
pub struct Clock(Arc<Mutex<State>>);

impl Default for Clock {
    fn default() -> Clock {
        Clock::new(ClockMode::Host, DEFAULT_UPTIME_NS)
    }
}

impl Clock {
    /// `uptime_ns`: monotonic time when the process starts.
    pub fn new(mode: ClockMode, uptime_ns: u64) -> Clock {
        Clock(Arc::new(Mutex::new(State { mode, uptime_ns, started: Instant::now(), reads: 0 })))
    }

    pub fn mode(&self) -> ClockMode {
        self.state().mode
    }

    /// An independent copy at the same point in time.
    pub fn detach(&self) -> Clock {
        let s = self.state();
        Clock(Arc::new(Mutex::new(State { mode: s.mode, uptime_ns: s.uptime_ns, started: s.started, reads: s.reads })))
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        // A panic while holding the lock leaves plain numbers behind, still valid.
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Nanoseconds since the process started, and the wall time then.
    fn read(&self) -> (u64, u64) {
        let mut s = self.state();
        match s.mode {
            ClockMode::Fixed { start_ns, step_ns } => {
                s.reads += 1;
                (s.reads.saturating_mul(step_ns), start_ns)
            }
            ClockMode::Host | ClockMode::Offset { .. } => {
                let elapsed = s.started.elapsed().as_nanos() as u64;
                let now = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos() as u64).unwrap_or(0);
                let now = match s.mode {
                    ClockMode::Offset { ns } => now.saturating_add_signed(ns),
                    _ => now,
                };
                (elapsed, now.saturating_sub(elapsed))
            }
        }
    }

    /// CLOCK_REALTIME: nanoseconds since the Unix epoch.
    pub fn realtime_ns(&self) -> u64 {
        let (elapsed, start) = self.read();
        start.saturating_add(elapsed)
    }

    /// CLOCK_MONOTONIC and CLOCK_BOOTTIME: nanoseconds since boot.
    pub fn monotonic_ns(&self) -> u64 {
        let (elapsed, _) = self.read();
        self.state().uptime_ns.saturating_add(elapsed)
    }

    /// CLOCK_PROCESS_CPUTIME_ID and CLOCK_THREAD_CPUTIME_ID: time since the process started.
    pub fn cputime_ns(&self) -> u64 {
        self.read().0
    }

    /// The time for a clock id of `clock_gettime`; None for an id Linux does not have.
    pub fn by_id(&self, id: u32) -> Option<u64> {
        match id {
            0 | 5 | 8 | 11 => Some(self.realtime_ns()), // REALTIME, _COARSE, _ALARM, TAI
            1 | 4 | 6 | 7 | 9 => Some(self.monotonic_ns()), // MONOTONIC, _RAW, _COARSE, BOOTTIME, _ALARM
            2 | 3 => Some(self.cputime_ns()),
            // Per-process and per-thread CPU clocks made by clock_getcpuclockid (negative ids).
            id if (id as i32) < 0 => Some(self.cputime_ns()),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixed_is_deterministic_and_advances() {
        let clock = Clock::new(ClockMode::Fixed { start_ns: 1_700_000_000_000_000_000, step_ns: 1000 }, 5_000);
        assert_eq!(clock.realtime_ns(), 1_700_000_000_000_001_000);
        assert_eq!(clock.realtime_ns(), 1_700_000_000_000_002_000);
        assert_eq!(clock.monotonic_ns(), 5_000 + 3_000);
        let fork = clock.detach();
        assert_eq!(fork.realtime_ns(), clock.realtime_ns());
        // Shared handles see one sequence.
        let shared = clock.clone();
        assert_eq!(shared.cputime_ns() + 1000, clock.cputime_ns());
        assert_eq!(clock.by_id(99), None);
    }

    #[test]
    fn host_monotonic_is_uptime_not_wall_time() {
        let clock = Clock::default();
        let wall = clock.realtime_ns();
        let mono = clock.monotonic_ns();
        assert!(mono >= DEFAULT_UPTIME_NS && mono < wall / 2);
        let offset = Clock::new(ClockMode::Offset { ns: -86_400_000_000_000 }, 0);
        assert!(offset.realtime_ns() + 86_000_000_000_000 < wall + 1_000_000_000);
    }
}
