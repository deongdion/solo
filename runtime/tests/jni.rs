//! JNI against unidbg: bench/jni/libjnifix.so registers natives in JNI_OnLoad and drives the JNI
//! functions from Native.run; bench/jni/expected.txt is the output a conforming VM produces.
//! It equals unidbg's arm64 output except for the lines where unidbg breaks the JNI
//! specification, listed in bench/jni/README.md.

mod common;

use std::path::PathBuf;

use backend::Arch;
use runtime::javavm::{Call, CallKind, Fail, JResult};
use runtime::{Emulator, Jni, Options, Value, Vm};

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..")
}

/// The Java side of the fixture, as JniRun.java implements it for unidbg.
struct Host;

fn mix(a: &[Value]) -> i32 {
    let (z, b, c, s, i, j, f, d) = (a[0].as_bool(), a[1].as_i32(), a[2].as_i32(), a[3].as_i32(), a[4].as_i32(), a[5].as_i64(), a[6].as_f64() as f32, a[7].as_f64());
    (z as i32)
        .wrapping_add(b.wrapping_mul(3))
        .wrapping_add(c.wrapping_mul(5))
        .wrapping_add(s.wrapping_mul(7))
        .wrapping_add(i.wrapping_mul(11))
        .wrapping_add((j % 1_000_003) as i32)
        .wrapping_add((f * 4.0) as i32)
        .wrapping_add((d * 8.0) as i32)
}

impl Jni for Host {
    fn call(&mut self, vm: &mut Vm, call: &Call) -> JResult<Value> {
        let a = call.args;
        Ok(match call.signature().as_str() {
            "fix/Host->add(IJ)J" => Value::Long(a[0].as_i64() + a[1].as_i64()),
            "fix/Host->mix(ZBCSIJFD)I" => Value::Int(mix(a)),
            "fix/Host->scale(FD)D" => Value::Double((a[0].as_f64() as f32) as f64 * a[1].as_f64()),
            "fix/Host->echo(Ljava/lang/String;)Ljava/lang/String;" => {
                let s = vm.string(a[0].object().ok_or(Fail::Unhandled)?).ok_or(Fail::Unhandled)?;
                Value::Object(vm.new_string(&format!("{s}!")))
            }
            "fix/Host->reverse([B)[B" => {
                let mut bytes = vm.bytes(a[0].object().ok_or(Fail::Unhandled)?).ok_or(Fail::Unhandled)?;
                bytes.reverse();
                Value::Object(vm.new_bytes(&bytes))
            }
            "fix/Host->boom()V" => return Err(Fail::Throw(vm.new_throwable("java/lang/IllegalStateException", Some("boom")))),
            "fix/Host-><init>(I)V" if call.kind == CallKind::Constructor => {
                let this = call.this.expect("constructor receiver");
                vm.heap.get_mut(this).unwrap().fields.insert("count".into(), a[0]);
                Value::Void
            }
            _ => return Err(Fail::Unhandled),
        })
    }
}

fn run(arch: Arch) -> Option<(String, Emulator)> {
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
    emu.vm.set_handler(Box::new(Host));
    let module = emu.load(fixture.to_str().unwrap()).unwrap_or_else(|e| panic!("load: {e}"));
    let version = emu.call_jni_onload(module).unwrap_or_else(|e| panic!("JNI_OnLoad: {e}\n{}", emu.summary()));
    assert_eq!(version, Some(0x0001_0006));

    let host_class = emu.vm.class("fix/Host");
    let name = emu.vm.new_string("fixture");
    emu.vm.class_info_mut(host_class).statics.insert("NAME".into(), Value::Object(name));
    let host = emu.vm.new_object(host_class);
    emu.vm.heap.get_mut(host).unwrap().fields.insert("count".into(), Value::Int(7));
    emu.vm.pin(host);

    let mut out = String::new();
    let call = |emu: &mut Emulator, name: &str, desc: &str, args: &[Value]| {
        emu.call_static("fix/Native", name, desc, args).unwrap_or_else(|e| panic!("{name}: {e}\n{}", emu.summary()))
    };
    let text = |emu: &Emulator, v: Value| emu.vm.string(v.object().expect("string result")).unwrap();
    let r = call(&mut emu, "run", "(Lfix/Host;)Ljava/lang/String;", &[Value::Object(host)]);
    out.push_str(&text(&emu, r));
    let who = emu.vm.new_string("wörld");
    let r = call(&mut emu, "hello", "(Ljava/lang/String;)Ljava/lang/String;", &[Value::Object(who)]);
    out.push_str(&format!("static {}\n", text(&emu, r)));
    let r = call(&mut emu, "twice", "(I)I", &[Value::Int(21)]);
    out.push_str(&format!("overload {}\n", r.as_i32()));
    let count = emu.vm.heap.get(host).unwrap().fields["count"];
    out.push_str(&format!("count after {}\n", count.as_i32()));
    Some((out, emu))
}

