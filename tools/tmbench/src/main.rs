//! The M0 benchmark (bench/baseline/Baseline.java) on this runtime: libtmessages.29.so init with
//! JNI_OnLoad, then `Utilities.aesCbcEncryptionByteArray` on 16 bytes and `Utilities.pbkdf2` with
//! 1000 iterations. Output hashes are the first 16 hex digits of SHA-256, as Baseline prints them,
//! so results compare line by line with bench/baseline/results.txt.
//!
//! Usage: tmbench <arm64|arm32> [dynarmic|unicorn] [latency|fork|throughput <instances> <seconds>|pool|poolfork <workers> <seconds>|soak <workers> <seconds>]
//!
//! `throughput` runs one instance per thread directly; `pool` runs the same call as jobs of a
//! `runtime::Pool` (with its per-job deadline) fed by two client threads per worker.
//!
//! `soak` runs for hours and prints one line a minute: process memory (working set and private
//! bytes), pool counters, and a worker's guest memory and live Java objects. Workers are never
//! recycled, so growth inside one instance shows; another thread forks once and then restores the
//! same instance from the template over and over. Every result is checked against the first.

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

fn build(arch: Arch, backend: BackendKind) -> Emulator {
        let (abi, lib) = if arch == Arch::Arm64 { ("arm64-v8a", "lib64") } else { ("armeabi-v7a", "lib") };
        let mut options = Options::new(arch);
        options.backend = backend;
        options.library_dirs = vec![resources().join("main/resources/android/sdk23").join(lib)];
        options.kernel.echo_output = false;
        options.kernel.process_name = "org.telegram.messenger".into();
        options.budget = 10_000_000_000;
        options.precise_faults = std::env::var_os("TMBENCH_PRECISE").is_some();
        let mut emu = Emulator::new(options).expect("emulator");
        let file = resources().join("test/resources/example_binaries").join(abi).join("libtmessages.29.so");
        let module = emu.load(file.to_str().unwrap()).unwrap_or_else(|e| panic!("load: {e}"));
        emu.call_jni_onload(module).unwrap_or_else(|e| panic!("JNI_OnLoad: {e}\n{}", emu.summary()));
        emu
}

fn aes(emu: &mut Emulator) -> Vec<u8> {
        let vm = &mut emu.vm;
        let data = vm.new_bytes(&[0; 16]);
        let key = vm.new_bytes(&[0; 32]);
        let iv = vm.new_bytes(&[0; 16]);
        let args = [Value::Object(data), Value::Object(key), Value::Object(iv), Value::Int(0), Value::Int(16), Value::Int(0), Value::Int(0)];
        emu
            .call_static(UTILITIES, "aesCbcEncryptionByteArray", "([B[B[BIIII)V", &args)
            .unwrap_or_else(|e| panic!("aesCbcEncryptionByteArray: {e}"));
        emu.vm.bytes(data).unwrap()
}

impl Bench {
    fn new(arch: Arch, backend: BackendKind) -> Bench {
        Bench { emu: build(arch, backend) }
    }

