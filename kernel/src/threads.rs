//! Guest threads on one CPU: clone, futex, exit and a deterministic round-robin scheduler.
//!
//! Threads run one at a time. A thread gives up the CPU when it waits on a futex, yields, sleeps,
//! exits, or when the runtime preempts it after an instruction slice. Waiting threads are woken by
//! FUTEX_WAKE in order of arrival. Time only passes when every thread waits: then the thread with
//! a timeout that was queued first times out. The same program and inputs give the same
//! interleaving on every run.

use std::collections::BTreeMap;

use backend::{Arch, Context, Cpu, Reg, load_context, save_context};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum State {
    Runnable,
    /// FUTEX_WAIT on `address` with `bitset`; `timed` when a timeout was given.
    Waiting { address: u64, bitset: u32, timed: bool, since: u64 },
    /// Reached the return trampoline of the host call at nesting `depth` while a deeper host call
    /// runs; it runs again (and returns) once that call level is the innermost.
    Parked { depth: u32 },
}

#[derive(Debug, Clone)]
pub struct Thread {
    pub tid: u32,
    pub state: State,
    /// Saved registers while another thread runs.
    context: Option<Context>,
    /// CLONE_CHILD_CLEARTID / set_tid_address: zeroed and woken on exit (pthread_join waits on it).
    pub clear_tid: u64,
}

/// What a thread operation did to the running thread.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Switch {
    /// The same thread continues.
    Stay,
    /// Another thread now runs; its registers are loaded.
    Switched,
    /// No thread can run and none can time out.
    Deadlock,
}

#[derive(Debug, Clone)]
pub struct Threads {
    pub threads: BTreeMap<u32, Thread>,
    pub current: u32,
    pub main: u32,
    next_tid: u32,
    /// Orders waiters for wake-ups and timeouts.
    wait_counter: u64,
    pub switches: u64,
    /// Host call nesting the runtime is at.
    pub depth: u32,
    /// The thread that made each host call level, outermost first.
    owners: Vec<u32>,
}

pub const CLONE_VM: u64 = 0x100;
pub const CLONE_SETTLS: u64 = 0x80000;
pub const CLONE_PARENT_SETTID: u64 = 0x100000;
pub const CLONE_CHILD_CLEARTID: u64 = 0x200000;
pub const CLONE_THREAD: u64 = 0x10000;
pub const CLONE_CHILD_SETTID: u64 = 0x1000000;

impl Threads {
    pub fn new(pid: u32) -> Threads {
        let mut threads = BTreeMap::new();
        threads.insert(pid, Thread { tid: pid, state: State::Runnable, context: None, clear_tid: 0 });
        Threads { threads, current: pid, main: pid, next_tid: pid + 1, wait_counter: 0, switches: 0, depth: 0, owners: Vec::new() }
    }

    pub fn count(&self) -> usize {
        self.threads.len()
    }

    /// Creates a thread for `clone`: a copy of the caller's registers that returns 0 on `stack`.
    /// The caller keeps running.
    pub fn spawn(&mut self, cpu: &mut dyn Cpu, flags: u64, stack: u64, parent_tid: u64, tls: u64, child_tid: u64) -> Result<u32, machine::Error> {
        let tid = self.next_tid;
        self.next_tid += 1;
        let arch = cpu.arch();
        let mut context = save_context(cpu);
        context.set(arch, Reg::General(0), 0);
        if stack != 0 {
            context.set(arch, Reg::Sp, stack);
        }
        if flags & CLONE_SETTLS != 0 {
            context.set(arch, Reg::Tls, tls);
        }
        if flags & CLONE_PARENT_SETTID != 0 && parent_tid != 0 {
            cpu.memory_mut().write_u32(parent_tid, tid)?;
        }
        if flags & CLONE_CHILD_SETTID != 0 && child_tid != 0 {
            cpu.memory_mut().write_u32(child_tid, tid)?;
        }
        let clear_tid = if flags & CLONE_CHILD_CLEARTID != 0 { child_tid } else { 0 };
        self.threads.insert(tid, Thread { tid, state: State::Runnable, context: Some(context), clear_tid });
        Ok(tid)
    }

