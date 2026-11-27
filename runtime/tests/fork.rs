//! Emulator::fork: the copy runs the same natives with the same results and shares nothing.

mod common;

use std::path::PathBuf;
use std::time::Duration;

use backend::Arch;
use runtime::{Emulator, Options, Pool, PoolConfig, Value};

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
    options.budget = 1_000_000_000;
    let mut emu = Emulator::new(options).unwrap();
    let module = emu.load(fixture.to_str().unwrap()).unwrap();
    assert_eq!(emu.call_jni_onload(module).unwrap(), Some(0x0001_0006));
    Some(emu)
}

fn walk(emu: &mut Emulator) -> String {
    let words = ["zeta", "alpha", "beta", "gamma", "delta", "epsilon", "eta", "theta", "iota", "kappa"];
    let entries: Vec<(Value, Value)> =
        words.iter().enumerate().map(|(i, w)| (Value::Object(emu.vm.new_string(w)), Value::Int(i as i32))).collect();
    let map = java::new_map(&mut emu.vm, java::MapKind::Hash, &entries);
    let r = emu.call_static("fix/Native", "walk", "(Ljava/util/Map;)Ljava/lang/String;", &[Value::Object(map)]).unwrap();
    emu.vm.string(r.object().unwrap()).unwrap()
}

fn fork(arch: Arch) {
    let Some(mut emu) = emulator(arch) else { return };
    // Objects made before the fork exist in both, then diverge.
    let shared = emu.vm.new_string("before");
    emu.vm.pin(shared);
    let mut copy = emu.fork().unwrap();
    let expected = std::fs::read_to_string(root().join("bench/jni/expected_map.txt")).unwrap();
    assert_eq!(walk(&mut copy), expected);
    assert_eq!(walk(&mut emu), expected);
    assert_eq!(copy.vm.string(shared).unwrap(), "before");
    copy.vm.heap.get_mut(shared).unwrap().data = javavm::Data::String("after".encode_utf16().collect());
    assert_eq!(emu.vm.string(shared).unwrap(), "before");

    // Guest memory is copied, not shared.
    let module = emu.linker.modules.iter().find(|m| m.name.contains("libjnifix")).unwrap();
    let address = module.base;
    let original = emu.cpu.memory().read_u32(address).unwrap();
    copy.cpu.memory_mut().write_u32(address, !original).unwrap();
    assert_eq!(emu.cpu.memory().read_u32(address).unwrap(), original);

    // A fork of a fork still works, and the original keeps working after its forks are gone.
    let mut second = copy.fork().unwrap();
    drop(copy);
    let r = second.call_static("fix/Native", "spin", "(I)I", &[Value::Int(100)]).unwrap();
    assert_eq!(r, Value::Int(4950));
    drop(second);
    assert_eq!(walk(&mut emu), expected);
}

#[test]
fn fork_arm64() {
    fork(Arch::Arm64);
}

#[test]
fn fork_arm32() {
    fork(Arch::Arm32);
}

#[test]
fn pool_of_forks() {
    let Some(emu) = emulator(Arch::Arm64) else { return };
    let config = PoolConfig { workers: 3, job_timeout: Some(Duration::from_secs(10)), ..PoolConfig::default() };
    let pool = Pool::forking(config, emu).unwrap();
    let tickets: Vec<_> = (0..12).map(|_| pool.submit(|emu| Ok(walk(emu))).unwrap()).collect();
    let expected = std::fs::read_to_string(root().join("bench/jni/expected_map.txt")).unwrap();
    for t in tickets {
        assert_eq!(t.wait().unwrap(), expected);
    }
    assert_eq!(pool.stats().builds, 3);
}

/// A recycled worker is reset from the template and gives the template's results again.
#[test]
fn restore_resets_state() {
    let Some(template) = emulator(Arch::Arm64) else { return };
    let mut emu = template.fork().unwrap();
    let expected = walk(&mut emu);
    // Dirty the worker: guest memory, Java heap, a pending exception.
    let module = emu.linker.modules.iter().find(|m| m.name.contains("libjnifix")).unwrap().base;
    emu.cpu.memory_mut().write_u32(module, 0).unwrap();
    for i in 0..1000 {
        let s = emu.vm.new_string(&i.to_string());
        emu.vm.pin(s);
    }
    let live = emu.vm.heap.live();
    emu.restore(&template).unwrap();
    assert!(emu.vm.heap.live() < live);
    assert_eq!(emu.cpu.memory().read_u32(module).unwrap(), template.cpu.memory().read_u32(module).unwrap());
    assert_eq!(walk(&mut emu), expected);

    let config = PoolConfig { workers: 2, recycle_after_jobs: 3, job_timeout: Some(Duration::from_secs(10)), ..PoolConfig::default() };
    let mut pool = Pool::forking(config, template).unwrap();
    for _ in 0..12 {
        assert_eq!(pool.run(|emu| Ok(walk(emu))).unwrap(), expected);
    }
    pool.shutdown();
    let stats = pool.stats();
    assert_eq!(stats.builds, 2);
    assert_eq!(stats.resets, 4, "{stats:?}");
}
