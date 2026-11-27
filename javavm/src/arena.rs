//! Guest memory for buffers JNI hands out (`GetStringUTFChars`, `GetByteArrayElements`, ...),
//! released by the matching `Release` call.
//!
//! Small blocks come from 1 MiB chunks with per-size free lists; blocks of 256 KiB or more get
//! their own mapping and are unmapped on release, so long runs do not grow without bound.

use std::collections::{BTreeMap, HashMap};

use backend::Cpu;
use machine::Perms;

const CHUNK: u64 = 1 << 20;
const LARGE: u64 = 256 << 10;
const ALIGN: u64 = 16;

#[derive(Clone)]
pub struct Arena {
    low: u64,
    high: u64,
    /// Current chunk: next free byte and end.
    cursor: u64,
    end: u64,
    free: HashMap<u64, Vec<u64>>,
    /// Live blocks: address to rounded size.
    live: BTreeMap<u64, u64>,
    large: HashMap<u64, u64>,
}

#[derive(Debug, thiserror::Error)]
pub enum ArenaError {
    #[error("no guest address space for a {0}-byte JNI buffer")]
    NoSpace(u64),
    #[error(transparent)]
    Backend(#[from] backend::Error),
}

impl Arena {
    /// Blocks are placed in `[low, high)`.
    pub fn new(low: u64, high: u64) -> Arena {
        Arena { low, high, cursor: 0, end: 0, free: HashMap::new(), live: BTreeMap::new(), large: HashMap::new() }
    }

    pub fn alloc(&mut self, cpu: &mut dyn Cpu, size: u64) -> Result<u64, ArenaError> {
        let rounded = (size.max(1) + ALIGN - 1) & !(ALIGN - 1);
        if rounded >= LARGE {
            let mapped = machine::page_ceil(rounded);
            let at = cpu.memory().find_free(mapped, self.low, self.high).map_err(|_| ArenaError::NoSpace(size))?;
            cpu.map(at, mapped, Perms::RW, "[jni]")?;
            self.large.insert(at, mapped);
            return Ok(at);
        }
        if let Some(at) = self.free.get_mut(&rounded).and_then(Vec::pop) {
            self.live.insert(at, rounded);
            return Ok(at);
        }
        if self.cursor + rounded > self.end {
            let at = cpu.memory().find_free(CHUNK, self.low, self.high).map_err(|_| ArenaError::NoSpace(size))?;
            cpu.map(at, CHUNK, Perms::RW, "[jni]")?;
            self.cursor = at;
            self.end = at + CHUNK;
        }
        let at = self.cursor;
        self.cursor += rounded;
        self.live.insert(at, rounded);
        Ok(at)
    }

    /// Allocates and fills a block.
    pub fn alloc_bytes(&mut self, cpu: &mut dyn Cpu, bytes: &[u8]) -> Result<u64, ArenaError> {
        let at = self.alloc(cpu, bytes.len() as u64)?;
        cpu.memory_mut().write(at, bytes).map_err(|e| ArenaError::Backend(backend::Error::Memory(e)))?;
        Ok(at)
    }

    /// Size of a live block, if `at` is one.
    pub fn size(&self, at: u64) -> Option<u64> {
        self.live.get(&at).copied().or_else(|| self.large.get(&at).copied())
    }

    /// Returns a block. False if `at` is not a live block (double release or a foreign pointer).
    pub fn release(&mut self, cpu: &mut dyn Cpu, at: u64) -> bool {
        if let Some(size) = self.large.remove(&at) {
            let _ = cpu.unmap(at, size);
            return true;
        }
        match self.live.remove(&at) {
            Some(size) => {
                self.free.entry(size).or_default().push(at);
                true
            }
            None => false,
        }
    }

    pub fn live_blocks(&self) -> usize {
        self.live.len() + self.large.len()
    }
}
