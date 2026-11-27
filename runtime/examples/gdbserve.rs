//! Serves bench/jni Native.hooked(2, 3) to a gdb or lldb client, stopped before its first
//! instruction. `cargo run -p runtime --example gdbserve -- <arm64|arm32> <port>`, then
//! `lldb -o "gdb-remote <port>"` or gdb's `target remote :<port>`.

use std::path::PathBuf;

use backend::Arch;
use runtime::{Emulator, GdbStub, Options, Value};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let arch = if args.get(1).map(String::as_str) == Some("arm32") { Arch::Arm32 } else { Arch::Arm64 };
    let port = args.get(2).map(String::as_str).unwrap_or("1234");
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..");
    let (dir, lib) = if arch == Arch::Arm64 { ("arm64", "lib64") } else { ("arm32", "lib") };
    let mut options = Options::new(arch);
    options.library_dirs = vec![root.join("bench/work/unidbg/unidbg-android/src/main/resources/android/sdk23").join(lib)];
    options.kernel.echo_output = false;
    let mut emu = Emulator::new(options).expect("emulator");
    let module = emu.load(root.join("bench/jni").join(dir).join("libjnifix.so").to_str().unwrap()).expect("load");
    emu.call_jni_onload(module).expect("JNI_OnLoad");
    for name in ["Java_fix_Native_hooked", "fix_add"] {
        println!("{name} {:#x}", emu.symbol(module, name).unwrap());
    }
    let stub = GdbStub::bind(format!("127.0.0.1:{port}")).expect("bind");
    println!("listening on {}", stub.local_addr().unwrap());
    emu.set_debugger(Box::new(stub));
    emu.break_next();
    let result = emu.call_static("fix/Native", "hooked", "(II)I", &[Value::Int(2), Value::Int(3)]);
    println!("result {result:?}");
    drop(emu.take_debugger());
}
