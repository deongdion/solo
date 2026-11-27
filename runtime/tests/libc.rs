//! Loads the bundled Android 6.0 bionic and calls into it.

mod common;

use std::path::PathBuf;

use backend::Arch;
use runtime::{Emulator, Options};

fn sdk(arch: Arch) -> Option<PathBuf> {
    let dir = if arch == Arch::Arm64 { "lib64" } else { "lib" };
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../bench/work/unidbg/unidbg-android/src/main/resources/android/sdk23")
        .join(dir);
    path.is_dir().then_some(path)
}

fn emulator(arch: Arch) -> Option<Emulator> {
    let dir = sdk(arch)?;
    let mut options = Options::new(arch);
    options.backend = common::backend();
    options.library_dirs = vec![dir];
    options.kernel.echo_output = false;
    options.kernel.random_seed = Some(1);
    options.budget = 200_000_000;
    options.kernel.trace_syscalls = std::env::var_os("TRACE_SYSCALLS").is_some();
    Some(Emulator::new(options).expect("emulator"))
}

fn put(emu: &mut Emulator, at: u64, text: &str) {
    let mut bytes = text.as_bytes().to_vec();
    bytes.push(0);
    emu.cpu.memory_mut().write(at, &bytes).unwrap();
}

fn check(arch: Arch) {
    let Some(mut emu) = emulator(arch) else {
        eprintln!("sdk23 libraries not present; skipped");
        return;
    };
    let libc = match emu.load("libc.so") {
        Ok(m) => m,
        Err(e) => panic!("load libc: {e}\n{}", emu.summary()),
    };
    let malloc = emu.symbol(libc, "malloc").unwrap();
    let buffer = emu.call(malloc, &[256]).unwrap();
    assert_ne!(buffer, 0);
    let text = buffer + 128;
    put(&mut emu, text, "hello");
    let strlen = emu.symbol(libc, "strlen").unwrap();
    assert_eq!(emu.call(strlen, &[text]).unwrap(), 5);

    let format = buffer + 192;
    put(&mut emu, format, "%s %d %x");
    let sprintf = emu.symbol(libc, "sprintf").unwrap();
    let n = emu.call(sprintf, &[buffer, format, text, 42, 0xbeef]).unwrap();
    let out = emu.cpu.memory().read_cstr(buffer, 64).unwrap();
    assert_eq!(String::from_utf8_lossy(&out), "hello 42 beef");
    assert_eq!(n, 13);

    let puts = emu.symbol(libc, "puts").unwrap();
    emu.call(puts, &[text]).unwrap();
    let fflush = emu.symbol(libc, "fflush").unwrap();
    emu.call(fflush, &[0]).unwrap();
    assert_eq!(emu.kernel.stdout, b"hello\n");

    let getpid = emu.symbol(libc, "getpid").unwrap();
    assert_eq!(emu.call(getpid, &[]).unwrap(), 2048);
    eprintln!("{}", emu.summary());
}

#[test]
fn libc_arm64() {
    check(Arch::Arm64);
}

#[test]
fn libc_arm32() {
    check(Arch::Arm32);
}

fn crash_has_backtrace(arch: Arch) -> Option<runtime::Crash> {
    let dir = sdk(arch)?;
    let mut options = Options::new(arch);
    options.backend = common::backend();
    options.library_dirs = vec![dir];
    options.kernel.echo_output = false;
    options.precise_faults = true;
    let mut emu = Emulator::new(options).unwrap();
    let libc = emu.load("libc.so").unwrap();
    let malloc = emu.symbol(libc, "malloc").unwrap();
    let buffer = emu.call(malloc, &[256]).unwrap();
    put(&mut emu, buffer + 128, "%s");
    let snprintf = emu.symbol(libc, "snprintf").unwrap();
    match emu.call(snprintf, &[buffer, 64, buffer + 128, 0x10]) {
        Err(runtime::Error::Crash(crash)) => Some(*crash),
        other => panic!("expected a crash, got {other:?}"),
    }
}

#[test]
fn crash_report_arm64() {
    let Some(crash) = crash_has_backtrace(Arch::Arm64) else { return };
    eprintln!("{crash}");
    assert!(matches!(crash.kind, runtime::StopKind::Fault { address: 0x10, write: false }));
    assert!(crash.precise_pc && crash.pc.contains("(strlen+"), "{}", crash.pc);
    assert!(crash.backtrace.iter().any(|f| f.contains("libc.so+")), "{:?}", crash.backtrace);
}

#[test]
fn crash_report_arm32() {
    let Some(crash) = crash_has_backtrace(Arch::Arm32) else { return };
    eprintln!("{crash}");
    assert!(matches!(crash.kind, runtime::StopKind::Fault { address: 0x10, write: false }));
    assert!(crash.precise_pc && crash.pc.contains("(strlen+"), "{}", crash.pc);
}
