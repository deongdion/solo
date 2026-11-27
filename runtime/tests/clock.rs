//! The process clock (PLAN.md F19) through bionic's time functions: a fixed clock gives every run
//! the same times, native and Java time share one sequence, forks continue it independently, and
//! the monotonic clocks report uptime rather than wall time.

mod common;

use std::path::PathBuf;

use backend::Arch;
use runtime::{ClockMode, Emulator, Options};

const START: u64 = 1_700_000_000_123_456_789;
const STEP: u64 = 1_000_000;
const UPTIME: u64 = 3_600_000_000_000;

fn emulator(arch: Arch, clock: ClockMode) -> Option<(Emulator, usize)> {
    let dir = if arch == Arch::Arm64 { "lib64" } else { "lib" };
    let sdk = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../bench/work/unidbg/unidbg-android/src/main/resources/android/sdk23")
        .join(dir);
    if !sdk.is_dir() {
        eprintln!("sdk23 libraries not present; skipped");
        return None;
    }
    let mut options = Options::new(arch);
    options.backend = common::backend();
    options.library_dirs = vec![sdk];
    options.kernel.echo_output = false;
    options.kernel.random_seed = Some(1);
    options.kernel.clock = clock;
    options.kernel.uptime_ns = UPTIME;
    options.budget = 200_000_000;
    let mut emu = Emulator::new(options).unwrap();
    let libc = emu.load("libc.so").unwrap();
    Some((emu, libc))
}

/// clock_gettime(id) as nanoseconds, read back from guest memory.
fn clock_gettime(emu: &mut Emulator, libc: usize, buffer: u64, id: u64) -> u64 {
    let f = emu.symbol(libc, "clock_gettime").unwrap();
    assert_eq!(emu.call(f, &[id, buffer]).unwrap() as u32, 0);
    let (secs, nanos) = if emu.arch() == Arch::Arm64 {
        (emu.cpu.memory().read_u64(buffer).unwrap(), emu.cpu.memory().read_u64(buffer + 8).unwrap())
    } else {
        (emu.cpu.memory().read_u32(buffer).unwrap() as u64, emu.cpu.memory().read_u32(buffer + 4).unwrap() as u64)
    };
    secs * 1_000_000_000 + nanos
}

/// Every time source once: realtime, monotonic, gettimeofday (µs), time (s).
fn readings(emu: &mut Emulator, libc: usize) -> Vec<u64> {
    let malloc = emu.symbol(libc, "malloc").unwrap();
    let buffer = emu.call(malloc, &[64]).unwrap();
    let mut out = vec![clock_gettime(emu, libc, buffer, 0), clock_gettime(emu, libc, buffer, 1)];
    let gettimeofday = emu.symbol(libc, "gettimeofday").unwrap();
    assert_eq!(emu.call(gettimeofday, &[buffer, 0]).unwrap() as u32, 0);
    let (secs, micros) = if emu.arch() == Arch::Arm64 {
        (emu.cpu.memory().read_u64(buffer).unwrap(), emu.cpu.memory().read_u64(buffer + 8).unwrap())
    } else {
        (emu.cpu.memory().read_u32(buffer).unwrap() as u64, emu.cpu.memory().read_u32(buffer + 4).unwrap() as u64)
    };
    out.push(secs * 1_000_000 + micros);
    let time = emu.symbol(libc, "time").unwrap();
    out.push(emu.call(time, &[0]).unwrap() & if emu.arch() == Arch::Arm64 { u64::MAX } else { 0xffff_ffff });
    out
}

fn fixed(arch: Arch) {
    let mode = ClockMode::Fixed { start_ns: START, step_ns: STEP };
    let Some((mut a, libc)) = emulator(arch, mode) else { return };
    let first = readings(&mut a, libc);
    // Times step once per read, whichever clock is read.
    let n = |reads: u64| START + reads * STEP;
    assert_eq!(first[0], n(1), "{first:?}");
    assert_eq!(first[1], UPTIME + 2 * STEP);
    assert_eq!(first[2], n(3) / 1000);
    assert_eq!(first[3], n(4) / 1_000_000_000);

    // The same configuration reads the same times.
    let (mut b, libc_b) = emulator(arch, mode).unwrap();
    assert_eq!(readings(&mut b, libc_b), first);

    // Java reads the same clock: the next value of the shared sequence.
    assert_eq!(a.vm.clock.realtime_ns(), n(5));
    // A fork continues from the same point, independently of its parent.
    let mut fork = a.fork().unwrap();
    let from_fork = readings(&mut fork, libc);
    let from_parent = readings(&mut a, libc);
    assert_eq!(from_fork, from_parent);
    assert_eq!(from_parent[0], n(6));
}

fn host(arch: Arch) {
    let Some((mut emu, libc)) = emulator(arch, ClockMode::Host) else { return };
    let r = readings(&mut emu, libc);
    let wall = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos() as u64;
    assert!(r[0] <= wall && wall - r[0] < 10_000_000_000, "{r:?}");
    // Monotonic time is uptime: the configured uptime plus the time since the process started.
    assert!(r[1] >= UPTIME && r[1] < UPTIME + 60_000_000_000, "{r:?}");
    // An unknown clock id fails as on Linux.
    let malloc = emu.symbol(libc, "malloc").unwrap();
    let buffer = emu.call(malloc, &[64]).unwrap();
    let f = emu.symbol(libc, "clock_gettime").unwrap();
    assert_eq!(emu.call(f, &[99, buffer]).unwrap() as u32, u32::MAX);
}

#[test]
fn fixed_arm64() {
    fixed(Arch::Arm64);
}

#[test]
fn fixed_arm32() {
    fixed(Arch::Arm32);
}

#[test]
fn host_arm64() {
    host(Arch::Arm64);
}

#[test]
fn host_arm32() {
    host(Arch::Arm32);
}
