//! A pool of worker threads, each owning one emulator built by a factory (load, JNI_OnLoad, ...).
//!
//! Jobs are closures run on whichever worker is free. A job that runs past its timeout is
//! interrupted; a job that fails in a way that may leave the guest inconsistent (crash, timeout,
//! budget, fatal JNI error, panic) gets its worker's emulator rebuilt before the next job, as do
//! emulators past a call count or a guest memory size. A panicking job is caught and reported;
//! the worker survives.

use std::collections::BTreeMap;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::{Emulator, Error};

#[derive(Debug, Clone)]
pub struct PoolConfig {
    pub workers: usize,
    /// Rebuild an emulator after this many jobs (0: never).
    pub recycle_after_jobs: u64,
    /// Rebuild an emulator whose committed guest memory exceeds this many bytes (0: never).
    pub recycle_above_committed: u64,
    /// Interrupt a job that runs longer than this.
    pub job_timeout: Option<Duration>,
    /// Jobs waiting for a worker; `submit` fails with `PoolError::Full` beyond it.
    pub queue_limit: usize,
}

impl Default for PoolConfig {
    fn default() -> PoolConfig {
        PoolConfig {
            workers: std::thread::available_parallelism().map_or(4, |n| n.get()),
            recycle_after_jobs: 0,
            recycle_above_committed: 1 << 30,
            job_timeout: Some(Duration::from_secs(30)),
            queue_limit: 1024,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum PoolError {
    #[error("pool is shut down")]
    Closed,
    #[error("pool queue is full")]
    Full,
    #[error("emulator factory failed: {0}")]
    Init(Error),
    #[error("job panicked: {0}")]
    Panicked(String),
    #[error(transparent)]
    Job(Error),
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PoolStats {
    pub jobs: u64,
    pub failed: u64,
    pub timeouts: u64,
    pub panics: u64,
    /// Emulators built, including the first one of each worker.
    pub builds: u64,
}

type Job = Box<dyn FnOnce(&mut Emulator) -> Outcome + Send>;

/// What a job tells its worker.
enum Outcome {
    Ok,
    /// The job failed; `poisoned` when the emulator must be rebuilt.
    Failed { poisoned: bool, timeout: bool },
}

/// The result of a submitted job.
pub struct Ticket<R> {
    receiver: Receiver<Result<R, PoolError>>,
}

impl<R> Ticket<R> {
    pub fn wait(self) -> Result<R, PoolError> {
        self.receiver.recv().unwrap_or_else(|_| Err(PoolError::Panicked("worker dropped the job".into())))
    }
}

struct Shared {
    stats: Mutex<PoolStats>,
    watchdog: Watchdog,
}

pub struct Pool {
    sender: Option<SyncSender<Job>>,
    workers: Vec<JoinHandle<()>>,
    shared: Arc<Shared>,
    timeout: Option<Duration>,
}

/// Errors after which the guest state is not trusted.
fn poisons(error: &Error) -> bool {
    !matches!(error, Error::JavaException { .. } | Error::NoNative(_) | Error::NoSymbol(_))
}

impl Pool {
    /// Starts the workers and builds one emulator each with `factory`; fails if any build fails.
    pub fn new<F>(config: PoolConfig, factory: F) -> Result<Pool, PoolError>
    where
        F: Fn() -> Result<Emulator, Error> + Send + Sync + 'static,
    {
        let factory = Arc::new(factory);
        let shared = Arc::new(Shared { stats: Mutex::new(PoolStats::default()), watchdog: Watchdog::start() });
        let (sender, receiver) = mpsc::sync_channel::<Job>(config.queue_limit);
        let receiver = Arc::new(Mutex::new(receiver));
        let (ready_tx, ready_rx) = mpsc::channel::<Result<(), Error>>();
        let mut workers = Vec::new();
        for index in 0..config.workers.max(1) {
            let (factory, receiver, shared, ready, config) = (factory.clone(), receiver.clone(), shared.clone(), ready_tx.clone(), config.clone());
            let handle = std::thread::Builder::new()
                .name(format!("emulator-{index}"))
                .spawn(move || worker(config, factory.as_ref(), receiver, shared, ready))
                .map_err(|e| PoolError::Panicked(format!("spawn: {e}")))?;
            workers.push(handle);
        }
        drop(ready_tx);
        let mut pool = Pool { sender: Some(sender), workers, shared, timeout: config.job_timeout };
        for _ in 0..pool.workers.len() {
            match ready_rx.recv() {
                Ok(Ok(())) => {}
                Ok(Err(e)) => {
                    pool.shutdown();
                    return Err(PoolError::Init(e));
                }
                Err(_) => {
                    pool.shutdown();
                    return Err(PoolError::Panicked("worker died during start".into()));
                }
            }
        }
        Ok(pool)
    }

    /// Queues a job. Fails with `Full` instead of blocking when the queue is at its limit.
    pub fn submit<R, J>(&self, job: J) -> Result<Ticket<R>, PoolError>
    where
        R: Send + 'static,
        J: FnOnce(&mut Emulator) -> Result<R, Error> + Send + 'static,
    {
        let sender = self.sender.as_ref().ok_or(PoolError::Closed)?;
        let (tx, rx) = mpsc::sync_channel(1);
        let timeout = self.timeout;
        let shared = self.shared.clone();
        let wrapped: Job = Box::new(move |emu: &mut Emulator| {
            emu.clear_interrupt();
            let armed = timeout.map(|t| shared.watchdog.arm(Instant::now() + t, emu.interrupt()));
            let result = catch_unwind(AssertUnwindSafe(|| job(emu)));
            if let Some(id) = armed {
                shared.watchdog.disarm(id);
            }
            emu.clear_interrupt();
            match result {
                Ok(Ok(value)) => {
                    let _ = tx.send(Ok(value));
                    Outcome::Ok
                }
                Ok(Err(error)) => {
                    let poisoned = poisons(&error);
                    let timeout = matches!(error, Error::Interrupted);
                    let _ = tx.send(Err(PoolError::Job(error)));
                    Outcome::Failed { poisoned, timeout }
                }
                Err(panic) => {
                    let message = panic
                        .downcast_ref::<&str>()
                        .map(|s| s.to_string())
                        .or_else(|| panic.downcast_ref::<String>().cloned())
                        .unwrap_or_else(|| "non-string panic".into());
                    let _ = tx.send(Err(PoolError::Panicked(message)));
                    Outcome::Failed { poisoned: true, timeout: false }
                }
            }
        });
        match sender.try_send(wrapped) {
            Ok(()) => Ok(Ticket { receiver: rx }),
            Err(TrySendError::Full(_)) => Err(PoolError::Full),
            Err(TrySendError::Disconnected(_)) => Err(PoolError::Closed),
        }
    }

    /// Submits and waits.
    pub fn run<R, J>(&self, job: J) -> Result<R, PoolError>
    where
        R: Send + 'static,
        J: FnOnce(&mut Emulator) -> Result<R, Error> + Send + 'static,
    {
        self.submit(job)?.wait()
    }

    pub fn stats(&self) -> PoolStats {
        self.shared.stats.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub fn workers(&self) -> usize {
        self.workers.len()
    }

    /// Lets queued jobs finish, then stops the workers.
    pub fn shutdown(&mut self) {
        self.sender = None;
        for w in self.workers.drain(..) {
            let _ = w.join();
        }
    }
}

impl Drop for Pool {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn worker(
    config: PoolConfig,
    factory: &(dyn Fn() -> Result<Emulator, Error> + Send + Sync),
    receiver: Arc<Mutex<Receiver<Job>>>,
    shared: Arc<Shared>,
    ready: mpsc::Sender<Result<(), Error>>,
) {
    let build = |shared: &Shared| -> Result<Emulator, Error> {
        let emu = catch_unwind(AssertUnwindSafe(factory)).unwrap_or_else(|_| Err(Error::Exited(-1)))?;
        shared.stats.lock().unwrap_or_else(|e| e.into_inner()).builds += 1;
        Ok(emu)
    };
    let mut emu = match build(&shared) {
        Ok(e) => {
            let _ = ready.send(Ok(()));
            Some(e)
        }
        Err(e) => {
            let _ = ready.send(Err(e));
            return;
        }
    };
    drop(ready);
    let mut jobs_on_this = 0u64;
    loop {
        let job = {
            let guard = receiver.lock().unwrap_or_else(|e| e.into_inner());
            match guard.recv() {
                Ok(job) => job,
                Err(_) => return,
            }
        };
        let current = match emu.as_mut() {
            Some(e) => e,
            None => match build(&shared) {
                Ok(e) => {
                    jobs_on_this = 0;
                    emu.insert(e)
                }
                Err(e) => {
                    // The job is dropped unrun; its ticket reports the drop. Count it as failed.
                    let mut stats = shared.stats.lock().unwrap_or_else(|e| e.into_inner());
                    stats.jobs += 1;
                    stats.failed += 1;
                    drop(stats);
                    eprintln!("[pool] emulator rebuild failed: {e}");
                    continue;
                }
            },
        };
        let outcome = job(current);
        jobs_on_this += 1;
        let mut recycle = false;
        {
            let mut stats = shared.stats.lock().unwrap_or_else(|e| e.into_inner());
            stats.jobs += 1;
            if let Outcome::Failed { poisoned, timeout } = outcome {
                stats.failed += 1;
                stats.timeouts += timeout as u64;
                recycle |= poisoned;
            }
        }
        let committed = emu.as_ref().map_or(0, |e| e.cpu.memory().committed());
        recycle |= config.recycle_after_jobs != 0 && jobs_on_this >= config.recycle_after_jobs;
        recycle |= config.recycle_above_committed != 0 && committed > config.recycle_above_committed;
        if recycle {
            emu = None;
            // Rebuild now, so the next job does not wait for it.
            if let Ok(e) = build(&shared) {
                emu = Some(e);
                jobs_on_this = 0;
            }
        }
    }
}

/// The longest a deadline can fire late.
const WATCH_TICK: Duration = Duration::from_millis(20);

/// One thread that fires interrupts at deadlines.
struct Watchdog {
    state: Arc<(Mutex<WatchState>, Condvar)>,
    next_id: AtomicU64,
}

#[derive(Default)]
struct WatchState {
    deadlines: BTreeMap<(Instant, u64), backend::Interrupt>,
    ids: BTreeMap<u64, Instant>,
}

impl Watchdog {
    fn start() -> Watchdog {
        let state: Arc<(Mutex<WatchState>, Condvar)> = Arc::default();
        let weak = Arc::downgrade(&state);
        std::thread::Builder::new()
            .name("emulator-watchdog".into())
            .spawn(move || {
                loop {
                    let Some(state) = weak.upgrade() else { return };
                    let (lock, condvar) = &*state;
                    let mut s = lock.lock().unwrap_or_else(|e| e.into_inner());
                    let now = Instant::now();
                    while let Some((&(at, id), _)) = s.deadlines.iter().next() {
                        if at > now {
                            break;
                        }
                        if let Some(interrupt) = s.deadlines.remove(&(at, id)) {
                            interrupt.interrupt();
                        }
                        s.ids.remove(&id);
                    }
                    let wait = s.deadlines.keys().next().map_or(WATCH_TICK, |(at, _)| at.saturating_duration_since(now));
                    // Sleeps to the next deadline or one tick; the tick also lets the thread see
                    // the pool go away.
                    let _ = condvar.wait_timeout(s, wait.min(WATCH_TICK));
                }
            })
            .expect("spawn watchdog");
        Watchdog { state, next_id: AtomicU64::new(0) }
    }

    fn arm(&self, at: Instant, interrupt: backend::Interrupt) -> u64 {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (lock, _) = &*self.state;
        let mut s = lock.lock().unwrap_or_else(|e| e.into_inner());
        // No wake-up: the watchdog looks at least every WATCH_TICK, which bounds how late a
        // deadline fires, and arming stays off the watchdog thread's schedule.
        s.deadlines.insert((at, id), interrupt);
        s.ids.insert(id, at);
        id
    }

    fn disarm(&self, id: u64) {
        let (lock, _) = &*self.state;
        let mut s = lock.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(at) = s.ids.remove(&id) {
            s.deadlines.remove(&(at, id));
        }
    }
}
