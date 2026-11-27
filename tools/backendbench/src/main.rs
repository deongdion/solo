//! Backend benchmark: the same freestanding guest kernels on each backend.
//!
//! Usage:
//!   backendbench <dynarmic|unicorn> <arm64|arm32> latency [code_cache_mib]
//!   backendbench <dynarmic|unicorn> <arm64|arm32> throughput <instances> <seconds> [code_cache_mib]
//!   backendbench <dynarmic|unicorn> <arm64|arm32> instances <count> [code_cache_mib]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use backend::{Arch, Cpu, Stop};
use machine::Perms;

const CODE: u64 = 0x10000;
const STACK: u64 = 0x80_0000;
const STACK_SIZE: u64 = 0x10_0000;
const DATA: u64 = 0x100_0000;
const DATA_SIZE: u64 = 0x10_0000;
const UNLIMITED: u64 = u64::MAX;

const ARM64_BIN: &[u8] = include_bytes!("../fixtures/arm64.bin");
const ARM64_SYM: &str = include_str!("../fixtures/arm64.sym");
const THUMB_BIN: &[u8] = include_bytes!("../fixtures/thumb.bin");
const THUMB_SYM: &str = include_str!("../fixtures/thumb.sym");

const WORDS: u64 = 1024;
const CHECKSUM_ROUNDS: u64 = 100;
const COPY_LEN: u64 = 4096;
const COPY_ROUNDS: u64 = 50;

struct Kernels {
    empty: u64,
    checksum: u64,
    copy: u64,
}

fn symbol(table: &str, name: &str) -> u64 {
    table
        .lines()
        .find_map(|line| {
            let (n, address) = line.split_once(' ')?;
            (n == name).then(|| u64::from_str_radix(address, 16).ok()).flatten()
        })
        .unwrap_or_else(|| panic!("symbol {name} missing"))
}

fn kernels(arch: Arch) -> Kernels {
    let (table, thumb) = match arch {
        Arch::Arm64 => (ARM64_SYM, 0),
        Arch::Arm32 => (THUMB_SYM, 1),
    };
    Kernels {
        empty: symbol(table, "empty") | thumb,
        checksum: symbol(table, "checksum") | thumb,
        copy: symbol(table, "copy") | thumb,
    }
}

fn create(backend: &str, arch: Arch, code_cache_mib: u64) -> Box<dyn Cpu> {
    let mut cpu: Box<dyn Cpu> = match backend {
        "dynarmic" => Box::new(backend::Dynarmic::with_code_cache(arch, code_cache_mib << 20).unwrap()),
        #[cfg(feature = "unicorn")]
        "unicorn" => Box::new(backend::Unicorn::new(arch).unwrap()),
        other => panic!("unknown backend {other}"),
    };
    let image = match arch {
        Arch::Arm64 => ARM64_BIN,
        Arch::Arm32 => THUMB_BIN,
    };
    cpu.map(CODE, 0x1000 * ((image.len() as u64).div_ceil(0x1000)), Perms::RX, "code").unwrap();
    cpu.memory_mut().write(CODE, image).unwrap();
    cpu.map(STACK, STACK_SIZE, Perms::RW, "stack").unwrap();
    cpu.map(DATA, DATA_SIZE, Perms::RW, "data").unwrap();
    let words: Vec<u8> = (0..WORDS as u32).flat_map(|i| i.wrapping_mul(2654435761).to_le_bytes()).collect();
    cpu.memory_mut().write(DATA, &words).unwrap();
    cpu.set_reg(backend::Reg::Sp, STACK + STACK_SIZE - 0x100);
    cpu
}

fn call(cpu: &mut dyn Cpu, function: u64, args: &[u64]) -> u64 {
    let (stop, value) = cpu.call(function, args, UNLIMITED).unwrap();
    assert_eq!(stop, Stop::Returned, "unexpected stop");
    match cpu.arch() {
        Arch::Arm64 => value,
        Arch::Arm32 => value & 0xffff_ffff,
    }
}

// Host reference implementations of the guest kernels.
fn reference_checksum() -> u64 {
    let mut h: u32 = 0x811c9dc5;
    for _ in 0..CHECKSUM_ROUNDS {
        for i in 0..WORDS as u32 {
            h = (h ^ i.wrapping_mul(2654435761)).wrapping_mul(16777619);
            h = h.rotate_left(7);
        }
    }
    h as u64
}

fn reference_copy() -> u64 {
    let src: Vec<u8> = (0..WORDS as u32).flat_map(|i| i.wrapping_mul(2654435761).to_le_bytes()).collect();
    let mut s: u32 = 0;
    for r in 0..COPY_ROUNDS as u32 {
        let last = src[COPY_LEN as usize - 1].wrapping_add(r as u8);
        s = s.wrapping_add(last as u32);
    }
    s as u64
}

fn stats(label: &str, mut samples: Vec<Duration>) {
    samples.sort();
    let p50 = samples[samples.len() / 2];
    let p99 = samples[samples.len() * 99 / 100];
    println!("{label} p50_us={:.3} p99_us={:.3} n={}", p50.as_secs_f64() * 1e6, p99.as_secs_f64() * 1e6, samples.len());
}

fn timed(n: usize, mut f: impl FnMut()) -> Vec<Duration> {
    (0..n)
        .map(|_| {
            let start = Instant::now();
            f();
            start.elapsed()
        })
        .collect()
}

