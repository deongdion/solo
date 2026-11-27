//! Guest threads (bench/jni Native.threads): pthread workers under a mutex, a condition-variable
//! ping-pong and a thread that only spins, which needs preemption. unidbg (with its thread
//! dispatcher on) completes only the first part; see bench/jni/README.md.

mod common;

use std::path::PathBuf;

use backend::Arch;
use runtime::{Emulator, Error, Options, Value};

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..")
}

fn emulator(arch: Arch) -> Option<Emulator> {
    let (dir, lib) = if arch == Arch::Arm64 { ("arm64", "lib64") } else { ("arm32", "lib") };
    let fixture = root().join("bench/jni").join(dir).join("libjnifix.so");
    let sdk = root().join("bench/work/unidbg/unidbg-android/src/main/resources/android/sdk23").join(lib);
    if !sdk.is_dir() {
        eprintln!("sdk23 libraries not present; skipped");
        return None;
    }
    let mut options = Options::new(arch);
    options.backend = common::backend();
    options.library_dirs = vec![sdk];
    options.kernel.echo_output = false;
    options.budget = 2_000_000_000;
    // A fixed clock and seed, so a run that mixes time, randomness and threads is reproducible
    // (PLAN.md F18/F19); `determinism` below checks two fresh emulators run identically.
    options.kernel.clock = runtime::ClockMode::Fixed { start_ns: 1_700_000_000_000_000_000, step_ns: 1000 };
    options.kernel.random_seed = Some(1);
    let mut emu = Emulator::new(options).unwrap();
    let module = emu.load(fixture.to_str().unwrap()).unwrap();
    emu.call_jni_onload(module).unwrap();
    Some(emu)
}

fn threads(emu: &mut Emulator, n: i32, rounds: i32, parts: i32) -> Result<String, Error> {
    let r = emu.call_static("fix/Native", "threads", "(III)Ljava/lang/String;", &[Value::Int(n), Value::Int(rounds), Value::Int(parts)])?;
    Ok(emu.vm.string(r.object().unwrap()).unwrap())
}

fn run(arch: Arch) {
    let Some(mut emu) = emulator(arch) else { return };
    let out = threads(&mut emu, 4, 1000, 7).unwrap_or_else(|e| panic!("{e}\n{}", emu.summary()));
    assert_eq!(out, "counter 10000 joined 100; pings 2000 turn 0; spun 7;\n");
    // All threads exited; the next call starts on the main thread again.
    assert_eq!(emu.kernel.threads.count(), 1);
    let switches = emu.kernel.threads.switches;
    assert_eq!(threads(&mut emu, 2, 10, 1).unwrap(), "counter 30 joined 30;\n");

    // The interleaving is the same on every run.
    let Some(mut again) = emulator(arch) else { return };
    threads(&mut again, 4, 1000, 7).unwrap();
    assert_eq!(again.kernel.threads.switches, switches);
}

#[test]
fn threads_arm64() {
    run(Arch::Arm64);
}

#[test]
fn threads_arm32() {
    run(Arch::Arm32);
}

/// A workload mixing threads, a futex, time and randomness runs identically twice (PLAN.md
/// F18/F19): same output, same thread interleaving, and the same number of instructions executed.
fn determinism(arch: Arch) {
    let once = || {
        let mut emu = emulator(arch)?;
        let out = threads(&mut emu, 4, 500, 7).unwrap();
        Some((out, emu.kernel.threads.switches, emu.cpu.executed()))
    };
    let Some(first) = once() else { return };
    let second = once().unwrap();
    assert_eq!(first, second, "run was not reproducible ({arch:?})");
}

#[test]
fn determinism_arm64() {
    determinism(Arch::Arm64);
}

#[test]
fn determinism_arm32() {
    determinism(Arch::Arm32);
}
