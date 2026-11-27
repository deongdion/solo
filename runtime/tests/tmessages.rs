//! Loads libtmessages.29.so (the M0 benchmark library) with its dependencies and runs its init
//! functions. JNI_OnLoad needs the JNI layer (M2) and is not called here.

mod common;

use std::path::PathBuf;
use std::time::Instant;

use backend::Arch;
use runtime::{Emulator, Options};

fn resources() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../bench/work/unidbg/unidbg-android/src")
}

fn check(arch: Arch) {
    let (abi, lib) = if arch == Arch::Arm64 { ("arm64-v8a", "lib64") } else { ("armeabi-v7a", "lib") };
    let file = resources().join("test/resources/example_binaries").join(abi).join("libtmessages.29.so");
    let sdk = resources().join("main/resources/android/sdk23").join(lib);
    if !file.is_file() || !sdk.is_dir() {
        eprintln!("unidbg resources not present; skipped");
        return;
    }
    let started = Instant::now();
    let mut options = Options::new(arch);
    options.backend = common::backend();
    options.library_dirs = vec![sdk];
    options.kernel.echo_output = false;
    options.kernel.random_seed = Some(1);
    options.kernel.trace_syscalls = std::env::var_os("TRACE_SYSCALLS").is_some();
    options.budget = 2_000_000_000;
    let mut emu = Emulator::new(options).expect("emulator");
    let created = started.elapsed();
    let module = match emu.load(file.to_str().unwrap()) {
        Ok(m) => m,
        Err(e) => panic!("load: {e}\n{}", emu.summary()),
    };
    let loaded = started.elapsed();
    assert!(emu.symbol(module, "JNI_OnLoad").is_some());
    // Probe: what JNI_OnLoad asks of Java (temporary, to choose the built-in models).
    if std::env::var_os("PROBE_ONLOAD").is_some() {
        emu.vm.policy = runtime::UnhandledPolicy::Zero;
        let r = emu.call_jni_onload(module);
        eprintln!("JNI_OnLoad -> {r:?}
{}", emu.summary());
    }
    eprintln!(
        "{arch:?}: emulator {:.1} ms, load+init {:.1} ms, modules {}, committed {} KiB",
        created.as_secs_f64() * 1000.0,
        (loaded - created).as_secs_f64() * 1000.0,
        emu.linker.modules.len(),
        emu.cpu.memory().committed() / 1024
    );
    eprintln!("{}", emu.summary());

    // Every loaded module is an ELF image in memory; each must be found at its base.
    let started = Instant::now();
    let hits = inspect::scan::scan(emu.cpu.memory()).unwrap();
    let elapsed = started.elapsed();
    let bases: Vec<u64> = hits.iter().filter(|h| h.kind == inspect::scan::Kind::Elf).map(|h| h.address).collect();
    for m in &emu.linker.modules {
        assert!(bases.contains(&m.base), "{} at {:#x} not found", m.name, m.base);
    }
    eprintln!(
        "scan: {} hits ({} ELF) over {} KiB in {:.1} ms",
        hits.len(),
        bases.len(),
        emu.cpu.memory().committed() / 1024,
        elapsed.as_secs_f64() * 1000.0
    );
}

#[test]
fn tmessages_arm64() {
    check(Arch::Arm64);
}

#[test]
fn tmessages_arm32() {
    check(Arch::Arm32);
}
