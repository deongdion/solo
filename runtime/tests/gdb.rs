//! GDB remote protocol (runtime::GdbStub) driven by a minimal client over TCP, on bench/jni
//! Native.hooked: stop at the first instruction, target.xml, registers, memory under a
//! breakpoint, Z0/z0, continue, step, register write, detach.

mod common;

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;

use backend::Arch;
use runtime::{Emulator, GdbStub, Options, Value};

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

struct Client {
    stream: TcpStream,
    ack: bool,
}

impl Client {
    fn byte(&mut self) -> u8 {
        let mut b = [0u8; 1];
        self.stream.read_exact(&mut b).unwrap();
        b[0]
    }

    fn ask(&mut self, payload: &str) -> String {
        let sum = payload.bytes().fold(0u8, |a, b| a.wrapping_add(b));
        self.stream.write_all(format!("${payload}#{sum:02x}").as_bytes()).unwrap();
        if self.ack {
            assert_eq!(self.byte(), b'+', "ack for {payload}");
        }
        while self.byte() != b'$' {}
        let mut data = Vec::new();
        loop {
            let b = self.byte();
            if b == b'#' {
                break;
            }
            data.push(b);
        }
        let (h, l) = (self.byte(), self.byte());
        let sum = data.iter().fold(0u8, |a, b| a.wrapping_add(*b));
        assert_eq!(u8::from_str_radix(std::str::from_utf8(&[h, l]).unwrap(), 16).unwrap(), sum);
        if self.ack {
            self.stream.write_all(b"+").unwrap();
        }
        String::from_utf8(data).unwrap()
    }
}

fn le(text: &str) -> u64 {
    let mut v = [0u8; 8];
    for i in 0..text.len() / 2 {
        v[i] = u8::from_str_radix(&text[i * 2..i * 2 + 2], 16).unwrap();
    }
    u64::from_le_bytes(v)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn run(arch: Arch) {
    let Some((mut emu, module)) = setup(arch) else { return };
    let entry = emu.symbol(module, "Java_fix_Native_hooked").unwrap();
    let add = emu.symbol(module, "fix_add").unwrap();
    let entry_bytes = emu.cpu.memory().read_vec(entry & !1, 4).unwrap();
    let add_bytes = emu.cpu.memory().read_vec(add & !1, 4).unwrap();
    let stub = GdbStub::bind("127.0.0.1:0").unwrap();
    let address = stub.local_addr().unwrap();
    emu.set_debugger(Box::new(stub));
    emu.break_next();
    let (width, pc_index) = if arch == Arch::Arm64 { (16, 32) } else { (8, 15) };
    let expected = add_bytes.clone();
    let client = std::thread::spawn(move || {
        let mut c = Client { stream: TcpStream::connect(address).unwrap(), ack: true };
        assert!(c.ask("qSupported:multiprocess+;swbreak+;xmlRegisters=aarch64,arm").contains("qXfer:features:read+"));
        assert_eq!(c.ask("QStartNoAckMode"), "OK");
        c.ack = false;
        let stop = c.ask("?");
        assert!(stop.starts_with("T05"), "{stop}");
        let xml = c.ask("qXfer:features:read:target.xml:0,fff");
        assert!(xml.starts_with("l<?xml") && xml.contains(if arch == Arch::Arm64 { "aarch64.core" } else { "arm.core" }), "{xml}");
        let regs = c.ask("g");
        assert_eq!(regs.len(), if arch == Arch::Arm64 { 33 * 16 + 8 } else { 16 * 8 + 8 });
        assert_eq!(le(&regs[pc_index * width..(pc_index + 1) * width]), entry & !1);
        if arch == Arch::Arm32 {
            // Thumb state in CPSR.
            assert_eq!(le(&regs[16 * 8..17 * 8]) & 0x20, (entry & 1) << 5);
        }
        assert_eq!(c.ask(&format!("m{:x},4", entry & !1)), hex(&entry_bytes));
        // Kind 4 (ARM) even for Thumb code, as lldb sends without symbols: the stub corrects it.
        let kind = 4;
        assert_eq!(c.ask(&format!("Z0,{:x},{kind}", add & !1)), "OK");
        assert_eq!(c.ask(&format!("m{:x},4", add & !1)), hex(&expected), "breakpoint hidden");
        let stop = c.ask("c");
        assert!(stop.starts_with("T05swbreak:"), "{stop}");
        assert_eq!(le(&c.ask(&format!("p{pc_index:x}"))), add & !1);
        assert_eq!(le(&c.ask("p0")), 2);
        assert_eq!(le(&c.ask("p1")), 3);
        // x0 = 10 before the add: fix_add returns 13.
        assert_eq!(c.ask(&format!("P0={}", hex(&10u64.to_le_bytes()[..width / 2]))), "OK");
        let stop = c.ask("s");
        assert!(stop.starts_with("T05"), "{stop}");
        let pc = le(&c.ask(&format!("p{pc_index:x}")));
        assert!(pc > add & !1 && pc < (add & !1) + 8, "{pc:#x}");
        assert_eq!(c.ask(&format!("z0,{:x},{kind}", add & !1)), "OK");
        assert_eq!(c.ask("D"), "OK");
    });
    let result = emu.call_static("fix/Native", "hooked", "(II)I", &[Value::Int(2), Value::Int(3)]).unwrap();
    client.join().unwrap();
    assert_eq!(result, Value::Int(13 * 100000 + 12345));
    assert_eq!(emu.breakpoints().count(), 0);
    assert_eq!(emu.cpu.memory().read_vec(add & !1, 4).unwrap(), add_bytes);
}

#[test]
fn gdb_arm64() {
    run(Arch::Arm64);
}

#[test]
fn gdb_arm32() {
    run(Arch::Arm32);
}