    fn aes_cbc(&mut self) -> Vec<u8> {
        aes(&mut self.emu)
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

fn pbkdf2(emu: &mut Emulator, iterations: i32) -> Vec<u8> {
    let vm = &mut emu.vm;
    let password = vm.new_bytes(b"123456");
    let salt = vm.new_bytes(&[0; 8]);
    let dst = vm.new_bytes(&[0; 64]);
    let args = [Value::Object(password), Value::Object(salt), Value::Object(dst), Value::Int(iterations)];
    emu.call_static(UTILITIES, "pbkdf2", "([B[B[BI)V", &args).unwrap_or_else(|e| panic!("pbkdf2: {e}"));
    emu.vm.bytes(dst).unwrap()
}

/// (working set, private bytes) of this process in KiB; zeros where not supported.
#[cfg(windows)]
fn process_memory_kb() -> (u64, u64) {
    #[repr(C)]
    #[derive(Default)]
    struct Counters {
        cb: u32,
        page_fault_count: u32,
        peak_working_set: usize,
        working_set: usize,
        quota_peak_paged_pool: usize,
        quota_paged_pool: usize,
        quota_peak_non_paged_pool: usize,
        quota_non_paged_pool: usize,
        pagefile_usage: usize,
        peak_pagefile_usage: usize,
    }
    unsafe extern "system" {
        fn GetCurrentProcess() -> isize;
        fn K32GetProcessMemoryInfo(process: isize, counters: *mut Counters, cb: u32) -> i32;
    }
    let mut c = Counters { cb: std::mem::size_of::<Counters>() as u32, ..Default::default() };
    // SAFETY: the pseudo handle needs no closing; the struct matches PROCESS_MEMORY_COUNTERS.
    let ok = unsafe { K32GetProcessMemoryInfo(GetCurrentProcess(), &mut c, c.cb) };
    if ok == 0 { (0, 0) } else { (c.working_set as u64 / 1024, c.pagefile_usage as u64 / 1024) }
}

#[cfg(not(windows))]
fn process_memory_kb() -> (u64, u64) {
    // /proc/self/statm: size resident shared text lib data dirty, in pages.
    let statm = std::fs::read_to_string("/proc/self/statm").unwrap_or_default();
    let pages: Vec<u64> = statm.split_whitespace().filter_map(|v| v.parse().ok()).collect();
    if pages.len() < 6 { (0, 0) } else { (pages[1] * 4, pages[5] * 4) }
}

fn soak(arch: Arch, backend: BackendKind, workers: usize, seconds: u64) {
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::Arc;

    let started = Instant::now();
    let mut template = build(arch, backend);
    let aes_ref = aes(&mut template);
    let pbkdf2_ref = pbkdf2(&mut template, 100);
    let config = runtime::PoolConfig { workers, queue_limit: workers * 4, recycle_after_jobs: 0, recycle_above_committed: 0, ..runtime::PoolConfig::default() };
    let pool = Arc::new(runtime::Pool::forking(config, template.fork().expect("fork")).expect("pool"));
    println!("soak arch={arch:?} workers={workers} seconds={seconds} init_ms={:.0} aes={} pbkdf2_100={}", started.elapsed().as_secs_f64() * 1000.0, sha(&aes_ref), sha(&pbkdf2_ref));
    let stop = Arc::new(AtomicBool::new(false));
    let mismatches = Arc::new(AtomicU64::new(0));
    let restores = Arc::new(AtomicU64::new(0));
    let mut threads = Vec::new();
    for _ in 0..workers * 2 {
        let (pool, stop, mismatches) = (pool.clone(), stop.clone(), mismatches.clone());
        let (aes_ref, pbkdf2_ref) = (aes_ref.clone(), pbkdf2_ref.clone());
        threads.push(std::thread::spawn(move || {
            let mut n = 0u64;
            while !stop.load(Ordering::Relaxed) {
                n += 1;
                let heavy = n.is_multiple_of(500);
                match pool.run(move |emu| Ok(if heavy { pbkdf2(emu, 100) } else { aes(emu) })) {
                    Ok(out) => {
                        if &out != if heavy { &pbkdf2_ref } else { &aes_ref } {
                            mismatches.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                    Err(runtime::PoolError::Full) => std::thread::yield_now(),
                    Err(e) => panic!("{e}"),
                }
            }
        }));
    }
    {
        // Restore cycles: one instance reset from the template after every 1000 calls.
        let (stop, mismatches, restores) = (stop.clone(), mismatches.clone(), restores.clone());
        let aes_ref = aes_ref.clone();
        threads.push(std::thread::spawn(move || {
            let mut emu = template.fork().expect("fork");
            while !stop.load(Ordering::Relaxed) {
                for _ in 0..1000 {
                    if aes(&mut emu) != aes_ref {
                        mismatches.fetch_add(1, Ordering::Relaxed);
                    }
                }
                emu.restore(&template).expect("restore");
                restores.fetch_add(1, Ordering::Relaxed);
            }
        }));
    }
    let deadline = started + Duration::from_secs(seconds);
    let mut last_jobs = 0;
    let mut next = Instant::now();
    loop {
        next += Duration::from_secs(60);
        let now = Instant::now();
        if next > now {
            std::thread::sleep(next - now);
        }
        let (working_set, private) = process_memory_kb();
        let stats = pool.stats();
        let guest = pool.run(|emu| Ok((emu.cpu.memory().committed() / 1024, emu.vm.heap.live())));
        let (guest_kb, objects) = guest.unwrap_or((0, 0));
        println!(
            "t_s={} working_set_kb={working_set} private_kb={private} jobs={} jobs_per_s={:.0} failed={} timeouts={} panics={} builds={} restores={} mismatches={} worker_guest_kb={guest_kb} worker_java_objects={objects}",
            started.elapsed().as_secs(),
            stats.jobs,
            (stats.jobs - last_jobs) as f64 / 60.0,
            stats.failed,
            stats.timeouts,
            stats.panics,
            stats.builds,
            restores.load(Ordering::Relaxed),
            mismatches.load(Ordering::Relaxed),
        );
        last_jobs = stats.jobs;
        use std::io::Write;
        std::io::stdout().flush().ok();
        if Instant::now() >= deadline {
            break;
        }
    }
    stop.store(true, Ordering::Relaxed);
    for t in threads {
        t.join().expect("soak thread");
    }
    if let Ok(mut pool) = Arc::try_unwrap(pool) {
        pool.shutdown();
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
    if mode == "fork" {
        // Cost of Emulator::fork against a fresh build, and the forked instance's call latency.
        let mut emu = build(arch, backend);
        for _ in 0..500 {
            aes(&mut emu);
        }
        let mut times = Vec::new();
        let mut last = None;
        for _ in 0..20 {
            let t = Instant::now();
            last = Some(emu.fork().expect("fork"));
            times.push(t.elapsed().as_nanos());
        }
        let mut copy = last.unwrap();
        let t = Instant::now();
        let first = aes(&mut copy);
        let first_call = t.elapsed();
        println!("fork {} first_call_us={:.1}", stats(times), first_call.as_secs_f64() * 1e6);
        // Reset of a used instance in place (what a pool does on recycle), keeping its JIT.
        let mut restores = Vec::new();
        for _ in 0..20 {
            aes(&mut copy);
            let t = Instant::now();
            copy.restore(&emu).expect("restore");
            restores.push(t.elapsed().as_nanos());
        }
        let t = Instant::now();
        aes(&mut copy);
        println!("restore {} first_call_us={:.1}", stats(restores), t.elapsed().as_secs_f64() * 1e6);
        for _ in 0..1000 {
            aes(&mut copy);
        }
        let mut latencies = Vec::with_capacity(10000);
        let mut out = Vec::new();
        for _ in 0..10000 {
            let t = Instant::now();
            out = aes(&mut copy);
            latencies.push(t.elapsed().as_nanos());
        }
        println!("forked aesCbc16 {} out={} first_out={}", stats(latencies), sha(&out), sha(&first));
        println!("guest_committed_kb={}", copy.cpu.memory().committed() / 1024);
        return;
    }
    if mode == "soak" {
        let workers: usize = args.get(4).and_then(|a| a.parse().ok()).unwrap_or(4);
        let seconds: u64 = args.get(5).and_then(|a| a.parse().ok()).unwrap_or(86_400);
        soak(arch, backend, workers, seconds);
        return;
    }
    if mode == "pool" || mode == "poolfork" {
        let workers: usize = args.get(4).and_then(|a| a.parse().ok()).unwrap_or(1);
        let seconds: u64 = args.get(5).and_then(|a| a.parse().ok()).unwrap_or(10);
        let started = Instant::now();
        let config = runtime::PoolConfig { workers, queue_limit: workers * 4, ..runtime::PoolConfig::default() };
        let pool = if mode == "poolfork" {
            runtime::Pool::forking(config, build(arch, backend))
        } else {
            runtime::Pool::new(config, move || {
                let mut emu = build(arch, backend);
                for _ in 0..500 {
                    aes(&mut emu);
                }
                Ok(emu)
            })
        };
        let pool = std::sync::Arc::new(pool.expect("pool"));
        println!("init_all_ms={:.1}", started.elapsed().as_secs_f64() * 1000.0);
        let deadline = Instant::now() + Duration::from_secs(seconds);
        let clients: Vec<_> = (0..workers * 2)
            .map(|_| {
                let pool = pool.clone();
                std::thread::spawn(move || {
                    let mut n = 0u64;
                    while Instant::now() < deadline {
                        match pool.run(|emu| Ok(aes(emu))) {
                            Ok(_) => n += 1,
                            Err(runtime::PoolError::Full) => std::thread::yield_now(),
                            Err(e) => panic!("{e}"),
                        }
                    }
                    n
                })
            })
            .collect();
        let calls: u64 = clients.into_iter().map(|h| h.join().unwrap()).sum();
        let stats = pool.stats();
        println!("workers={workers} calls_per_s={:.0} failed={} builds={}", calls as f64 / seconds as f64, stats.failed, stats.builds);
        return;
    }
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