    pub fn set_clear_tid(&mut self, address: u64) {
        if let Some(t) = self.threads.get_mut(&self.current) {
            t.clear_tid = address;
        }
    }

    /// Puts the current thread to sleep on a futex and switches away. The result register of the
    /// waiter is set when it is woken (0) or times out (-ETIMEDOUT).
    pub fn wait(&mut self, cpu: &mut dyn Cpu, address: u64, bitset: u32, timed: bool) -> Switch {
        self.wait_counter += 1;
        let since = self.wait_counter;
        if let Some(t) = self.threads.get_mut(&self.current) {
            t.state = State::Waiting { address, bitset, timed, since };
        }
        self.reschedule(cpu)
    }

    /// Wakes up to `count` waiters on `address` whose bitset intersects `bitset`; returns how many.
    pub fn wake(&mut self, cpu: &dyn Cpu, address: u64, bitset: u32, count: u64) -> u64 {
        let arch = cpu.arch();
        let mut waiters: Vec<(u64, u32)> = self
            .threads
            .values()
            .filter_map(|t| match t.state {
                State::Waiting { address: a, bitset: b, since, .. } if a == address && b & bitset != 0 => Some((since, t.tid)),
                _ => None,
            })
            .collect();
        waiters.sort_unstable();
        let mut woken = 0;
        for (_, tid) in waiters.into_iter().take(count.min(u32::MAX as u64) as usize) {
            let t = self.threads.get_mut(&tid).expect("listed");
            t.state = State::Runnable;
            if let Some(c) = t.context.as_mut() {
                c.set(arch, Reg::General(0), 0);
            }
            woken += 1;
        }
        woken
    }

    /// Moves up to `count` waiters from `from` to `to`; returns how many.
    pub fn requeue(&mut self, from: u64, to: u64, count: u64) -> u64 {
        let mut moved = 0;
        let mut waiters: Vec<(u64, u32)> = self
            .threads
            .values()
            .filter_map(|t| match t.state {
                State::Waiting { address, since, .. } if address == from => Some((since, t.tid)),
                _ => None,
            })
            .collect();
        waiters.sort_unstable();
        for (_, tid) in waiters.into_iter().take(count.min(u32::MAX as u64) as usize) {
            if let State::Waiting { address, .. } = &mut self.threads.get_mut(&tid).expect("listed").state {
                *address = to;
                moved += 1;
            }
        }
        moved
    }

    /// Ends the current thread: clears and wakes its clear_tid word, then switches away.
    pub fn exit(&mut self, cpu: &mut dyn Cpu) -> Result<Switch, machine::Error> {
        let tid = self.current;
        let clear_tid = self.threads.get(&tid).map_or(0, |t| t.clear_tid);
        if clear_tid != 0 {
            cpu.memory_mut().write_u32(clear_tid, 0)?;
            self.wake(cpu, clear_tid, u32::MAX, u64::MAX);
        }
        self.threads.remove(&tid);
        Ok(self.switch_to_next(cpu, false))
    }

    /// The current thread reached the trampoline of an outer host call (`return_pc` is that
    /// trampoline): it waits there until its call level is the innermost again.
    pub fn park(&mut self, cpu: &mut dyn Cpu, return_pc: u64) -> Switch {
        let arch = cpu.arch();
        let mut context = save_context(cpu);
        context.set(arch, Reg::Pc, return_pc);
        let owner_depth = self.owner_depth(self.current);
        if let Some(t) = self.threads.get_mut(&self.current) {
            t.state = State::Parked { depth: owner_depth };
            t.context = Some(context);
        }
        self.switch_to_next(cpu, false)
    }

