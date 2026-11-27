//! Hooks (bench/jni Native.hooked = fix_add(a, b) * 100000 + fix_global() * 10 + strlen("hello")):
//! inline hooks that observe, replace and change the return value, an import hook on strlen,
//! unhooking, a hook whose first instruction is PC-relative (fix_global on arm64), and hooks
//! surviving fork.

mod common;

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use backend::Arch;
use runtime::{Emulator, Flow, Hook, Options, Value};

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
    options.budget = 50_000_000;
    let mut emu = Emulator::new(options).unwrap();
    let module = emu.load(root().join("bench/jni").join(dir).join("libjnifix.so").to_str().unwrap()).unwrap();
    emu.call_jni_onload(module).unwrap();
    Some((emu, module))
}

fn hooked(emu: &mut Emulator, a: i32, b: i32) -> i32 {
    match emu.call_static("fix/Native", "hooked", "(II)I", &[Value::Int(a), Value::Int(b)]) {
        Ok(Value::Int(v)) => v,
        other => panic!("{other:?}\n{}", emu.summary()),
    }
}

fn run(arch: Arch) {
    let Some((mut emu, module)) = setup(arch) else { return };
    let plain = 5 * 100000 + 1234 * 10 + 5;
    assert_eq!(hooked(&mut emu, 2, 3), plain);

    // Observe the arguments and run the original.
    let seen = Arc::new(AtomicU64::new(0));
    let s = seen.clone();
    let observe = emu
        .hook_symbol(
            module,
            "fix_add",
            Hook::on_enter(move |emu, call| {
                let (a, b) = (call.arg(emu, 0)?, call.arg(emu, 1)?);
                s.store((a << 32) | b, Ordering::SeqCst);
                Ok(Flow::Continue)
            }),
        )
        .unwrap();
    assert_eq!(hooked(&mut emu, 2, 3), plain);
    assert_eq!(seen.load(Ordering::SeqCst), (2 << 32) | 3);
    // A second call still hits the hook (it was put back after the step).
    assert_eq!(hooked(&mut emu, 7, 8), 15 * 100000 + 1234 * 10 + 5);
    assert_eq!(seen.load(Ordering::SeqCst), (7 << 32) | 8);
    emu.unhook(observe).unwrap();

    // Replace: the original never runs.
    let replace = emu.hook_symbol(module, "fix_add", Hook::replace(|_, _| Ok(42))).unwrap();
    assert_eq!(hooked(&mut emu, 2, 3), 42 * 100000 + 1234 * 10 + 5);
    emu.unhook(replace).unwrap();
    assert_eq!(hooked(&mut emu, 2, 3), plain);

    // Leave: the original runs (its first instruction is PC-relative on arm64), the return
    // value is changed on the way out.
    let leave = emu.hook_symbol(module, "fix_global", Hook::on_leave(|_, call| {
        call.ret += 1;
        Ok(())
    })).unwrap();
    assert_eq!(hooked(&mut emu, 2, 3), 5 * 100000 + 1235 * 10 + 5);

    // Import hook on strlen, only for this module's calls.
    let calls = Arc::new(AtomicU64::new(0));
    let c = calls.clone();
    let import = emu
        .hook_import(
            module,
            "strlen",
            Hook::on_enter(move |_, _| {
                c.fetch_add(1, Ordering::SeqCst);
                Ok(Flow::Continue)
            })
            .and_leave(|_, call| {
                call.ret *= 2;
                Ok(())
            }),
        )
        .unwrap();
    assert_eq!(hooked(&mut emu, 2, 3), 5 * 100000 + 1235 * 10 + 10);
    assert_eq!(calls.load(Ordering::SeqCst), 1);

    // A fork carries the hooks; the template keeps its own.
    let mut copy = emu.fork().unwrap();
    assert_eq!(hooked(&mut copy, 2, 3), 5 * 100000 + 1235 * 10 + 10);
    emu.unhook(import).unwrap();
    emu.unhook(leave).unwrap();
    assert_eq!(hooked(&mut emu, 2, 3), plain);
    assert_eq!(hooked(&mut copy, 2, 3), 5 * 100000 + 1235 * 10 + 10);
    assert_eq!(calls.load(Ordering::SeqCst), 3);
    copy.restore(&emu).unwrap();
    assert_eq!(hooked(&mut copy, 2, 3), plain);
    assert!(emu.hook_import(module, "no_such_import", Hook::default()).is_err());

    // A callback may call guest code: fix_global replaced by a call to fix_add(40, 2).
    let add = emu.symbol(module, "fix_add").unwrap();
    let nested = emu.hook_symbol(module, "fix_global", Hook::replace(move |emu, _| emu.call(add, &[40, 2]))).unwrap();
    assert_eq!(hooked(&mut emu, 2, 3), 5 * 100000 + 42 * 10 + 5);
    emu.unhook(nested).unwrap();

    // fix_step's first instruction is a branch: the original runs by single step.
    let step = |emu: &mut Emulator, a: i32, b: i32| match emu.call_static("fix/Native", "step", "(II)I", &[Value::Int(a), Value::Int(b)]) {
        Ok(Value::Int(v)) => v,
        other => panic!("{other:?}"),
    };
    assert_eq!(step(&mut emu, 20, 22), 42);
    let entered = Arc::new(AtomicU64::new(0));
    let e = entered.clone();
    let stepped = emu
        .hook_symbol(
            module,
            "fix_step",
            Hook::on_enter(move |_, _| {
                e.fetch_add(1, Ordering::SeqCst);
                Ok(Flow::Continue)
            })
            .and_leave(|_, call| {
                call.ret += 100;
                Ok(())
            }),
        )
        .unwrap();
    assert_eq!(step(&mut emu, 20, 22), 142);
    assert_eq!(step(&mut emu, 1, 2), 103);
    assert_eq!(entered.load(Ordering::SeqCst), 2);
    emu.unhook(stepped).unwrap();
    assert_eq!(step(&mut emu, 20, 22), 42);
}

