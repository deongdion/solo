//! GDB remote serial protocol server on top of `debug::Debugger`, for gdb and lldb
//! (`target remote host:port`, `gdb-remote host:port`).
//!
//! The stub waits for a client at the first stop (use `Emulator::break_next` to stop before the
//! first instruction of the next call) and serves it while the guest is stopped: registers (the
//! general ones, SP, PC and CPSR/PSTATE, described by a target.xml), memory (breakpoint patches
//! hidden), software breakpoints (Z0/z0), continue and step (c, s, vCont), detach and kill.
//! When a call returns while the client waits for a stop, the client is told at the next stop or
//! when the stub is dropped (an exit reply).

use std::io::{self, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream, ToSocketAddrs};

use backend::{Arch, Reg};

use crate::debug::{DebugEvent, Debugger, Resume};
use crate::Emulator;

const SIGTRAP: u32 = 5;

pub struct GdbStub {
    listener: TcpListener,
    connection: Option<Connection>,
    /// The client sent c or s and waits for a stop reply.
    running: bool,
}

struct Connection {
    reader: BufReader<TcpStream>,
    writer: TcpStream,
    ack: bool,
}

impl GdbStub {
    /// Listens on `address` (port 0 picks a free port, see `local_addr`).
    pub fn bind(address: impl ToSocketAddrs) -> io::Result<GdbStub> {
        Ok(GdbStub { listener: TcpListener::bind(address)?, connection: None, running: false })
    }

    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.listener.local_addr()
    }

    fn connection(&mut self) -> io::Result<&mut Connection> {
        if self.connection.is_none() {
            let (stream, _) = self.listener.accept()?;
            stream.set_nodelay(true)?;
            self.connection = Some(Connection { reader: BufReader::new(stream.try_clone()?), writer: stream, ack: true });
        }
        Ok(self.connection.as_mut().expect("connection"))
    }

    fn serve(&mut self, emu: &mut Emulator, event: DebugEvent) -> io::Result<Resume> {
        let reply = stop_reply(emu, event);
        if std::mem::take(&mut self.running) {
            self.connection()?.send(&reply)?;
        }
        loop {
            let packet = self.connection()?.receive()?;
            let Some(packet) = packet else {
                // The client went away: run on without it.
                self.connection = None;
                return Ok(Resume::Continue);
            };
            let answer = match command(emu, &packet, &reply) {
                Command::Reply(text) => text,
                Command::NoAck => {
                    let c = self.connection()?;
                    c.send("OK")?;
                    c.ack = false;
                    continue;
                }
                Command::Resume(resume) => {
                    self.running = true;
                    return Ok(resume);
                }
                Command::Detach => {
                    self.connection()?.send("OK")?;
                    self.connection = None;
                    clear_breakpoints(emu);
                    return Ok(Resume::Continue);
                }
                Command::Kill => {
                    self.connection = None;
                    return Ok(Resume::Abort);
                }
            };
            self.connection()?.send(&answer)?;
        }
    }
}

impl Drop for GdbStub {
    fn drop(&mut self) {
        if self.running
            && let Some(c) = self.connection.as_mut()
        {
            let _ = c.send("W00");
        }
    }
}

impl Debugger for GdbStub {
    fn stopped(&mut self, emu: &mut Emulator, event: DebugEvent) -> Resume {
        self.serve(emu, event).unwrap_or_else(|_| {
            // A broken connection: forget it and run on.
            self.connection = None;
            self.running = false;
            Resume::Continue
        })
    }
}

impl Connection {
    /// The next packet's payload; None at end of stream. A lone 0x03 (interrupt) reads as "\x03".
    fn receive(&mut self) -> io::Result<Option<String>> {
        let mut byte = [0u8; 1];
        loop {
            if self.reader.read(&mut byte)? == 0 {
                return Ok(None);
            }
            match byte[0] {
                b'$' => break,
                0x03 => return Ok(Some("\x03".into())),
                _ => {} // acks and noise
            }
        }
        let mut data = Vec::new();
        loop {
            if self.reader.read(&mut byte)? == 0 {
                return Ok(None);
            }
            if byte[0] == b'#' {
                break;
            }
            data.push(byte[0]);
        }
        let mut checksum = [0u8; 2];
        self.reader.read_exact(&mut checksum)?;
        if self.ack {
            let expected = u8::from_str_radix(std::str::from_utf8(&checksum).unwrap_or("zz"), 16).ok();
            let sum = data.iter().fold(0u8, |a, b| a.wrapping_add(*b));
            self.writer.write_all(if expected == Some(sum) { b"+" } else { b"-" })?;
        }
        Ok(Some(String::from_utf8_lossy(&unescape(&data)).into_owned()))
    }

