//! Signals (bench/jni Native.signals): a SA_SIGINFO handler run by raise(), a SIGSEGV handler that
//! leaves a faulting read with siglongjmp, and a blocked signal that stays pending until unblocked.

mod common;

use std::path::PathBuf;

use backend::Arch;
use runtime::{Emulator, Options};

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..")
}

fn run(arch: Arch) {
    let (dir, lib) = if arch == Arch::Arm64 { ("arm64", "lib64") } else { ("arm32", "lib") };
    let fixture = root().join("bench/jni").join(dir).join("libjnifix.so");
    let sdk = root().join("bench/work/unidbg/unidbg-android/src/main/resources/android/sdk23").join(lib);
    if !sdk.is_dir() {
        eprintln!("sdk23 libraries not present; skipped");
        return;
    }
    let mut options = Options::new(arch);
    options.backend = common::backend();
    options.library_dirs = vec![sdk];
    options.kernel.echo_output = false;
    options.budget = 50_000_000;
    let mut emu = Emulator::new(options).unwrap();
    let module = emu.load(fixture.to_str().unwrap()).unwrap();
    emu.call_jni_onload(module).unwrap();
    let r = emu.call_static("fix/Native", "signals", "()Ljava/lang/String;", &[]).unwrap_or_else(|e| panic!("{e}\n{}", emu.summary()));
    let out = emu.vm.string(r.object().unwrap()).unwrap();
    assert_eq!(out, std::fs::read_to_string(root().join("bench/jni/expected_signals.txt")).unwrap());
    // The handlers ran: twice SIGUSR1, two SIGSEGV probes, SIGUSR2 once unblocked.
    assert_eq!(emu.kernel.signals.delivered, 5);
}

#[test]
fn signals_arm64() {
    run(Arch::Arm64);
}

#[test]
fn signals_arm32() {
    run(Arch::Arm32);
}