/// Cost per call of `Native.hooked` with no hook, observing inline hooks (the original's first
/// instruction run from a trampoline), a replacing hook, a leave hook and an import hook. `cargo test --release -- --ignored --nocapture`.
#[test]
#[ignore]
fn hook_cost() {
    for arch in [Arch::Arm64, Arch::Arm32] {
        let Some((mut emu, module)) = setup(arch) else { return };
        let time = |emu: &mut Emulator, label: &str| {
            for _ in 0..1000 {
                hooked(emu, 2, 3);
            }
            let n = 20000;
            let started = std::time::Instant::now();
            for _ in 0..n {
                hooked(emu, 2, 3);
            }
            println!("{arch:?} {label}: {:.2} us/call", started.elapsed().as_secs_f64() * 1e6 / n as f64);
        };
        time(&mut emu, "no hook");
        let id = emu.hook_symbol(module, "fix_add", Hook::on_enter(|_, _| Ok(Flow::Continue))).unwrap();
        time(&mut emu, "inline observe fix_add (trampoline)");
        emu.unhook(id).unwrap();
        let id = emu.hook_symbol(module, "fix_add", Hook::replace(|_, _| Ok(5))).unwrap();
        time(&mut emu, "inline replace");
        emu.unhook(id).unwrap();
        let id = emu.hook_symbol(module, "fix_add", Hook::on_leave(|_, _| Ok(()))).unwrap();
        time(&mut emu, "inline leave fix_add (trampoline + return stub)");
        emu.unhook(id).unwrap();
        let id = emu.hook_symbol(module, "fix_global", Hook::on_enter(|_, _| Ok(Flow::Continue))).unwrap();
        time(&mut emu, "inline observe fix_global (adrp on arm64: rewritten)");
        emu.unhook(id).unwrap();
        let id = emu.hook_symbol(module, "fix_step", Hook::on_enter(|_, _| Ok(Flow::Continue))).unwrap();
        let steps = |emu: &mut Emulator, label: &str| {
            let call = |emu: &mut Emulator| emu.call_static("fix/Native", "step", "(II)I", &[Value::Int(1), Value::Int(2)]).unwrap();
            for _ in 0..1000 {
                call(emu);
            }
            let n = 20000;
            let started = std::time::Instant::now();
            for _ in 0..n {
                call(emu);
            }
            println!("{arch:?} {label}: {:.2} us/call", started.elapsed().as_secs_f64() * 1e6 / n as f64);
        };
        steps(&mut emu, "Native.step, fix_step hooked (branch first: single step)");
        emu.unhook(id).unwrap();
        steps(&mut emu, "Native.step, no hook");
        let id = emu.hook_import(module, "strlen", Hook::on_enter(|_, _| Ok(Flow::Continue))).unwrap();
        time(&mut emu, "import observe");
        emu.unhook(id).unwrap();
    }
}

#[test]
fn hooks_arm64() {
    run(Arch::Arm64);
}

#[test]
fn hooks_arm32() {
    run(Arch::Arm32);
}
