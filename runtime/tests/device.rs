//! Device profile (PLAN.md F15): the guest reads its properties through bionic's own
//! `__system_property_get` and `/proc/cpuinfo` through `open`/`read`, both answering from one
//! coherent Nexus 5X profile; `Profile::check` finds no contradiction.

mod common;

use std::path::PathBuf;

use backend::Arch;
use device::profile;
use runtime::{Emulator, Options, Profile};

fn emulator(arch: Arch) -> Option<(Emulator, usize)> {
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
    options.budget = 200_000_000;
    let mut emu = Emulator::new(options).unwrap();
    let libc = emu.load("libc.so").unwrap();
    Some((emu, libc))
}

/// `__system_property_get(name, value)`: returns the value string, via a 128-byte scratch buffer.
fn property(emu: &mut Emulator, libc: usize, name: &str) -> String {
    let malloc = emu.symbol(libc, "malloc").unwrap();
    let name_ptr = emu.call(malloc, &[256]).unwrap();
    let value_ptr = emu.call(malloc, &[128]).unwrap();
    let mut bytes = name.as_bytes().to_vec();
    bytes.push(0);
    emu.cpu.memory_mut().write(name_ptr, &bytes).unwrap();
    let get = emu.symbol(libc, "__system_property_get").unwrap();
    let len = emu.call(get, &[name_ptr, value_ptr]).unwrap();
    let got = emu.cpu.memory().read_cstr(value_ptr, 128).unwrap();
    assert_eq!(len as usize, got.len(), "returned length matches the string");
    String::from_utf8(got).unwrap()
}

/// Reads a vfs file through open/read (the path the guest takes to /proc/cpuinfo).
fn read_file(emu: &mut Emulator, libc: usize, path: &str) -> String {
    let malloc = emu.symbol(libc, "malloc").unwrap();
    let path_ptr = emu.call(malloc, &[256]).unwrap();
    let buf = emu.call(malloc, &[8192]).unwrap();
    let mut bytes = path.as_bytes().to_vec();
    bytes.push(0);
    emu.cpu.memory_mut().write(path_ptr, &bytes).unwrap();
    let open = emu.symbol(libc, "open").unwrap();
    let fd = emu.call(open, &[path_ptr, 0]).unwrap();
    assert!(fd < 0x8000_0000, "open {path} returned {fd:#x}");
    let read = emu.symbol(libc, "read").unwrap();
    let n = emu.call(read, &[fd, buf, 8192]).unwrap();
    let data = emu.cpu.memory().read_vec(buf, n as usize).unwrap();
    String::from_utf8_lossy(&data).into_owned()
}

/// open() a path, returning the fd (which may be a negative errno); does not read.
fn read_or_fail(emu: &mut Emulator, libc: usize, path: &str) -> u64 {
    let malloc = emu.symbol(libc, "malloc").unwrap();
    let path_ptr = emu.call(malloc, &[256]).unwrap();
    let mut bytes = path.as_bytes().to_vec();
    bytes.push(0);
    emu.cpu.memory_mut().write(path_ptr, &bytes).unwrap();
    let open = emu.symbol(libc, "open").unwrap();
    emu.call(open, &[path_ptr, 0]).unwrap()
}

fn run(arch: Arch) {
    let Some((mut emu, libc)) = emulator(arch) else { return };
    let parch = if arch == Arch::Arm64 { profile::Arch::Arm64 } else { profile::Arch::Arm32 };
    let p = Profile::nexus5x(parch);
    assert_eq!(p.check(parch), vec![], "preset is consistent");
    emu.install_device_profile(libc, p).unwrap();

    assert_eq!(property(&mut emu, libc, "ro.product.model"), "Nexus 5X");
    assert_eq!(property(&mut emu, libc, "ro.build.version.sdk"), "23");
    assert_eq!(
        property(&mut emu, libc, "ro.build.fingerprint"),
        "google/bullhead/bullhead:6.0/MDA89E/2296692:user/release-keys"
    );
    let abi = property(&mut emu, libc, "ro.product.cpu.abi");
    assert_eq!(abi, if arch == Arch::Arm64 { "arm64-v8a" } else { "armeabi-v7a" });
    // An unset property returns the empty string and length 0 (bionic's contract), and the read
    // is recorded for the F16 report so a missing stub does not fail silently.
    assert_eq!(property(&mut emu, libc, "ro.nonexistent.key"), "");
    let summary = emu.summary();
    assert!(
        summary.missing_properties.iter().any(|(k, _)| k == "ro.nonexistent.key"),
        "missing property not reported: {:?}",
        summary.missing_properties
    );

    // Opening a file that does not exist is also recorded.
    let _ = read_or_fail(&mut emu, libc, "/data/data/com.example/does_not_exist");
    let summary = emu.summary();
    assert!(
        summary.missing_files.iter().any(|(p, _)| p.contains("does_not_exist")),
        "missing file not reported: {:?}",
        summary.missing_files
    );

    let cpuinfo = read_file(&mut emu, libc, "/proc/cpuinfo");
    assert!(cpuinfo.contains("Hardware\t: Qualcomm Technologies, Inc MSM8992"), "{cpuinfo}");
    assert_eq!(cpuinfo.matches("processor\t:").count(), 6);
    // No emulator tells in the CPU info (goldfish/ranchu name the Android emulator).
    for tell in ["goldfish", "ranchu", "Goldfish"] {
        assert!(!cpuinfo.contains(tell), "cpuinfo names {tell}");
    }
    // TracerPid is 0: the process is not being traced.
    let status = read_file(&mut emu, libc, "/proc/self/status");
    assert!(status.contains("TracerPid:\t0"), "{status}");

    // ptrace(PTRACE_TRACEME) succeeds (returns 0), as on a real undebugged process: an anti-debug
    // check that reads a failure as "already traced" must not fire, and it is not left unhandled.
    let nr_ptrace = if arch == Arch::Arm64 { 117 } else { 26 };
    let syscall = emu.symbol(libc, "syscall").unwrap();
    assert_eq!(emu.call(syscall, &[nr_ptrace, 0, 0, 0, 0]).unwrap(), 0);
    assert!(
        !emu.summary().unhandled_syscalls.iter().any(|(n, ..)| n.contains("ptrace") || n.contains("117") || n.contains("26")),
        "ptrace left unhandled"
    );

    // The profile survives a fork.
    let mut fork = emu.fork().unwrap();
    assert_eq!(property(&mut fork, libc, "ro.product.model"), "Nexus 5X");
}

#[test]
fn device_arm64() {
    run(Arch::Arm64);
}

#[test]
fn device_arm32() {
    run(Arch::Arm32);
}