    fn send(&mut self, payload: &str) -> io::Result<()> {
        let escaped = escape(payload.as_bytes());
        let sum = escaped.iter().fold(0u8, |a, b| a.wrapping_add(*b));
        let mut packet = Vec::with_capacity(escaped.len() + 4);
        packet.push(b'$');
        packet.extend_from_slice(&escaped);
        packet.extend_from_slice(format!("#{sum:02x}").as_bytes());
        loop {
            self.writer.write_all(&packet)?;
            if !self.ack {
                return Ok(());
            }
            let mut byte = [0u8; 1];
            loop {
                if self.reader.read(&mut byte)? == 0 {
                    return Err(io::ErrorKind::UnexpectedEof.into());
                }
                match byte[0] {
                    b'+' => return Ok(()),
                    b'-' => break,
                    _ => {}
                }
            }
        }
    }
}

fn escape(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len());
    for &b in data {
        if matches!(b, b'$' | b'#' | b'}' | b'*') {
            out.push(b'}');
            out.push(b ^ 0x20);
        } else {
            out.push(b);
        }
    }
    out
}

fn unescape(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len());
    let mut it = data.iter();
    while let Some(&b) = it.next() {
        if b == b'}' {
            if let Some(&n) = it.next() {
                out.push(n ^ 0x20);
            }
        } else {
            out.push(b);
        }
    }
    out
}

enum Command {
    Reply(String),
    NoAck,
    Resume(Resume),
    Detach,
    Kill,
}

fn stop_reply(emu: &Emulator, event: DebugEvent) -> String {
    let tid = emu.kernel.threads.current;
    match event {
        DebugEvent::Breakpoint(_) => format!("T{SIGTRAP:02x}swbreak:;thread:{tid:x};"),
        DebugEvent::Step(_) => format!("T{SIGTRAP:02x}thread:{tid:x};"),
        DebugEvent::Signal { signo, .. } => format!("T{signo:02x}thread:{tid:x};"),
    }
}

/// Registers in target.xml order: (value, bytes).
fn registers(emu: &Emulator) -> Vec<(u64, usize)> {
    match emu.cpu.arch() {
        Arch::Arm64 => {
            let mut out: Vec<(u64, usize)> = (0..31).map(|i| (emu.cpu.reg(Reg::General(i)), 8)).collect();
            out.push((emu.cpu.reg(Reg::Sp), 8));
            out.push((emu.cpu.pc(), 8));
            out.push((emu.cpu.reg(Reg::Flags), 4));
            out
        }
        Arch::Arm32 => {
            let pc = emu.cpu.pc();
            let mut out: Vec<(u64, usize)> = (0..13).map(|i| (emu.cpu.reg(Reg::General(i)), 4)).collect();
            out.push((emu.cpu.reg(Reg::Sp), 4));
            out.push((emu.cpu.reg(Reg::Lr), 4));
            out.push((pc & !1, 4));
            let thumb = (pc & 1) << 5;
            out.push((emu.cpu.reg(Reg::Flags) | thumb, 4));
            out
        }
    }
}

fn set_register(emu: &mut Emulator, index: usize, value: u64) -> bool {
    let arch = emu.cpu.arch();
    let (general, sp, pc, flags) = if arch == Arch::Arm64 { (31, 31, 32, 33) } else { (13, 13, 15, 16) };
    match index {
        i if i < general => emu.cpu.set_reg(Reg::General(i as u8), value),
        i if i == sp => emu.cpu.set_reg(Reg::Sp, value),
        14 if arch == Arch::Arm32 => emu.cpu.set_reg(Reg::Lr, value),
        i if i == pc => {
            let thumb = arch == Arch::Arm32 && emu.cpu.pc() & 1 != 0;
            emu.cpu.set_pc(value | thumb as u64);
        }
        i if i == flags => {
            emu.cpu.set_reg(Reg::Flags, value & !0x20);
            if arch == Arch::Arm32 {
                let pc = emu.cpu.pc() & !1;
                emu.cpu.set_pc(pc | (value >> 5) & 1);
            }
        }
        _ => return false,
    }
    true
}