fn latency(backend: &str, arch: Arch, cache: u64) {
    let start = Instant::now();
    let mut cpu = create(backend, arch, cache);
    let init = start.elapsed();
    let k = kernels(arch);
    let start = Instant::now();
    call(cpu.as_mut(), k.empty, &[]);
    let first = start.elapsed();
    println!("init_us={:.1} first_call_us={:.1}", init.as_secs_f64() * 1e6, first.as_secs_f64() * 1e6);

    let src = DATA;
    let dst = DATA + 0x8000;
    let checksum = call(cpu.as_mut(), k.checksum, &[DATA, WORDS, CHECKSUM_ROUNDS]);
    let copy = call(cpu.as_mut(), k.copy, &[dst, src, COPY_LEN, COPY_ROUNDS]);
    println!(
        "correct checksum={} copy={}",
        checksum == reference_checksum(),
        copy == reference_copy()
    );

    for _ in 0..1000 {
        call(cpu.as_mut(), k.empty, &[]);
    }
    stats("empty", timed(100_000, || {
        call(cpu.as_mut(), k.empty, &[]);
    }));
    for _ in 0..20 {
        call(cpu.as_mut(), k.checksum, &[DATA, WORDS, CHECKSUM_ROUNDS]);
    }
    stats("checksum_102400", timed(500, || {
        call(cpu.as_mut(), k.checksum, &[DATA, WORDS, CHECKSUM_ROUNDS]);
    }));
    stats("copy_204800", timed(500, || {
        call(cpu.as_mut(), k.copy, &[dst, src, COPY_LEN, COPY_ROUNDS]);
    }));

    // Instruction budget: the stop must be Budget, and enforcing it must not cost per instruction.
    let (stop, _) = cpu.call(k.checksum, &[DATA, WORDS, CHECKSUM_ROUNDS], 10_000).unwrap();
    println!("budget_stop={stop:?}");
    stats("checksum_102400_budgeted", timed(500, || {
        let (stop, _) = cpu.call(k.checksum, &[DATA, WORDS, CHECKSUM_ROUNDS], 1 << 40).unwrap();
        assert_eq!(stop, Stop::Returned);
    }));
}

fn throughput(backend: &str, arch: Arch, instances: usize, seconds: u64, cache: u64) {
    let calls = Arc::new(AtomicU64::new(0));
    let work = Arc::new(AtomicU64::new(0));
    let go = Arc::new(AtomicBool::new(false));
    let start = Instant::now();
    let threads: Vec<_> = (0..instances)
        .map(|_| {
            let (calls, work, go) = (calls.clone(), work.clone(), go.clone());
            let backend = backend.to_string();
            std::thread::spawn(move || {
                let mut cpu = create(&backend, arch, cache);
                let k = kernels(arch);
                for _ in 0..500 {
                    call(cpu.as_mut(), k.empty, &[]);
                }
                while !go.load(Ordering::Acquire) {
                    std::hint::spin_loop();
                }
                let deadline = Instant::now() + Duration::from_secs(seconds);
                let (mut n, mut w) = (0u64, 0u64);
                while Instant::now() < deadline {
                    call(cpu.as_mut(), k.empty, &[]);
                    n += 1;
                    if n % 64 == 0 {
                        call(cpu.as_mut(), k.checksum, &[DATA, 64, 1]);
                        w += 1;
                    }
                }
                calls.fetch_add(n, Ordering::Relaxed);
                work.fetch_add(w, Ordering::Relaxed);
            })
        })
        .collect();
    std::thread::sleep(Duration::from_millis(200));
    let ready = start.elapsed();
    go.store(true, Ordering::Release);
    for t in threads {
        t.join().unwrap();
    }
    println!(
        "instances={instances} setup_ms={:.1} empty_calls_per_s={:.0}",
        ready.as_secs_f64() * 1e3,
        calls.load(Ordering::Relaxed) as f64 / seconds as f64
    );
}

fn instances(backend: &str, arch: Arch, count: usize, cache: u64) {
    let start = Instant::now();
    let mut all: Vec<Box<dyn Cpu>> = (0..count).map(|_| create(backend, arch, cache)).collect();
    let k = kernels(arch);
    for cpu in all.iter_mut() {
        call(cpu.as_mut(), k.checksum, &[DATA, WORDS, 1]);
        call(cpu.as_mut(), k.copy, &[DATA + 0x8000, DATA, COPY_LEN, 1]);
    }
    println!("instances={count} create_and_warm_ms={:.1}", start.elapsed().as_secs_f64() * 1e3);
    // Keep the process alive briefly so the external sampler sees the steady state.
    std::thread::sleep(Duration::from_millis(500));
    drop(all);
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let backend = args.get(1).map(String::as_str).unwrap_or("dynarmic");
    let arch = match args.get(2).map(String::as_str) {
        Some("arm32") => Arch::Arm32,
        _ => Arch::Arm64,
    };
    let number = |i: usize, default: u64| args.get(i).and_then(|v| v.parse().ok()).unwrap_or(default);
    match args.get(3).map(String::as_str).unwrap_or("latency") {
        "latency" => latency(backend, arch, number(4, 0)),
        "throughput" => throughput(backend, arch, number(4, 1) as usize, number(5, 5), number(6, 0)),
        "instances" => instances(backend, arch, number(4, 8) as usize, number(5, 0)),
        other => panic!("unknown mode {other}"),
    }
}