    /// Call level a parked thread belongs to: the runtime records owners per level.
    fn owner_depth(&self, tid: u32) -> u32 {
        self.owners.iter().rposition(|&t| t == tid).map_or(0, |i| i as u32 + 1)
    }

    /// The runtime enters a host call level run by the current thread.
    pub fn enter(&mut self) {
        self.owners.push(self.current);
        self.depth = self.owners.len() as u32;
    }

    pub fn leave(&mut self) {
        self.owners.pop();
        self.depth = self.owners.len() as u32;
    }

    /// The thread whose call the innermost level runs.
    pub fn owner(&self) -> Option<u32> {
        self.owners.last().copied()
    }

    /// Gives the CPU to the next runnable thread, if any (sched_yield, preemption).
    pub fn yield_now(&mut self, cpu: &mut dyn Cpu) -> Switch {
        self.reschedule(cpu)
    }

    /// Makes `tid` the running thread (the runtime starts every call on the main thread).
    pub fn run(&mut self, cpu: &mut dyn Cpu, tid: u32) {
        if self.current == tid || !self.threads.contains_key(&tid) {
            return;
        }
        if let Some(t) = self.threads.get_mut(&self.current) {
            t.context = Some(save_context(cpu));
        }
        self.load(cpu, tid);
    }

    fn load(&mut self, cpu: &mut dyn Cpu, tid: u32) {
        let t = self.threads.get_mut(&tid).expect("exists");
        if let State::Parked { .. } = t.state {
            t.state = State::Runnable;
        }
        if let Some(context) = t.context.take() {
            load_context(cpu, &context);
        }
        self.current = tid;
        self.switches += 1;
    }

    /// The next runnable thread after the current one in tid order, wrapping around.
    fn next_runnable(&self) -> Option<u32> {
        let after = self.threads.range(self.current + 1..).map(|(_, t)| t);
        let before = self.threads.range(..=self.current).map(|(_, t)| t);
        let depth = self.depth;
        after.chain(before).find(|t| t.state == State::Runnable || t.state == State::Parked { depth }).map(|t| t.tid)
    }

    fn reschedule(&mut self, cpu: &mut dyn Cpu) -> Switch {
        self.switch_to_next(cpu, true)
    }

    /// `save`: the current thread still exists and keeps its registers.
    fn switch_to_next(&mut self, cpu: &mut dyn Cpu, save: bool) -> Switch {
        if save {
            let context = save_context(cpu);
            if let Some(t) = self.threads.get_mut(&self.current) {
                t.context = Some(context);
            }
        }
        let arch = cpu.arch();
        let Some(next) = self.next_runnable().or_else(|| self.time_out_first(arch)) else {
            if let Some(t) = self.threads.get_mut(&self.current) {
                t.context = None;
            }
            return Switch::Deadlock;
        };
        if save && next == self.current {
            // Reload: a timeout may have set the result register.
            let t = self.threads.get_mut(&next).expect("exists");
            if let Some(c) = t.context.take() {
                load_context(cpu, &c);
            }
            return Switch::Stay;
        }
        self.load(cpu, next);
        Switch::Switched
    }

    /// Every thread waits: the earliest timed waiter times out (time passes only now).
    fn time_out_first(&mut self, arch: Arch) -> Option<u32> {
        let tid = self
            .threads
            .values()
            .filter_map(|t| match t.state {
                State::Waiting { timed: true, since, .. } => Some((since, t.tid)),
                _ => None,
            })
            .min()?
            .1;
        let t = self.threads.get_mut(&tid).expect("listed");
        t.state = State::Runnable;
        let etimedout = 110u64;
        let value = match arch {
            Arch::Arm64 => (-(etimedout as i64)) as u64,
            Arch::Arm32 => (-(etimedout as i64)) as u64 & 0xffff_ffff,
        };
        if let Some(c) = t.context.as_mut() {
            c.set(arch, Reg::General(0), value);
        }
        Some(tid)
    }
}