fn hex_le(value: u64, bytes: usize) -> String {
    value.to_le_bytes()[..bytes].iter().map(|b| format!("{b:02x}")).collect()
}

fn parse_le(text: &str) -> Option<u64> {
    let bytes = parse_hex_bytes(text)?;
    let mut value = [0u8; 8];
    value[..bytes.len().min(8)].copy_from_slice(&bytes[..bytes.len().min(8)]);
    Some(u64::from_le_bytes(value))
}

fn parse_hex_bytes(text: &str) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2) {
        return None;
    }
    (0..text.len()).step_by(2).map(|i| u8::from_str_radix(&text[i..i + 2], 16).ok()).collect()
}

fn hex(text: &str) -> Option<u64> {
    u64::from_str_radix(text, 16).ok()
}

fn target_xml(arch: Arch) -> String {
    let mut regs = String::new();
    match arch {
        Arch::Arm64 => {
            for i in 0..31 {
                regs.push_str(&format!("<reg name=\"x{i}\" bitsize=\"64\" type=\"int\"/>"));
            }
            regs.push_str("<reg name=\"sp\" bitsize=\"64\" type=\"data_ptr\"/><reg name=\"pc\" bitsize=\"64\" type=\"code_ptr\"/><reg name=\"cpsr\" bitsize=\"32\" type=\"int\"/>");
            format!(
                "<?xml version=\"1.0\"?><!DOCTYPE target SYSTEM \"gdb-target.dtd\"><target version=\"1.0\"><architecture>aarch64</architecture><osabi>GNU/Linux</osabi><feature name=\"org.gnu.gdb.aarch64.core\">{regs}</feature></target>"
            )
        }
        Arch::Arm32 => {
            for i in 0..13 {
                regs.push_str(&format!("<reg name=\"r{i}\" bitsize=\"32\" type=\"uint32\"/>"));
            }
            regs.push_str("<reg name=\"sp\" bitsize=\"32\" type=\"data_ptr\"/><reg name=\"lr\" bitsize=\"32\"/><reg name=\"pc\" bitsize=\"32\" type=\"code_ptr\"/><reg name=\"cpsr\" bitsize=\"32\"/>");
            format!(
                "<?xml version=\"1.0\"?><!DOCTYPE target SYSTEM \"gdb-target.dtd\"><target version=\"1.0\"><architecture>arm</architecture><osabi>GNU/Linux</osabi><feature name=\"org.gnu.gdb.arm.core\">{regs}</feature></target>"
            )
        }
    }
}

fn clear_breakpoints(emu: &mut Emulator) {
    let all: Vec<u64> = emu.breakpoints().collect();
    for address in all {
        let _ = emu.remove_breakpoint(address);
    }
}

