//! Byte-for-byte parity with unidbg: bench/parity/libparity.so exercises bionic and writes every
//! result into a buffer; bench/parity/expected.txt is what unidbg 30911d7 produced for it with
//! both of its backends on both architectures (see bench/parity/README.md).

use std::path::PathBuf;

use backend::Arch;
use runtime::{Emulator, Options};

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..")
}

fn run(arch: Arch, file: &str) -> Option<Vec<u8>> {
    let (dir, lib) = if arch == Arch::Arm64 { ("arm64", "lib64") } else { ("arm32", "lib") };
    let fixture = root().join("bench/parity").join(dir).join(file);
    let sdk = root().join("bench/work/unidbg/unidbg-android/src/main/resources/android/sdk23").join(lib);
    if !sdk.is_dir() {
        eprintln!("sdk23 libraries not present; skipped");
        return None;
    }
    let mut options = Options::new(arch);
    options.library_dirs = vec![sdk];
    options.kernel.echo_output = false;
    options.budget = 1_000_000_000;
    let mut emu = Emulator::new(options).unwrap();
    let module = match emu.load(fixture.to_str().unwrap()) {
        Ok(m) => m,
        Err(e) => panic!("load {file}: {e}\n{}", emu.summary()),
    };
    let libc = emu.linker.find("libc.so").unwrap();
    let malloc = emu.symbol(libc, "malloc").unwrap();
    let size = 16384;
    let buffer = emu.call(malloc, &[size]).unwrap();
    let run = emu.symbol(module, "parity_run").unwrap();
    let length = match emu.call(run, &[buffer, size]) {
        Ok(n) => n as usize,
        Err(e) => panic!("parity_run: {e}\n{}", emu.summary()),
    };
    let mut out = emu.cpu.memory().read_vec(buffer, length).unwrap();
    out.extend_from_slice(format!("return {length}\n").as_bytes());
    Some(out)
}

fn check(arch: Arch, file: &str) {
    let Some(actual) = run(arch, file) else { return };
    let expected = std::fs::read(root().join("bench/parity/expected.txt")).unwrap();
    if actual != expected {
        let a = String::from_utf8_lossy(&actual);
        let e = String::from_utf8_lossy(&expected);
        for (i, (x, y)) in a.lines().zip(e.lines()).enumerate() {
            if x != y {
                eprintln!("line {}: got      [{x}]\n          expected [{y}]", i + 1);
            }
        }
        panic!("{arch:?} {file}: output differs from unidbg ({} vs {} bytes)", actual.len(), expected.len());
    }
}

#[test]
fn parity_arm64() {
    check(Arch::Arm64, "libparity.so");
}

#[test]
fn parity_arm32() {
    check(Arch::Arm32, "libparity.so");
}

/// GNU_HASH only (no DT_HASH), as current NDKs emit. unidbg produces the same text for it.
#[test]
fn parity_gnu_hash_arm64() {
    check(Arch::Arm64, "libparitygnu.so");
}

#[test]
fn parity_gnu_hash_arm32() {
    check(Arch::Arm32, "libparitygnu.so");
}

/// No DT_REL/DT_RELENT, only DT_JMPREL. unidbg cannot bind this library's armv7 PLT (it assumes
/// 12-byte entries when DT_RELENT is absent); the expected text is unchanged.
#[test]
fn parity_plt_only_arm64() {
    check(Arch::Arm64, "libparityplt.so");
}

#[test]
fn parity_plt_only_arm32() {
    check(Arch::Arm32, "libparityplt.so");
}
