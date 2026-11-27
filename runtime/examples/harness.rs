//! Harness template for driving a protected library's JNI entry point — the shape a signature or
//! attestation SDK takes, where one native dispatcher (SafetyNet/SG `doCommandNative`, a `sign` or
//! `encrypt` method, a `getToken`) is bound through `RegisterNatives` in `JNI_OnLoad`, reads device
//! facts by calling back into Java, and returns a token as a `String` or `byte[]`.
//!
//! `cargo run -p runtime --example harness -- <arm64|arm32>`
//!
//! To point this at your own library, change four things and nothing else:
//!   1. `LIBRARY` — the path to your `.so`.
//!   2. the `Jni` handler cases — one per Java method the native calls back into (device id, app
//!      signature, system properties); return the value your app's Java side would. Unhandled
//!      callbacks are listed in the end-of-run summary so you can see what the library wanted.
//!   3. the Java-side state the native reads (static fields, the receiver object's fields).
//!   4. the `call_static` / `call_method` line — the dispatcher's class, name and descriptor, and
//!      its arguments. Find the name with the `registered_natives()` dump this prints.
//!
//! Everything else — the device profile (a coherent Nexus 5X, F15), the fixed clock and random
//! seed (so a token that mixes in time or nonces is reproducible, F18/F19), and the summary of
//! missing stubs (F16) — is the production-realistic default and is what you usually want.
//!
//! The example runs against the bundled fixture `bench/jni/libjnifix.so`, whose `fix/Native.run`
//! native calls back into a `fix/Host` object (echo, reverse) and reads a static field, standing
//! in for the real dispatcher.

use std::path::PathBuf;

use backend::Arch;
use runtime::javavm::{Call, CallKind, Fail, JResult};
use runtime::{Emulator, Jni, Options, Profile, Value, Vm};

/// The Java methods the native calls back into. One arm per method the library invokes; return
/// what your app's Java would. `Fail::Unhandled` leaves a trace in the summary instead of guessing.
struct AppJava;

impl Jni for AppJava {
    fn call(&mut self, vm: &mut Vm, call: &Call) -> JResult<Value> {
        let a = call.args;
        Ok(match call.signature().as_str() {
            "fix/Host->add(IJ)J" => Value::Long(a[0].as_i64() + a[1].as_i64()),
            "fix/Host->mix(ZBCSIJFD)I" => Value::Int(a[1].as_i32() + a[4].as_i32()),
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
            // The native probes exception handling: throw, as the Java method would.
            "fix/Host->boom()V" => return Err(Fail::Throw(vm.new_throwable("java/lang/IllegalStateException", Some("boom")))),
            // The native constructs a Java object (new fix/Host(count)); store its field.
            "fix/Host-><init>(I)V" if call.kind == CallKind::Constructor => {
                let this = call.this.expect("constructor receiver");
                vm.heap.get_mut(this).unwrap().fields.insert("count".into(), a[0]);
                Value::Void
            }
            // Any other callback the library makes falls through to here and is reported.
            _ => return Err(Fail::Unhandled),
        })
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let arch = if args.get(1).map(String::as_str) == Some("arm32") { Arch::Arm32 } else { Arch::Arm64 };
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..");
    let (dir, lib) = if arch == Arch::Arm64 { ("arm64", "lib64") } else { ("arm32", "lib") };
    let library = root.join("bench/jni").join(dir).join("libjnifix.so");
    let profile_arch = if arch == Arch::Arm64 { device::profile::Arch::Arm64 } else { device::profile::Arch::Arm32 };

    let mut options = Options::new(arch);
    options.library_dirs = vec![root.join("bench/work/unidbg/unidbg-android/src/main/resources/android/sdk23").join(lib)];
    options.kernel.echo_output = false;
    // Reproducible: a fixed clock and a fixed RNG seed, so a token mixing in time or a nonce is the
    // same on every run (F18/F19).
    options.kernel.clock = runtime::ClockMode::Fixed { start_ns: 1_700_000_000_000_000_000, step_ns: 1000 };
    options.kernel.random_seed = Some(1);
    let mut emu = Emulator::new(options).expect("emulator");
    emu.vm.set_handler(Box::new(AppJava));

    // A coherent device fingerprint (F15): properties, /proc/cpuinfo and build fields all answer
    // from one Nexus 5X, so a library that cross-checks them sees no contradiction.
    let libc = emu.load("libc.so").expect("load libc");
    emu.install_device_profile(libc, Profile::nexus5x(profile_arch)).expect("device profile");

    // Load the target and run its JNI_OnLoad, which is where RegisterNatives binds the dispatcher.
    let module = emu.load(library.to_str().unwrap()).expect("load library");
    let version = emu.call_jni_onload(module).expect("JNI_OnLoad");
    println!("JNI_OnLoad reported version {version:x?}\n");

    // The entry points the library bound. The dispatcher you want to call is one of these; call it
    // by its class, name and descriptor with call_static (static native) or call_method (instance).
    println!("natives bound by RegisterNatives:");
    for (class, member, address) in emu.registered_natives() {
        println!("  {class}->{member}  @ {address:#x}");
    }
    println!();

    // The Java-side state the native reads: a static field and the receiver object's fields. Set
    // these to what your app would have in place before the call.
    let host_class = emu.vm.class("fix/Host");
    let name = emu.vm.new_string("fixture");
    emu.vm.class_info_mut(host_class).statics.insert("NAME".into(), Value::Object(name));
    let host = emu.vm.new_object(host_class);
    emu.vm.heap.get_mut(host).unwrap().fields.insert("count".into(), Value::Int(7));
    emu.vm.pin(host);

    // Call the dispatcher by name. For a real sign/doCommandNative this is where you pass the
    // payload (a byte[] via emu.vm.new_bytes) and read the token back.
    match emu.call_static("fix/Native", "run", "(Lfix/Host;)Ljava/lang/String;", &[Value::Object(host)]) {
        Ok(result) => {
            let token = emu.vm.string(result.object().expect("String result")).expect("string");
            println!("dispatcher returned:\n{token}");
        }
        Err(e) => println!("dispatcher failed: {e}"),
    }

    // Everything the run could not satisfy — missing symbols, syscalls, files, properties, JNI
    // members — so a token that came out wrong tells you which stub to add (F7/F16).
    println!("\n{}", emu.summary());
}
