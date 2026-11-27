//! Instruction trace (inspect::trace) over bench/jni Native.hooked, limited to libjnifix.so.

mod common;

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use backend::Arch;
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
    let (start, end) = emu.module_range(module).unwrap();
    let lines = Arc::new(AtomicU64::new(0));
    let l = lines.clone();
    emu.start_trace(TraceConfig {
        ranges: vec![(start, end)],
        capacity: 10_000,
        registers: true,
        sink: Some(Arc::new(move |_| {
            l.fetch_add(1, Ordering::Relaxed);
        })),
    });
    assert_eq!(hooked(&mut emu), 512345);
    let trace = emu.stop_trace().unwrap();
    // The same count unidbg's traceCode gives when set before the first call
    // (bench/jni/TraceRun.java cold): 36 on arm64, 37 on armv7.
    assert_eq!(trace.recorded, if arch == Arch::Arm64 { 36 } else { 37 });
    assert_eq!(lines.load(Ordering::Relaxed), trace.recorded);
    assert!(trace.stepped > trace.recorded, "libc's strlen and the JNI layer run outside the range");
    assert!(trace.ring.iter().all(|e| (e.pc & !1) >= start && (e.pc & !1) < end));
    let entry = emu.symbol(module, "Java_fix_Native_hooked").unwrap();
    let add = emu.symbol(module, "fix_add").unwrap();
    assert_eq!(trace.ring.front().unwrap().pc, entry);
    let at_add = trace.ring.iter().position(|e| e.pc == add).expect("fix_add traced");
    // fix_add(2, 3) leaves 5 in r0/x0 before its return.
    assert!(trace.ring.iter().skip(at_add).take(8).any(|e| e.changed.contains(&(0, 5))), "x0 = 5 recorded");
    let line = emu.format_executed(trace.ring.front().unwrap());
    assert!(line.starts_with("[libjnifix.so 0x"), "{line}");
    assert!(line.contains("push") || line.contains("stp"), "{line}");
    // Off again: runs at full speed and records nothing.
    assert_eq!(hooked(&mut emu), 512345);
    assert!(emu.trace().is_none());
}

#[test]
fn trace_arm64() {
    run(Arch::Arm64);
}

#[test]
fn trace_arm32() {
    run(Arch::Arm32);
}

/// Stepping speed with a trace on, everything recorded, against the same calls untraced.
/// `cargo test --release -p runtime --test trace -- --ignored --nocapture`
#[test]
#[ignore]
fn trace_cost() {
    for arch in [Arch::Arm64, Arch::Arm32] {
        let Some((mut emu, module)) = setup(arch) else { return };
        for _ in 0..1000 {
            hooked(&mut emu);
        }
        let n = 2000;
        let started = std::time::Instant::now();
        for _ in 0..n {
            hooked(&mut emu);
        }
        let plain = started.elapsed().as_secs_f64() * 1e6 / n as f64;
        emu.start_trace(TraceConfig { capacity: 4096, ..Default::default() });
        let started = std::time::Instant::now();
        for _ in 0..n {
            hooked(&mut emu);
        }
        let traced = started.elapsed().as_secs_f64() * 1e6 / n as f64;
        let trace = emu.stop_trace().unwrap();
        let per_call = trace.stepped as f64 / n as f64;
        emu.start_trace(TraceConfig { ranges: vec![emu.module_range(module).unwrap()], ..Default::default() });
        hooked(&mut emu);
        let in_module = emu.stop_trace().unwrap().recorded;
        println!("{arch:?}: {in_module} instructions recorded in libjnifix.so per call (after warm-up)");
        println!(
            "{arch:?}: untraced {plain:.2} us/call, traced {traced:.1} us/call, {per_call:.0} instructions/call, {:.2} M instructions/s traced",
            per_call / traced
        );
    }
}
