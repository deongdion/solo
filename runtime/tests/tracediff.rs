//! Device trace diff (PLAN.md F17): record what the emulator executes for a call, treat that as
//! the trace a real device produced, and show that re-running the same call matches it step for
//! step while a single altered step is pinpointed. The comparison is module-relative, so a device
//! trace taken at different load addresses still lines up.

mod common;

use std::path::PathBuf;

use backend::Arch;
use runtime::{Emulator, Options, Step, TraceConfig, Value};

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

/// Calls Native.hooked(2, 3) while tracing only libjnifix.so, and returns the module-relative trace.
fn traced_call(emu: &mut Emulator, module: usize) -> Vec<Step> {
    let (start, end) = emu.module_range(module).unwrap();
    emu.start_trace(TraceConfig { ranges: vec![(start, end)], capacity: 10_000, registers: false, sink: None });
    let r = emu.call_static("fix/Native", "hooked", "(II)I", &[Value::Int(2), Value::Int(3)]).unwrap();
    assert_eq!(r, Value::Int(512345));
    let steps = emu.executed_trace();
    emu.stop_trace();
    steps
}

fn run(arch: Arch) {
    let Some((mut emu, module)) = setup(arch) else { return };

    // The "device" trace: what the call executes, as module+offset, so another run at a different
    // load address can be compared against it.
    let reference = traced_call(&mut emu, module);
    assert!(!reference.is_empty(), "nothing traced");
    assert!(reference.iter().all(|s| s.module == "libjnifix.so"));

    // A round trip through the text log format the device side would hand us preserves it.
    let text = inspect::tracediff::to_text(&reference);
    assert_eq!(inspect::tracediff::parse(&text).unwrap(), reference);

    // A freshly loaded emulator (its own load addresses) follows the device trace exactly. The
    // trace is left running so `diff_trace` reads what this run just executed.
    let Some((mut again, module2)) = setup(arch) else { return };
    let (start, end) = again.module_range(module2).unwrap();
    again.start_trace(TraceConfig { ranges: vec![(start, end)], capacity: 10_000, registers: false, sink: None });
    let r = again.call_static("fix/Native", "hooked", "(II)I", &[Value::Int(2), Value::Int(3)]).unwrap();
    assert_eq!(r, Value::Int(512345));
    assert_eq!(again.diff_trace(&reference), None, "re-run diverged from the device trace");

    // Flip one recorded step: the diff points exactly there and nowhere earlier.
    let mut altered = reference.clone();
    let at = altered.len() / 2;
    altered[at] = Step::new("libjnifix.so", altered[at].offset ^ 0x4);
    let d = again.diff_trace(&altered).expect("altered trace must diverge");
    assert_eq!(d.index, at, "divergence not reported at the altered step");
    assert_eq!(d.reference.unwrap(), altered[at]);
    assert_eq!(d.actual.unwrap(), reference[at]);
}

#[test]
fn tracediff_arm64() {
    run(Arch::Arm64);
}

#[test]
fn tracediff_arm32() {
    run(Arch::Arm32);
}
