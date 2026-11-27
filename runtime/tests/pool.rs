//! Worker pool: jobs on several threads, deadlines, recycling after failures, panics.

mod common;

use std::path::PathBuf;
use std::time::{Duration, Instant};

use backend::Arch;
use runtime::{Emulator, Error, Options, Pool, PoolConfig, PoolError, Value};

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..")
}

fn factory(arch: Arch) -> Option<impl Fn() -> Result<Emulator, Error> + Send + Sync + 'static> {
    let (dir, lib) = if arch == Arch::Arm64 { ("arm64", "lib64") } else { ("arm32", "lib") };
    let fixture = root().join("bench/jni").join(dir).join("libjnifix.so");
    let sdk = root().join("bench/work/unidbg/unidbg-android/src/main/resources/android/sdk23").join(lib);
    if !sdk.is_dir() {
        eprintln!("sdk23 libraries not present; skipped");
        return None;
    }
    let backend = common::backend();
    Some(move || {
        let mut options = Options::new(arch);
        options.backend = backend;
        options.library_dirs = vec![sdk.clone()];
        options.kernel.echo_output = false;
        options.budget = u64::MAX;
        let mut emu = Emulator::new(options)?;
        let module = emu.load(fixture.to_str().unwrap())?;
        emu.call_jni_onload(module)?;
        Ok(emu)
    })
}

fn spin(emu: &mut Emulator, rounds: i32) -> Result<i32, Error> {
    Ok(emu.call_static("fix/Native", "spin", "(I)I", &[Value::Int(rounds)])?.as_i32())
}

fn expected(rounds: i32) -> i32 {
    (0..rounds).fold(0i32, |a, i| a.wrapping_add(i))
}

fn pool(arch: Arch) {
    let Some(factory) = factory(arch) else { return };
    let config = PoolConfig { workers: 4, recycle_after_jobs: 0, recycle_above_committed: 0, job_timeout: Some(Duration::from_millis(300)), queue_limit: 64 };
    let mut pool = Pool::new(config, factory).unwrap();
    assert_eq!(pool.stats().builds, 4);

    // Jobs run on all workers and return their results.
    let tickets: Vec<_> = (0..32).map(|i| pool.submit(move |emu| spin(emu, 1000 + i)).unwrap()).collect();
    for (i, t) in tickets.into_iter().enumerate() {
        assert_eq!(t.wait().unwrap(), expected(1000 + i as i32));
    }

    // A job that never returns is interrupted at its deadline, and its worker is rebuilt.
    let start = Instant::now();
    let error = pool.run(|emu| spin(emu, -1)).unwrap_err();
    let elapsed = start.elapsed();
    assert!(matches!(error, PoolError::Job(Error::Interrupted)), "{error}");
    assert!(elapsed >= Duration::from_millis(300) && elapsed < Duration::from_secs(5), "{elapsed:?}");
    // A panicking job is reported; the worker survives.
    let error = pool.run(|_emu| -> Result<(), Error> { panic!("boom") }).unwrap_err();
    assert!(matches!(&error, PoolError::Panicked(m) if m == "boom"), "{error}");

    // The pool still serves every worker after both.
    let tickets: Vec<_> = (0..16).map(|_| pool.submit(|emu| spin(emu, 5000)).unwrap()).collect();
    for t in tickets {
        assert_eq!(t.wait().unwrap(), expected(5000));
    }
    // A worker rebuilds after replying, so wait for the workers before counting builds.
    pool.shutdown();
    let stats = pool.stats();
    assert_eq!(stats.jobs, 32 + 2 + 16);
    assert_eq!(stats.failed, 2);
    assert_eq!(stats.timeouts, 1);
    assert_eq!(stats.builds, 4 + 2, "the timed-out and the panicked worker were rebuilt");
}

#[test]
fn pool_arm64() {
    pool(Arch::Arm64);
}

#[test]
fn pool_arm32() {
    pool(Arch::Arm32);
}

#[test]
fn interrupt_between_calls_is_cleared() {
    let Some(factory) = factory(Arch::Arm64) else { return };
    let mut emu = factory().unwrap();
    emu.interrupt().interrupt();
    assert!(matches!(spin(&mut emu, 10), Err(Error::Interrupted)));
    // The interrupt was consumed by the call it stopped.
    assert_eq!(spin(&mut emu, 10).unwrap(), expected(10));
    emu.interrupt().interrupt();
    emu.clear_interrupt();
    assert_eq!(spin(&mut emu, 10).unwrap(), expected(10));
}

#[test]
fn full_queue_is_refused() {
    let Some(factory) = factory(Arch::Arm64) else { return };
    let config = PoolConfig { workers: 1, job_timeout: Some(Duration::from_secs(5)), queue_limit: 1, ..PoolConfig::default() };
    let pool = Pool::new(config, factory).unwrap();
    let (tx, rx) = std::sync::mpsc::channel::<()>();
    // Hold the only worker until released.
    let held = pool
        .submit(move |_emu| {
            let _ = rx.recv();
            Ok(())
        })
        .unwrap();
    // Wait until the worker has taken it, so the queue is empty.
    let deadline = Instant::now() + Duration::from_secs(5);
    let queued = loop {
        match pool.submit(|emu| spin(emu, 1)) {
            Ok(t) => break t,
            Err(PoolError::Full) if Instant::now() < deadline => std::thread::yield_now(),
            Err(e) => panic!("{e}"),
        }
    };
    assert!(matches!(pool.submit(|emu| spin(emu, 1)), Err(PoolError::Full)));
    tx.send(()).unwrap();
    held.wait().unwrap();
    assert_eq!(queued.wait().unwrap(), 0);
}

#[test]
fn interrupt_stops_a_running_loop() {
    let Some(factory) = factory(Arch::Arm64) else { return };
    let mut emu = factory().unwrap();
    let handle = emu.interrupt();
    let t = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(200));
        handle.interrupt();
    });
    let start = Instant::now();
    assert!(matches!(spin(&mut emu, -1), Err(Error::Interrupted)));
    assert!(start.elapsed() < Duration::from_secs(5));
    t.join().unwrap();
}
