//! Memory watch (Emulator::watch_memory) over bench/jni Native.hooked: the read of
//! fix_global_value, the bytes strlen reads, and writes to the stack.

mod common;

use std::path::PathBuf;

use backend::{Arch, Reg};
use runtime::{Emulator, Options, TraceConfig, Value};

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..")
}

fn setup(arch: Arch) -> Option<(Emulator, usize)> {
    let (dir, lib) = if arch == Arch::Arm64 { ("arm64", "lib64") } else { ("arm32", "lib") };
    let sdk = root().join("bench/work/unidbg/unidbg-android/src/main/resources/android/sdk23").join(lib);
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
    let module = emu.load(root().join("bench/jni").join(dir).join("libjnifix.so").to_str().unwrap()).unwrap();
    emu.call_jni_onload(module).unwrap();
    Some((emu, module))
}

fn hooked(emu: &mut Emulator) -> i32 {
    match emu.call_static("fix/Native", "hooked", "(II)I", &[Value::Int(2), Value::Int(3)]) {
        Ok(Value::Int(v)) => v,
        other => panic!("{other:?}"),
    }
}

fn run(arch: Arch) {
    let Some((mut emu, module)) = setup(arch) else { return };
    // Translated before the watch starts: the watch must still see these accesses.
    assert_eq!(hooked(&mut emu), 512345);
    let global = emu.linker.modules[module].static_symbol("fix_global_value").expect("fix_global_value");
    emu.watch_memory(global, global + 4).unwrap();
    assert_eq!(hooked(&mut emu), 512345);
    let accesses = emu.take_accesses();
    assert_eq!(accesses.len(), 1, "{accesses:x?}");
    let a = accesses[0];
    assert_eq!((a.address, a.size, a.write, a.value, a.pc), (global, 4, false, 1234, None));
    // Nothing else on the page is recorded, though the whole page takes the slow path.
    assert_eq!(hooked(&mut emu), 512345);
    assert_eq!(emu.take_accesses().len(), 1);

    // With an instruction trace on, each access names its instruction (inside fix_global).
    let fix_global = emu.symbol(module, "fix_global").unwrap() & !1;
    emu.start_trace(TraceConfig { capacity: 0, ..Default::default() });
    assert_eq!(hooked(&mut emu), 512345);
    emu.stop_trace();
    let accesses = emu.take_accesses();
    assert_eq!(accesses.len(), 1);
    let pc = accesses[0].pc.unwrap() & !1;
    assert!(pc >= fix_global && pc < fix_global + 0x20, "{pc:#x} {fix_global:#x}");
    emu.unwatch_memory();

    // Writes: the stack below the current SP. Values go through the slow path and must land.
    let sp = emu.cpu.reg(Reg::Sp);
    emu.watch_memory(sp - 0x1000, sp).unwrap();
    assert_eq!(hooked(&mut emu), 512345);
    let accesses = emu.take_accesses();
    let writes = accesses.iter().filter(|a| a.write).count();
    let reads = accesses.len() - writes;
    assert!(writes >= 2 && reads >= 2, "{writes} writes, {reads} reads");
    assert!(accesses.iter().all(|a| a.address >= sp - 0x1000 && a.address < sp));
    // A saved register is read back with the value written.
    let first = accesses.iter().find(|a| a.write).unwrap();
    assert!(accesses.iter().any(|a| !a.write && a.address == first.address && a.value == first.value));
    emu.unwatch_memory();
    assert_eq!(hooked(&mut emu), 512345);
    assert!(emu.take_accesses().is_empty());
}

/// Reads of the whole module during one call, in bytes: 52 on arm64, 36 on armv7, on both
/// backends and whether or not the code ran before the watch started. (dynarmic reports an
/// armv7 ldrd as one 8-byte access, unicorn as two 4-byte ones; hence bytes, not counts.)
/// unidbg's traceRead (unicorn2, bench/jni/TraceRun.java readcold) gives 44 and 32: the same
/// accesses without the second half of strlen's ldp/ldrd, which holds "hello"; set after
/// warm-up it gives none.
fn module_reads(arch: Arch) {
    for warm in [false, true] {
        let Some((mut emu, module)) = setup(arch) else { return };
        if warm {
            for _ in 0..1000 {
                hooked(&mut emu);
            }
        }
        let (start, end) = emu.module_range(module).unwrap();
        emu.watch_memory(start, end).unwrap();
        assert_eq!(hooked(&mut emu), 512345);
        let reads: Vec<_> = emu.take_accesses().into_iter().filter(|a| !a.write).collect();
        let bytes: u32 = reads.iter().map(|a| a.size).sum();
        assert_eq!(bytes, if arch == Arch::Arm64 { 52 } else { 36 }, "warm {warm}: {reads:x?}");
        let global = emu.linker.modules[module].static_symbol("fix_global_value").unwrap();
        assert!(reads.iter().any(|a| a.address == global && a.value == 1234));
    }
}

#[test]
fn module_reads_arm64() {
    module_reads(Arch::Arm64);
}

#[test]
fn module_reads_arm32() {
    module_reads(Arch::Arm32);
}

#[test]
fn watch_arm64() {
    run(Arch::Arm64);
}

#[test]
fn watch_arm32() {
    run(Arch::Arm32);
}

/// Cost of a watch on the page of fix_global_value and on the stack, against none.
/// `cargo test --release -p runtime --test watch -- --ignored --nocapture`
#[test]
#[ignore]
fn watch_cost() {
    for arch in [Arch::Arm64, Arch::Arm32] {
        let Some((mut emu, module)) = setup(arch) else { return };
        let global = emu.linker.modules[module].static_symbol("fix_global_value").unwrap();
        let sp = emu.cpu.reg(Reg::Sp);
        let n = 20_000;
        let time = |emu: &mut Emulator| {
            for _ in 0..1000 {
                hooked(emu);
            }
            let started = std::time::Instant::now();
            for _ in 0..n {
                hooked(emu);
            }
            let us = started.elapsed().as_secs_f64() * 1e6 / n as f64;
            let count = emu.take_accesses().len() as f64 / (n + 1000) as f64;
            (us, count)
        };
        let (plain, _) = time(&mut emu);
        emu.watch_memory(global, global + 4).unwrap();
        let (global_us, global_n) = time(&mut emu);
        emu.unwatch_memory();
        emu.watch_memory(sp - 0x1000, sp).unwrap();
        let (stack_us, stack_n) = time(&mut emu);
        emu.unwatch_memory();
        println!(
            "{arch:?}: none {plain:.2} us/call, global {global_us:.2} us/call ({global_n:.0}/call), stack {stack_us:.2} us/call ({stack_n:.0} accesses/call)"
        );
    }
}
