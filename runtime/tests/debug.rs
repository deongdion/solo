//! Debugger (runtime::debug) over bench/jni Native.hooked: breakpoints, steps, register and
//! memory access from the callback, abort.

mod common;

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use backend::{Arch, Reg};
use runtime::{DebugEvent, Emulator, Error, Options, Resume, Value};

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

fn hooked(emu: &mut Emulator) -> Result<i32, Error> {
    match emu.call_static("fix/Native", "hooked", "(II)I", &[Value::Int(2), Value::Int(3)]) {
        Ok(Value::Int(v)) => Ok(v),
        Ok(other) => panic!("{other:?}"),
        Err(e) => Err(e),
    }
}

fn run(arch: Arch) {
    let Some((mut emu, module)) = setup(arch) else { return };
    let add = emu.symbol(module, "fix_add").unwrap();
    let entry = emu.symbol(module, "Java_fix_Native_hooked").unwrap();
    assert_eq!(hooked(&mut emu).unwrap(), 512345);
    let original = emu.cpu.memory().read_vec(add & !1, 4).unwrap();

    // No debugger: a breakpoint fails the call.
    emu.add_breakpoint(add).unwrap();
    assert!(matches!(hooked(&mut emu), Err(Error::Breakpoint(pc)) if pc == add));
    assert_ne!(emu.cpu.memory().read_vec(add & !1, 4).unwrap(), original);
    assert_eq!(emu.read_memory(add & !1, 4).unwrap(), original);

    // At fix_add: the arguments, steps until it returned, x0 changed to 10 there, continue.
    let events = Arc::new(Mutex::new(Vec::new()));
    let log = events.clone();
    let (low, high) = (add & !1, (add & !1) + 8);
    emu.set_debugger(Box::new(move |emu: &mut Emulator, event: DebugEvent| {
        log.lock().unwrap().push((event, emu.cpu.reg(Reg::General(0)), emu.cpu.reg(Reg::General(1))));
        match event {
            DebugEvent::Step(pc) if !(low..high).contains(&(pc & !1)) => {
                emu.cpu.set_reg(Reg::General(0), 10);
                Resume::Continue
            }
            _ => Resume::Step,
        }
    }));
    let result = hooked(&mut emu).unwrap();
    let events = events.lock().unwrap().clone();
    assert_eq!(events[0], (DebugEvent::Breakpoint(add), 2, 3));
    // fix_add is `add w0, w0, w1; ret` on arm64, `push; mov r7, sp; add r0, r1; pop {r7, pc}` in
    // Thumb: one step per instruction, then the return address in the caller.
    assert_eq!(events.len(), if arch == Arch::Arm64 { 3 } else { 5 }, "{events:x?}");
    assert!(events[1..events.len() - 1].iter().all(|(e, ..)| matches!(e, DebugEvent::Step(pc) if (low..high).contains(&(pc & !1)))));
    assert_eq!(events.last().unwrap().1, 5, "the add ran");
    // The caller multiplies the changed return value.
    assert_eq!(result, 10 * 100000 + 12345, "{events:x?}");

    // Removed: runs as before, original bytes back.
    assert!(emu.remove_breakpoint(add).unwrap());
    assert_eq!(emu.cpu.memory().read_vec(add & !1, 4).unwrap(), original);
    assert_eq!(hooked(&mut emu).unwrap(), 512345);

    // break_next: the next call stops before its first instruction, at the native method.
    let first = Arc::new(Mutex::new(None));
    let f = first.clone();
    emu.set_debugger(Box::new(move |_: &mut Emulator, event: DebugEvent| {
        f.lock().unwrap().get_or_insert(event);
        Resume::Continue
    }));
    emu.break_next();
    assert_eq!(hooked(&mut emu).unwrap(), 512345);
    assert_eq!(first.lock().unwrap().unwrap(), DebugEvent::Step(entry));

    // Continue over each displacement form, three calls each: fix_add's first instruction runs
    // from a stub, fix_global's (adrp on arm64) is computed, strlen's import stub in the PLT
    // (PC-relative) is stepped in place.
    let strlen = emu.linker.modules[module].imports.iter().find(|(_, n)| n == "strlen").map(|(slot, _)| *slot).unwrap();
    let strlen = if arch == Arch::Arm64 { emu.cpu.memory().read_u64(strlen).unwrap() } else { emu.cpu.memory().read_u32(strlen).unwrap() as u64 };
    for site in [add, emu.symbol(module, "fix_global").unwrap(), strlen] {
        let hits = Arc::new(Mutex::new(0));
        let h = hits.clone();
        emu.set_debugger(Box::new(move |_: &mut Emulator, event: DebugEvent| {
            assert_eq!(event, DebugEvent::Breakpoint(site));
            *h.lock().unwrap() += 1;
            Resume::Continue
        }));
        emu.add_breakpoint(site).unwrap();
        for _ in 0..3 {
            assert_eq!(hooked(&mut emu).unwrap(), 512345, "{site:#x}");
        }
        emu.remove_breakpoint(site).unwrap();
        assert_eq!(*hits.lock().unwrap(), 3, "{site:#x}");
    }

    // Abort fails the call; the emulator runs the next one normally.
    emu.add_breakpoint(add).unwrap();
    emu.set_debugger(Box::new(|_: &mut Emulator, _| Resume::Abort));
    assert!(matches!(hooked(&mut emu), Err(Error::Aborted)));
    emu.remove_breakpoint(add).unwrap();
    assert_eq!(hooked(&mut emu).unwrap(), 512345);
}

#[test]
fn debug_arm64() {
    run(Arch::Arm64);
}

#[test]
fn debug_arm32() {
    run(Arch::Arm32);
}

/// Cost of a breakpoint hit whose debugger continues at once, per call of Native.hooked.
/// `cargo test --release -p runtime --test debug -- --ignored --nocapture`
#[test]
#[ignore]
fn breakpoint_cost() {
    for arch in [Arch::Arm64, Arch::Arm32] {
        let Some((mut emu, module)) = setup(arch) else { return };
        let add = emu.symbol(module, "fix_add").unwrap();
        let n = 20_000;
        let time = |emu: &mut Emulator| {
            for _ in 0..1000 {
                hooked(emu).unwrap();
            }
            let started = std::time::Instant::now();
            for _ in 0..n {
                hooked(emu).unwrap();
            }
            started.elapsed().as_secs_f64() * 1e6 / n as f64
        };
        let plain = time(&mut emu);
        emu.set_debugger(Box::new(|_: &mut Emulator, _| Resume::Continue));
        emu.add_breakpoint(add).unwrap();
        let hit = time(&mut emu);
        println!("{arch:?}: no breakpoint {plain:.2} us/call, breakpoint hit and continued {hit:.2} us/call");
    }
}