fn check(arch: Arch) {
    let Some((actual, emu)) = run(arch) else { return };
    let expected = std::fs::read_to_string(root().join("bench/jni/expected.txt")).unwrap();
    if actual != expected {
        for (i, (x, y)) in actual.lines().zip(expected.lines()).enumerate() {
            if x != y {
                eprintln!("line {}: got      [{x}]\n          expected [{y}]", i + 1);
            }
        }
        panic!("{arch:?}: output differs\n{actual}\n{}", emu.summary());
    }
    let summary = emu.summary();
    assert!(summary.unhandled_jni.is_empty(), "{summary}");
    assert!(summary.jni_timeline.iter().any(|l| l.starts_with("RegisterNatives fix/Native")), "{summary}");
    // Every buffer handed out was released.
    assert!(summary.jni_warnings.is_empty(), "{summary}");
}

#[test]
fn jni_arm64() {
    check(Arch::Arm64);
}

#[test]
fn jni_arm32() {
    check(Arch::Arm32);
}

#[test]
fn unhandled_callback_names_the_signature() {
    let Some((_, mut emu)) = run(Arch::Arm64) else { return };
    // Without the handler, the first Java call from run() is reported with its signature
    // (PLAN.md F1) instead of failing somewhere unrelated.
    emu.vm.set_handler(Box::new(NoJava));
    let host = emu.vm.new_object(emu.vm.find_class("fix/Host").unwrap());
    let error = emu.call_static("fix/Native", "run", "(Lfix/Host;)Ljava/lang/String;", &[Value::Object(host)]).unwrap_err();
    let message = error.to_string();
    assert!(message.contains("unhandled JNI callback: fix/Host->add(IJ)J"), "{message}");
    assert!(message.contains("libjnifix.so+"), "{message}");
    let summary = emu.summary();
    assert_eq!(summary.unhandled_jni.len(), 1, "{summary}");

    // With the Zero policy the run continues and every miss is counted. (Methods whose null
    // result the native dereferences stay handled.)
    emu.vm.set_handler(Box::new(WithoutMix));
    emu.vm.policy = runtime::UnhandledPolicy::Zero;
    let host = emu.vm.new_object(emu.vm.find_class("fix/Host").unwrap());
    emu.call_static("fix/Native", "run", "(Lfix/Host;)Ljava/lang/String;", &[Value::Object(host)]).unwrap();
    let summary = emu.summary();
    assert!(summary.unhandled_jni.iter().any(|(s, _, _)| s == "fix/Host->mix(ZBCSIJFD)I"), "{summary}");
    assert!(summary.unhandled_jni.iter().any(|(s, _, _)| s == "fix/Host->count:I"), "{summary}");
}

struct NoJava;
impl Jni for NoJava {}

struct WithoutMix;
impl Jni for WithoutMix {
    fn call(&mut self, vm: &mut Vm, call: &Call) -> JResult<Value> {
        if call.name == "mix" {
            return Err(Fail::Unhandled);
        }
        Host.call(vm, call)
    }
}