fn command(emu: &mut Emulator, packet: &str, stop: &str) -> Command {
    use Command::Reply;
    let arch = emu.cpu.arch();
    let ok = || Reply("OK".into());
    let error = || Reply("E01".into());
    let tid = emu.kernel.threads.current;
    match packet {
        "?" => return Reply(stop.into()),
        "\x03" => return Reply(stop.into()),
        "QStartNoAckMode" => return Command::NoAck,
        "qAttached" => return Reply("1".into()),
        "qC" => return Reply(format!("QC{tid:x}")),
        "qfThreadInfo" => return Reply(format!("m{tid:x}")),
        "qsThreadInfo" => return Reply("l".into()),
        "vCont?" => return Reply("vCont;c;C;s;S".into()),
        "c" | "vCont;c" => return Command::Resume(Resume::Continue),
        "s" | "vCont;s" => return Command::Resume(Resume::Step),
        "D" => return Command::Detach,
        "k" | "vKill" => return Command::Kill,
        "qHostInfo" => {
            let triple: String = (if arch == Arch::Arm64 { "aarch64-unknown-linux-android" } else { "armv7-unknown-linux-androideabi" })
                .bytes()
                .map(|b| format!("{b:02x}"))
                .collect();
            let ptr = if arch == Arch::Arm64 { 8 } else { 4 };
            return Reply(format!("triple:{triple};endian:little;ptrsize:{ptr};"));
        }
        "qProcessInfo" => return Reply("pid:1;endian:little;".into()),
        "g" => return Reply(registers(emu).into_iter().map(|(v, n)| hex_le(v, n)).collect()),
        _ => {}
    }
    if packet.starts_with("qSupported") {
        return Reply("PacketSize=4000;qXfer:features:read+;swbreak+;QStartNoAckMode+;vContSupported+".into());
    }
    if let Some(rest) = packet.strip_prefix("vCont;") {
        // One action for every thread: the guest runs its threads itself.
        let action = rest.split(';').next().unwrap_or("c");
        return Command::Resume(if action.starts_with('s') || action.starts_with('S') { Resume::Step } else { Resume::Continue });
    }
    if packet.starts_with('H') || packet.starts_with('T') {
        return ok();
    }
    if let Some(rest) = packet.strip_prefix("qXfer:features:read:target.xml:") {
        let Some((offset, length)) = rest.split_once(',') else { return error() };
        let (Some(offset), Some(length)) = (hex(offset), hex(length)) else { return error() };
        let xml = target_xml(arch);
        let start = (offset as usize).min(xml.len());
        let end = (start + length as usize).min(xml.len());
        let more = if end < xml.len() { 'm' } else { 'l' };
        return Reply(format!("{more}{}", &xml[start..end]));
    }
    if let Some(rest) = packet.strip_prefix('p') {
        let Some(index) = hex(rest) else { return error() };
        return match registers(emu).get(index as usize) {
            Some((v, n)) => Reply(hex_le(*v, *n)),
            None => error(),
        };
    }
    if let Some(rest) = packet.strip_prefix('P') {
        let Some((index, value)) = rest.split_once('=') else { return error() };
        let (Some(index), Some(value)) = (hex(index), parse_le(value)) else { return error() };
        return if set_register(emu, index as usize, value) { ok() } else { error() };
    }
    if let Some(rest) = packet.strip_prefix('G') {
        let sizes: Vec<usize> = registers(emu).iter().map(|(_, n)| *n).collect();
        let mut at = 0;
        for (index, n) in sizes.into_iter().enumerate() {
            let Some(text) = rest.get(at..at + n * 2) else { break };
            if let Some(value) = parse_le(text) {
                set_register(emu, index, value);
            }
            at += n * 2;
        }
        return ok();
    }
    if let Some(rest) = packet.strip_prefix('m') {
        let Some((address, length)) = rest.split_once(',') else { return error() };
        let (Some(address), Some(length)) = (hex(address), hex(length)) else { return error() };
        return match emu.read_memory(address, length.min(0x10000) as usize) {
            Ok(bytes) => Reply(bytes.iter().map(|b| format!("{b:02x}")).collect()),
            Err(_) => Reply("E14".into()),
        };
    }
    if let Some(rest) = packet.strip_prefix('M') {
        let Some((head, data)) = rest.split_once(':') else { return error() };
        let Some((address, _)) = head.split_once(',') else { return error() };
        let (Some(address), Some(data)) = (hex(address), parse_hex_bytes(data)) else { return error() };
        return if emu.write_memory(address, &data).is_ok() { ok() } else { Reply("E14".into()) };
    }
    if let Some(rest) = packet.strip_prefix("Z0,").or_else(|| packet.strip_prefix("z0,")) {
        let insert = packet.starts_with('Z');
        let mut fields = rest.split([',', ';']);
        let (Some(address), Some(kind)) = (fields.next().and_then(hex), fields.next().and_then(hex)) else { return error() };
        // Kinds 2 and 3 are Thumb (16- and 32-bit), 4 is ARM; arm64 always 4. A client without
        // symbols (lldb on a bare connection) guesses ARM: the module's function symbols win.
        let thumb = match emu.linker.module_at(address).and_then(|m| m.is_thumb(address)) {
            Some(thumb) => thumb,
            None => kind == 2 || kind == 3,
        };
        let address = if arch == Arch::Arm32 && thumb { address | 1 } else { address };
        let done = if insert { emu.add_breakpoint(address).is_ok() } else { emu.remove_breakpoint(address).is_ok() };
        return if done { ok() } else { error() };
    }
    // Everything else (X, hardware breakpoints, watchpoints, ...) is not supported.
    Reply(String::new())
}
