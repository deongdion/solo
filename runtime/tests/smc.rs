//! Self-modifying code (bench/jni/libsmc.so): `patch` writes a function returning 111 into an
//! RWX buffer, runs it, overwrites it to return 222, flushes the instruction cache (arm64 IC IVAU,
//! arm32 the cacheflush syscall), and runs it again. A backend that does not re-translate on the
//! flush returns the stale 111 the second time. Both backends must return 111 then 222.

mod common;

use std::path::PathBuf;

use backend::Arch;
use machine::Perms;
use runtime::{Emulator, Options};

fn fixture(arch: Arch) -> PathBuf {
    let dir = if arch == Arch::Arm64 { "arm64" } else { "arm32" };
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../bench/jni").join(dir).join("libsmc.so")
}

fn run(arch: Arch) {
    let mut options = Options::new(arch);
    options.backend = common::backend();
    options.kernel.echo_output = false;
    options.budget = 10_000_000;
    let mut emu = Emulator::new(options).unwrap();
    let module = emu.load(fixture(arch).to_str().unwrap()).unwrap();

    // An RWX scratch page for the generated function (unicorn enforces the permissions).
    let code = 0x4000_0000u64;
    emu.cpu.map(code, 0x1000, Perms::RWX, "[smc]").unwrap();

    let patch = emu.symbol(module, "patch").unwrap();
    let result = emu.call(patch, &[code]).unwrap();
    assert_eq!(result, 111 * 1000 + 222, "stale translation after cache flush ({result})");
}

#[test]
fn smc_arm64() {
    run(Arch::Arm64);
}

#[test]
fn smc_arm32() {
    run(Arch::Arm32);
}
