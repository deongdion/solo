//! The M0 benchmark (bench/baseline/Baseline.java) on this runtime: libtmessages.29.so init with
//! JNI_OnLoad, then `Utilities.aesCbcEncryptionByteArray` on 16 bytes and `Utilities.pbkdf2` with
//! 1000 iterations. Output hashes are the first 16 hex digits of SHA-256, as Baseline prints them,
//! so results compare line by line with bench/baseline/results.txt.
//!
//! Usage: tmbench <arm64|arm32> [dynarmic|unicorn] [latency|throughput <instances> <seconds>]

use std::path::PathBuf;
use std::time::{Duration, Instant};

use backend::Arch;
use runtime::{BackendKind, Emulator, Options, Value};
use sha2::{Digest, Sha256};

const UTILITIES: &str = "org/telegram/messenger/Utilities";

fn resources() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../bench/work/unidbg/unidbg-android/src")
}

struct Bench {
    emu: Emulator,
}

impl Bench {
    fn new(arch: Arch, backend: BackendKind) -> Bench {
        let (abi, lib) = if arch == Arch::Arm64 { ("arm64-v8a", "lib64") } else { ("armeabi-v7a", "lib") };
        let mut options = Options::new(arch);
        options.backend = backend;
        options.library_dirs = vec![resources().join("main/resources/android/sdk23").join(lib)];
        options.kernel.echo_output = false;
        options.kernel.process_name = "org.telegram.messenger".into();
        options.budget = 10_000_000_000;
        let mut emu = Emulator::new(options).expect("emulator");
        let file = resources().join("test/resources/example_binaries").join(abi).join("libtmessages.29.so");
        let module = emu.load(file.to_str().unwrap()).unwrap_or_else(|e| panic!("load: {e}"));
        emu.call_jni_onload(module).unwrap_or_else(|e| panic!("JNI_OnLoad: {e}\n{}", emu.summary()));
        Bench { emu }
    }

    fn aes_cbc(&mut self) -> Vec<u8> {
        let vm = &mut self.emu.vm;
        let data = vm.new_bytes(&[0; 16]);
        let key = vm.new_bytes(&[0; 32]);
        let iv = vm.new_bytes(&[0; 16]);
        let args = [Value::Object(data), Value::Object(key), Value::Object(iv), Value::Int(0), Value::Int(16), Value::Int(0), Value::Int(0)];
        self.emu
            .call_static(UTILITIES, "aesCbcEncryptionByteArray", "([B[B[BIIII)V", &args)
            .unwrap_or_else(|e| panic!("aesCbcEncryptionByteArray: {e}"));
        self.emu.vm.bytes(data).unwrap()
    }

    fn pbkdf2(&mut self, iterations: i32) -> Vec<u8> {
        let vm = &mut self.emu.vm;
        let password = vm.new_bytes(b"123456");
        let salt = vm.new_bytes(&[0; 8]);
        let dst = vm.new_bytes(&[0; 64]);
        let args = [Value::Object(password), Value::Object(salt), Value::Object(dst), Value::Int(iterations)];
        self.emu.call_static(UTILITIES, "pbkdf2", "([B[B[BI)V", &args).unwrap_or_else(|e| panic!("pbkdf2: {e}"));
        self.emu.vm.bytes(dst).unwrap()
    }
}

fn sha(bytes: &[u8]) -> String {
    Sha256::digest(bytes).iter().take(8).map(|b| format!("{b:02x}")).collect()
}

fn stats(mut nanos: Vec<u128>) -> String {
    nanos.sort_unstable();
    let p50 = nanos[nanos.len() / 2] as f64 / 1000.0;
    let p99 = nanos[(nanos.len() as f64 * 0.99) as usize] as f64 / 1000.0;
    format!("p50_us={p50:.1} p99_us={p99:.1} n={}", nanos.len())
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let arch = if args.get(1).map(String::as_str) == Some("arm32") { Arch::Arm32 } else { Arch::Arm64 };
    let backend = match args.get(2).map(String::as_str) {
        #[cfg(feature = "unicorn")]
        Some("unicorn") => BackendKind::Unicorn,
        Some("dynarmic") | None => BackendKind::Dynarmic,
        Some(other) => panic!("backend {other} not available in this build"),
    };
    let mode = args.get(3).map(String::as_str).unwrap_or("latency");
    if mode == "throughput" {
        let instances: usize = args.get(4).and_then(|a| a.parse().ok()).unwrap_or(1);
        let seconds: u64 = args.get(5).and_then(|a| a.parse().ok()).unwrap_or(10);
        // Each instance lives on its own thread: created, warmed up, then all start together.
        let started = Instant::now();
        let ready = std::sync::Arc::new(std::sync::Barrier::new(instances + 1));
        let go = std::sync::Arc::new(std::sync::Barrier::new(instances + 1));
        let handles: Vec<_> = (0..instances)
            .map(|_| {
                let (ready, go) = (ready.clone(), go.clone());
                std::thread::spawn(move || {
                    let mut b = Bench::new(arch, backend);
                    for _ in 0..500 {
                        b.aes_cbc();
                    }
                    ready.wait();
                    go.wait();
                    let deadline = Instant::now() + Duration::from_secs(seconds);
                    let mut n = 0u64;
                    while Instant::now() < deadline {
                        b.aes_cbc();
                        n += 1;
                    }
                    n
                })
            })
            .collect();
        ready.wait();
        println!("init_all_ms={:.1}", started.elapsed().as_secs_f64() * 1000.0);
        go.wait();
        let calls: u64 = handles.into_iter().map(|h| h.join().unwrap()).sum();
        println!("instances={instances} calls_per_s={:.0}", calls as f64 / seconds as f64);
        return;
    }

    let started = Instant::now();
    let mut b = Bench::new(arch, backend);
    let init = started.elapsed();
    let t = Instant::now();
    let first = b.aes_cbc();
    let first_call = t.elapsed();
    println!("init_ms={:.1} first_call_us={:.1}", init.as_secs_f64() * 1000.0, first_call.as_secs_f64() * 1e6);
    for _ in 0..1000 {
        b.aes_cbc();
    }
    let mut latencies = Vec::with_capacity(10000);
    let mut out = Vec::new();
    for _ in 0..10000 {
        let t = Instant::now();
        out = b.aes_cbc();
        latencies.push(t.elapsed().as_nanos());
    }
    println!("aesCbc16 {} out={} first_out={}", stats(latencies), sha(&out), sha(&first));
    for _ in 0..20 {
        b.pbkdf2(1000);
    }
    let mut latencies = Vec::with_capacity(200);
    let mut pout = Vec::new();
    for _ in 0..200 {
        let t = Instant::now();
        pout = b.pbkdf2(1000);
        latencies.push(t.elapsed().as_nanos());
    }
    println!("pbkdf2_1000 {} out={}", stats(latencies), sha(&pout));
    println!(
        "guest_committed_kb={} java_objects={} unhandled_jni={}",
        b.emu.cpu.memory().committed() / 1024,
        b.emu.vm.heap.live(),
        b.emu.vm.unhandled.len()
    );
}
