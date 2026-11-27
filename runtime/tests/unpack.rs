//! Unpacker (Emulator::track_unpack, Emulator::unpack) over bench/jni libpacked.so: its init
//! function decrypts pk_code and clears the ELF magic in memory, and the exported unlock()
//! decrypts pk_data after the load. A dump must equal the unencrypted build byte for byte.

mod common;

use std::path::PathBuf;

use backend::Arch;
use runtime::{Emulator, Options, UnpackConfig};

fn fixture(arch: Arch, name: &str) -> PathBuf {
    let dir = if arch == Arch::Arm64 { "arm64" } else { "arm32" };
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../bench/jni").join(dir).join(name)
}

fn emulator(arch: Arch) -> Emulator {
    with_libraries(arch, Vec::new())
}

fn with_libraries(arch: Arch, library_dirs: Vec<PathBuf>) -> Emulator {
    let mut options = Options::new(arch);
    options.library_dirs = library_dirs;
    options.backend = common::backend();
    options.kernel.echo_output = false;
    options.budget = 10_000_000;
    Emulator::new(options).unwrap()
}

fn secret(x: i32) -> i32 {
    (0..x).fold(12345i32, |value, i| value.wrapping_mul(31).wrapping_add(i))
}

/// Changed runs between two files of the same length.
fn differences(a: &[u8], b: &[u8]) -> Vec<(usize, usize)> {
    let mut runs: Vec<(usize, usize)> = Vec::new();
    for i in (0..a.len()).filter(|&i| a[i] != b[i]) {
        match runs.last_mut() {
            Some((start, length)) if *start + *length == i => *length += 1,
            _ => runs.push((i, 1)),
        }
    }
    runs
}

fn run(arch: Arch) {
    let packed = std::fs::read(fixture(arch, "libpacked.so")).unwrap();
    let plain = std::fs::read(fixture(arch, "libpacked_plain.so")).unwrap();
    let encrypted = differences(&packed, &plain);
    assert_eq!(encrypted.len(), 2, "pk_data and pk_code: {encrypted:x?}");
    let (data, code) = (encrypted[0], encrypted[1]);

    let mut emu = emulator(arch);
    emu.track_unpack("libpacked.so");
    let module = emu.load(fixture(arch, "libpacked.so").to_str().unwrap()).unwrap();
    let call = |emu: &mut Emulator, name: &str, args: &[u64]| {
        let function = emu.symbol(module, name).unwrap();
        emu.call(function, args).unwrap()
    };
    assert_eq!(call(&mut emu, "secret", &[7]) as i32, secret(7));

    // After the init function: the code is decrypted, the data is not yet.
    let first = emu.unpack(module, UnpackConfig::default()).unwrap();
    assert_eq!(first.header_bytes_kept, 4, "the cleared ELF magic stays as in the file");
    assert_eq!(first.patched, vec![(code.0 as u64, code.1 as u64)]);
    assert_eq!(differences(&first.file, &plain), vec![data]);

    // A call after the load decrypts the data; a later dump has it too.
    let message = call(&mut emu, "unlock", &[]);
    let text = emu.cpu.memory().read_vec(message, 66).unwrap();
    assert_eq!(&text[..], b"decrypted after load by an exported call, not by an init function\0");
    let second = emu.unpack(module, UnpackConfig::default()).unwrap();
    assert_eq!(second.patched, vec![(data.0 as u64, data.1 as u64), (code.0 as u64, code.1 as u64)]);
    assert!(second.file == plain, "the dump equals the unencrypted build");
    // Relocated words are in the baseline, so writable segments add nothing here.
    let all = emu.unpack(module, UnpackConfig { writable: true }).unwrap();
    assert!(all.file == plain);

    // Forks keep the baseline.
    let fork = emu.fork().unwrap();
    assert!(fork.unpack(module, UnpackConfig::default()).unwrap().file == plain);

    // A module loaded without tracking has no baseline.
    let mut untracked = emulator(arch);
    let module = untracked.load(fixture(arch, "libpacked.so").to_str().unwrap()).unwrap();
    assert!(untracked.unpack(module, UnpackConfig::default()).is_err());
}

#[test]
fn unpack_arm64() {
    run(Arch::Arm64);
}

#[test]
fn unpack_arm32() {
    run(Arch::Arm32);
}

/// Cost on a full library (sdk23 libc.so, loaded as libjnifix.so's dependency, init functions
/// included): the load with and without tracking, and one dump. Nothing runs while the guest
/// executes. `cargo test --release -p runtime --test unpack -- --ignored --nocapture`
#[test]
#[ignore]
fn unpack_cost() {
    for arch in [Arch::Arm64, Arch::Arm32] {
        let lib = if arch == Arch::Arm64 { "lib64" } else { "lib" };
        let sdk = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../bench/work/unidbg/unidbg-android/src/main/resources/android/sdk23")
            .join(lib);
        if !sdk.is_dir() {
            eprintln!("sdk23 libraries not present; skipped");
            return;
        }
        let rounds = 20;
        let mut plain_ms = 0.0;
        let mut tracked_ms = 0.0;
        let mut dump_ms = 0.0;
        let mut patched = 0;
        for _ in 0..rounds {
            let mut emu = with_libraries(arch, vec![sdk.clone()]);
            emu.track_unpack("libc.so");
            let started = std::time::Instant::now();
            emu.load(fixture(arch, "libjnifix.so").to_str().unwrap()).unwrap();
            tracked_ms += started.elapsed().as_secs_f64() * 1e3;
            let libc = emu.linker.find("libc.so").unwrap();
            let started = std::time::Instant::now();
            let dump = emu.unpack(libc, UnpackConfig::default()).unwrap();
            dump_ms += started.elapsed().as_secs_f64() * 1e3;
            patched = dump.patched.iter().map(|(_, length)| length).sum::<u64>();

            let mut emu = with_libraries(arch, vec![sdk.clone()]);
            let started = std::time::Instant::now();
            emu.load(fixture(arch, "libjnifix.so").to_str().unwrap()).unwrap();
            plain_ms += started.elapsed().as_secs_f64() * 1e3;
        }
        let n = rounds as f64;
        println!(
            "{arch:?}: load {:.2} ms, load tracking libc.so {:.2} ms, dump {:.2} ms ({patched} bytes patched)",
            plain_ms / n,
            tracked_ms / n,
            dump_ms / n
        );
    }
}
