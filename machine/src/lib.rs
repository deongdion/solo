//! Guest memory (pages, permissions, mappings), registers and shared error types.
//!
//! Guest memory is owned here, not by a CPU backend. Each backend sees the same host pages: dynarmic
//! through a page table of host pointers, unicorn through `uc_mem_map_ptr`.
//!
//! Host memory is allocated per `map` call. Unmapping part of a mapping clears its pages from the
//! page table at once; the host allocation is freed when its last page is gone.

use std::alloc::{self, Layout};
use std::collections::BTreeMap;
use std::ptr::NonNull;

pub const PAGE_BITS: u32 = 12;
pub const PAGE_SIZE: u64 = 1 << PAGE_BITS;
const PAGE_MASK: u64 = PAGE_SIZE - 1;

pub fn page_floor(value: u64) -> u64 {
    value & !PAGE_MASK
}

pub fn page_ceil(value: u64) -> u64 {
    (value + PAGE_MASK) & !PAGE_MASK
}

/// Errors raised by guest memory operations.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("address {0:#x} is not mapped")]
    Unmapped(u64),
    #[error("range {address:#x}+{length:#x} overlaps an existing mapping")]
    Overlap { address: u64, length: u64 },
    #[error("range {address:#x}+{length:#x} is not page aligned")]
    Unaligned { address: u64, length: u64 },
    #[error("range {address:#x}+{length:#x} is outside the {bits}-bit address space")]
    OutOfRange { address: u64, length: u64, bits: u32 },
    #[error("no free range of {0:#x} bytes")]
    NoSpace(u64),
    #[error("mapping {requested:#x} bytes would exceed the {limit:#x}-byte host memory limit")]
    Limit { requested: u64, limit: u64 },
    #[error("string at {0:#x} is not terminated within {1} bytes")]
    Unterminated(u64, usize),
}

/// Page permissions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Perms(u8);

impl Perms {
    pub const NONE: Perms = Perms(0);
    pub const READ: Perms = Perms(1);
    pub const WRITE: Perms = Perms(2);
    pub const EXEC: Perms = Perms(4);
    pub const RW: Perms = Perms(3);
    pub const RX: Perms = Perms(5);
    pub const RWX: Perms = Perms(7);

    pub const fn from_bits(bits: u8) -> Perms {
        Perms(bits & 7)
    }

    pub const fn bits(self) -> u8 {
        self.0
    }

    pub const fn contains(self, other: Perms) -> bool {
        self.0 & other.0 == other.0
    }
}

impl std::ops::BitOr for Perms {
    type Output = Perms;
    fn bitor(self, rhs: Perms) -> Perms {
        Perms(self.0 | rhs.0)
    }
}

/// One mapped range with uniform permissions, as `/proc/self/maps` shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mapping {
    pub start: u64,
    pub end: u64,
    pub perms: Perms,
    pub label: String,
}

struct Allocation {
    host: NonNull<u8>,
    length: u64,
    live_pages: u64,
}

impl Allocation {
    fn layout(length: u64) -> Layout {
        Layout::from_size_align(length as usize, PAGE_SIZE as usize).expect("page aligned layout")
    }
}

impl Drop for Allocation {
    fn drop(&mut self) {
        // SAFETY: allocated in `Memory::map` with the same layout.
        unsafe { alloc::dealloc(self.host.as_ptr(), Allocation::layout(self.length)) };
    }
}

/// Allocates a zeroed table; the OS hands out zero pages lazily instead of the table being written
/// entry by entry (measured: 20 ms to 15 us for a 36-bit table).
fn zeroed_table<T>(entries: usize) -> Box<[T]> {
    let layout = Layout::array::<T>(entries).expect("table layout");
    // SAFETY: non-zero size; callers only use T for which all-zero bits is a valid value.
    unsafe {
        let ptr = alloc::alloc_zeroed(layout) as *mut T;
        if ptr.is_null() {
            alloc::handle_alloc_error(layout);
        }
        Box::from_raw(std::ptr::slice_from_raw_parts_mut(ptr, entries))
    }
}

/// Guest address space with a flat page table of host pointers.
pub struct Memory {
    address_bits: u32,
    table: Box<[*mut u8]>,
    /// Keyed by host address: a guest range can be partly unmapped and remapped while the rest of
    /// its allocation stays live, so guest addresses do not identify allocations.
    allocations: BTreeMap<usize, Allocation>,
    mappings: BTreeMap<u64, Mapping>,
    /// Host bytes currently allocated for guest pages.
    committed: u64,
    /// Upper bound for `committed`; a guest that maps more gets an error instead of exhausting the host.
    limit: u64,
    /// Watched ranges: their pages are absent from `table` (so a JIT reading the table takes its
    /// slow path, where the access can be recorded) and kept in `watched` by page index.
    watch_ranges: Vec<(u64, u64)>,
    watched: BTreeMap<usize, *mut u8>,
}

/// Default host memory limit per address space.
pub const DEFAULT_LIMIT: u64 = 4 << 30;

// SAFETY: the raw pointers only refer to allocations owned by this value.
unsafe impl Send for Memory {}

impl Memory {
    /// Creates an empty address space. A64 guests use 36 bits; A32 guests use 32 bits.
    pub fn new(address_bits: u32) -> Memory {
        assert!((PAGE_BITS + 1..=48).contains(&address_bits), "unsupported address space size");
        Memory {
            address_bits,
            table: zeroed_table(1usize << (address_bits - PAGE_BITS)),
            allocations: BTreeMap::new(),
            mappings: BTreeMap::new(),
            committed: 0,
            limit: DEFAULT_LIMIT,
            watch_ranges: Vec::new(),
            watched: BTreeMap::new(),
        }
    }

    pub fn set_limit(&mut self, bytes: u64) {
        self.limit = bytes;
    }

    /// Host bytes allocated for guest pages.
    pub fn committed(&self) -> u64 {
        self.committed
    }

    pub fn address_bits(&self) -> u32 {
        self.address_bits
    }

    /// Host pointer per guest page, null when unmapped. The table itself never moves.
    pub fn page_table(&mut self) -> *mut *mut u8 {
        self.table.as_mut_ptr()
    }

    pub fn mappings(&self) -> impl Iterator<Item = &Mapping> {
        self.mappings.values()
    }

    /// Host address of a guest page, for backends that map pages themselves.
    pub fn host_page(&self, address: u64) -> Option<*mut u8> {
        let page = self.page_ptr((address >> PAGE_BITS) as usize);
        (!page.is_null()).then_some(page)
    }

    fn page_ptr(&self, index: usize) -> *mut u8 {
        match self.table.get(index) {
            Some(page) if !page.is_null() => *page,
            Some(_) => self.watched.get(&index).copied().unwrap_or(std::ptr::null_mut()),
            None => std::ptr::null_mut(),
        }
    }

    /// Takes the pages of `[start, end)`, mapped now or later, out of the page table so that a
    /// JIT's accesses to them go through its slow path (see `watched_access`). Host-side reads
    /// and writes are unaffected.
    pub fn watch(&mut self, start: u64, end: u64) {
        self.watch_ranges.push((start, end));
        let first = (start >> PAGE_BITS) as usize;
        let last = (page_ceil(end) >> PAGE_BITS) as usize;
        for index in first..last.min(self.table.len()) {
            self.watch_page(index);
        }
    }

    fn watch_page(&mut self, index: usize) {
        let page = std::mem::replace(&mut self.table[index], std::ptr::null_mut());
        if !page.is_null() {
            self.watched.insert(index, page);
        }
    }

    /// Puts every watched page back into the page table.
    pub fn unwatch_all(&mut self) {
        for (index, page) in std::mem::take(&mut self.watched) {
            self.table[index] = page;
        }
        self.watch_ranges.clear();
    }

    /// Whether `[address, address + size)` overlaps a watched range (finer than the pages).
    pub fn in_watch_range(&self, address: u64, size: u64) -> bool {
        self.watch_ranges.iter().any(|(s, e)| address < *e && address + size > *s)
    }

    pub fn is_watched(&self, address: u64) -> bool {
        self.watched.contains_key(&((address >> PAGE_BITS) as usize))
    }

    /// The slow-path access of a JIT: `kind` 0 reads into `data`, 1 writes `data`, 2 fetches code
    /// (a read). False when a byte is not mapped. Whether the access touched a watched page is
    /// for the caller to check with `is_watched`.
    ///
    /// # Safety
    /// Writes through the page pointers while `self` is borrowed shared: no other reference to
    /// the written bytes may be live (the JIT calls this while it is the only user of memory).
    pub unsafe fn watched_access(&self, address: u64, kind: u32, data: &mut [u8]) -> bool {
        let mut done = 0usize;
        while done < data.len() {
            let at = address.wrapping_add(done as u64);
            let Some(host) = self.host(at) else { return false };
            let chunk = (data.len() - done).min((PAGE_SIZE - (at & PAGE_MASK)) as usize);
            // SAFETY: host is valid for `chunk` bytes within one page; see the function's contract.
            unsafe {
                if kind == 1 {
                    std::ptr::copy_nonoverlapping(data[done..].as_ptr(), host, chunk);
                } else {
                    std::ptr::copy_nonoverlapping(host, data[done..].as_mut_ptr(), chunk);
                }
            }
            done += chunk;
        }
        true
    }

    fn check_range(&self, address: u64, length: u64) -> Result<u64, Error> {
        if address & PAGE_MASK != 0 || length & PAGE_MASK != 0 || length == 0 {
            return Err(Error::Unaligned { address, length });
        }
        let end = address.checked_add(length).ok_or(Error::OutOfRange { address, length, bits: self.address_bits })?;
        if end > 1u64 << self.address_bits {
            return Err(Error::OutOfRange { address, length, bits: self.address_bits });
        }
        Ok(end)
    }

    pub fn is_free(&self, address: u64, length: u64) -> bool {
        let end = address + length;
        !self.mappings.range(..end).next_back().is_some_and(|(_, m)| m.end > address)
    }

    /// Lowest free, page-aligned range of `length` bytes within `[low, high)`.
    pub fn find_free(&self, length: u64, low: u64, high: u64) -> Result<u64, Error> {
        let length = page_ceil(length);
        let mut candidate = page_ceil(low);
        for m in self.mappings.range(..high).map(|(_, m)| m) {
            if m.end <= candidate {
                continue;
            }
            if m.start >= candidate + length {
                break;
            }
            candidate = m.end;
        }
        if candidate + length <= high { Ok(candidate) } else { Err(Error::NoSpace(length)) }
    }

    /// Maps a zero-filled range. Fails if any page of it is already mapped.
    pub fn map(&mut self, address: u64, length: u64, perms: Perms, label: &str) -> Result<*mut u8, Error> {
        let end = self.check_range(address, length)?;
        if !self.is_free(address, length) {
            return Err(Error::Overlap { address, length });
        }
        if self.committed.saturating_add(length) > self.limit {
            return Err(Error::Limit { requested: length, limit: self.limit });
        }
        let layout = Allocation::layout(length);
        // SAFETY: non-zero size, valid alignment.
        let host = NonNull::new(unsafe { alloc::alloc_zeroed(layout) }).unwrap_or_else(|| alloc::handle_alloc_error(layout));
        for page in 0..length >> PAGE_BITS {
            // SAFETY: page lies within the allocation.
            self.table[((address >> PAGE_BITS) + page) as usize] = unsafe { host.as_ptr().add((page << PAGE_BITS) as usize) };
        }
        if self.watch_ranges.iter().any(|(s, e)| *s < end && *e > address) {
            for page in (address..end).step_by(PAGE_SIZE as usize) {
                if self.watch_ranges.iter().any(|(s, e)| page < *e && page + PAGE_SIZE > *s) {
                    self.watch_page((page >> PAGE_BITS) as usize);
                }
            }
        }
        self.allocations.insert(host.as_ptr() as usize, Allocation { host, length, live_pages: length >> PAGE_BITS });
        self.committed += length;
        self.mappings.insert(address, Mapping { start: address, end, perms, label: label.to_string() });
        self.merge_around(address, end);
        Ok(host.as_ptr())
    }

    /// Unmaps every mapped page in a range; unmapped pages inside it are ignored, as munmap does.
    pub fn unmap(&mut self, address: u64, length: u64) -> Result<(), Error> {
        let end = self.check_range(address, length)?;
        self.split_at(address);
        self.split_at(end);
        let starts: Vec<u64> = self.mappings.range(address..end).map(|(s, _)| *s).collect();
        for start in starts {
            let m = self.mappings.remove(&start).expect("mapping");
            for page in (m.start..m.end).step_by(PAGE_SIZE as usize) {
                self.release_page(page);
            }
        }
        Ok(())
    }

    /// Changes permissions of a fully mapped range.
    pub fn protect(&mut self, address: u64, length: u64, perms: Perms) -> Result<(), Error> {
        let end = self.check_range(address, length)?;
        let mut cursor = address;
        for m in self.mappings.range(..end).map(|(_, m)| m) {
            if m.end <= cursor {
                continue;
            }
            if m.start > cursor {
                return Err(Error::Unmapped(cursor));
            }
            cursor = m.end;
        }
        if cursor < end {
            return Err(Error::Unmapped(cursor));
        }
        self.split_at(address);
        self.split_at(end);
        for (_, m) in self.mappings.range_mut(address..end) {
            m.perms = perms;
        }
        self.merge_around(address, end);
        Ok(())
    }

    /// Renames the mappings inside a range (labels show up in `/proc/self/maps`).
    pub fn label(&mut self, address: u64, length: u64, label: &str) {
        let end = address + length;
        self.split_at(address);
        self.split_at(end);
        for (_, m) in self.mappings.range_mut(address..end) {
            m.label = label.to_string();
        }
        self.merge_around(address, end);
    }

    pub fn mapping_at(&self, address: u64) -> Option<&Mapping> {
        self.mappings.range(..=address).next_back().map(|(_, m)| m).filter(|m| m.end > address)
    }

    fn release_page(&mut self, page: u64) {
        let index = (page >> PAGE_BITS) as usize;
        let host = match std::mem::replace(&mut self.table[index], std::ptr::null_mut()) {
            p if !p.is_null() => p as usize,
            _ => match self.watched.remove(&index) {
                Some(p) => p as usize,
                None => return,
            },
        };
        let start = *self.allocations.range(..=host).next_back().expect("allocation").0;
        let allocation = self.allocations.get_mut(&start).expect("allocation");
        allocation.live_pages -= 1;
        if allocation.live_pages == 0 {
            let length = allocation.length;
            self.allocations.remove(&start);
            self.committed -= length;
        }
    }

    fn split_at(&mut self, address: u64) {
        let Some((&start, m)) = self.mappings.range(..address).next_back() else { return };
        if m.end <= address {
            return;
        }
        let mut tail = m.clone();
        tail.start = address;
        self.mappings.get_mut(&start).expect("mapping").end = address;
        self.mappings.insert(address, tail);
    }

    fn merge_around(&mut self, start: u64, end: u64) {
        let first = self.mappings.range(..start).next_back().map(|(s, _)| *s).unwrap_or(start);
        let keys: Vec<u64> = self.mappings.range(first..=end).map(|(s, _)| *s).collect();
        let mut left_key = match keys.first() {
            Some(k) => *k,
            None => return,
        };
        for &right_key in &keys[1..] {
            let left = &self.mappings[&left_key];
            let right = &self.mappings[&right_key];
            if left.end == right.start && left.perms == right.perms && left.label == right.label {
                let right_end = right.end;
                self.mappings.remove(&right_key);
                self.mappings.get_mut(&left_key).expect("mapping").end = right_end;
            } else {
                left_key = right_key;
            }
        }
    }

    fn host(&self, address: u64) -> Option<*mut u8> {
        let page = self.page_ptr((address >> PAGE_BITS) as usize);
        // SAFETY: offset stays inside the page.
        (!page.is_null()).then(|| unsafe { page.add((address & PAGE_MASK) as usize) })
    }

    pub fn read(&self, address: u64, buf: &mut [u8]) -> Result<(), Error> {
        let mut done = 0usize;
        while done < buf.len() {
            let at = address.wrapping_add(done as u64);
            let src = self.host(at).ok_or(Error::Unmapped(at))?;
            let chunk = (buf.len() - done).min((PAGE_SIZE - (at & PAGE_MASK)) as usize);
            // SAFETY: src is valid for `chunk` bytes within one page.
            unsafe { std::ptr::copy_nonoverlapping(src, buf[done..].as_mut_ptr(), chunk) };
            done += chunk;
        }
        Ok(())
    }

    pub fn write(&mut self, address: u64, data: &[u8]) -> Result<(), Error> {
        let mut done = 0usize;
        while done < data.len() {
            let at = address.wrapping_add(done as u64);
            let dst = self.host(at).ok_or(Error::Unmapped(at))?;
            let chunk = (data.len() - done).min((PAGE_SIZE - (at & PAGE_MASK)) as usize);
            // SAFETY: dst is valid for `chunk` bytes within one page.
            unsafe { std::ptr::copy_nonoverlapping(data[done..].as_ptr(), dst, chunk) };
            done += chunk;
        }
        Ok(())
    }

    pub fn read_vec(&self, address: u64, length: usize) -> Result<Vec<u8>, Error> {
        let mut buf = vec![0u8; length];
        self.read(address, &mut buf)?;
        Ok(buf)
    }

    pub fn read_u32(&self, address: u64) -> Result<u32, Error> {
        let mut b = [0u8; 4];
        self.read(address, &mut b)?;
        Ok(u32::from_le_bytes(b))
    }

    pub fn read_u64(&self, address: u64) -> Result<u64, Error> {
        let mut b = [0u8; 8];
        self.read(address, &mut b)?;
        Ok(u64::from_le_bytes(b))
    }

    pub fn write_u32(&mut self, address: u64, value: u32) -> Result<(), Error> {
        self.write(address, &value.to_le_bytes())
    }

    pub fn write_u64(&mut self, address: u64, value: u64) -> Result<(), Error> {
        self.write(address, &value.to_le_bytes())
    }

    /// Reads a NUL-terminated string of at most `limit` bytes.
    pub fn read_cstr(&self, address: u64, limit: usize) -> Result<Vec<u8>, Error> {
        let mut out = Vec::new();
        let mut at = address;
        while out.len() < limit {
            let src = self.host(at).ok_or(Error::Unmapped(at))?;
            let room = ((PAGE_SIZE - (at & PAGE_MASK)) as usize).min(limit - out.len());
            // SAFETY: src is valid for `room` bytes within one page.
            let bytes = unsafe { std::slice::from_raw_parts(src, room) };
            if let Some(nul) = bytes.iter().position(|b| *b == 0) {
                out.extend_from_slice(&bytes[..nul]);
                return Ok(out);
            }
            out.extend_from_slice(bytes);
            at += room as u64;
        }
        Err(Error::Unterminated(address, limit))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn watched_pages_leave_the_table_but_stay_readable() {
        let mut m = Memory::new(32);
        m.map(0x10000, 0x3000, Perms::RW, "a").unwrap();
        m.write(0x11000, b"abc").unwrap();
        m.watch(0x11000, 0x11004);
        let table = m.page_table();
        // SAFETY: indexes within the table.
        unsafe {
            assert!(!(*table.add(0x10)).is_null());
            assert!((*table.add(0x11)).is_null());
            assert!(!(*table.add(0x12)).is_null());
        }
        assert!(m.is_watched(0x11fff) && !m.is_watched(0x12000));
        assert_eq!(m.read_vec(0x11000, 3).unwrap(), b"abc");
        let mut data = *b"xy";
        // SAFETY: no other reference to the bytes.
        assert!(unsafe { m.watched_access(0x11001, 1, &mut data) });
        assert_eq!(m.read_vec(0x11000, 3).unwrap(), b"axy");
        // A page mapped later inside a watched range is watched too.
        m.watch(0x20000, 0x21000);
        m.map(0x20000, 0x1000, Perms::RW, "b").unwrap();
        assert!(m.is_watched(0x20000));
        // Unmapping a watched page releases it; unwatching restores the rest.
        m.unmap(0x20000, 0x1000).unwrap();
        assert!(!m.is_watched(0x20000));
        m.unwatch_all();
        // SAFETY: as above.
        unsafe { assert!(!(*m.page_table().add(0x11)).is_null()) };
        assert_eq!(m.committed(), 0x3000);
    }

    #[test]
    fn map_read_write_across_pages() {
        let mut memory = Memory::new(32);
        memory.map(0x1000, 0x2000, Perms::RW, "a").unwrap();
        memory.write(0x1ffe, &[1, 2, 3, 4]).unwrap();
        let mut out = [0u8; 4];
        memory.read(0x1ffe, &mut out).unwrap();
        assert_eq!(out, [1, 2, 3, 4]);
        assert!(matches!(memory.read(0x3000, &mut out), Err(Error::Unmapped(0x3000))));
    }

    #[test]
    fn rejects_overlap_and_unaligned() {
        let mut memory = Memory::new(32);
        memory.map(0x4000, 0x2000, Perms::RW, "").unwrap();
        assert!(matches!(memory.map(0x5000, 0x1000, Perms::RW, ""), Err(Error::Overlap { .. })));
        assert!(matches!(memory.map(0x3000, 0x2000, Perms::RW, ""), Err(Error::Overlap { .. })));
        assert!(matches!(memory.map(0x8001, 0x1000, Perms::RW, ""), Err(Error::Unaligned { .. })));
    }

    #[test]
    fn partial_unmap_keeps_the_rest() {
        let mut memory = Memory::new(32);
        memory.map(0x10000, 0x4000, Perms::RW, "heap").unwrap();
        memory.write(0x13000, &[9]).unwrap();
        memory.unmap(0x11000, 0x2000).unwrap();
        assert!(memory.read_vec(0x11000, 1).is_err());
        assert!(memory.read_vec(0x12fff, 1).is_err());
        assert_eq!(memory.read_vec(0x13000, 1).unwrap(), [9]);
        let maps: Vec<(u64, u64)> = memory.mappings().map(|m| (m.start, m.end)).collect();
        assert_eq!(maps, [(0x10000, 0x11000), (0x13000, 0x14000)]);
        // The freed hole is reusable and comes back zeroed.
        memory.map(0x11000, 0x1000, Perms::RW, "heap").unwrap();
        assert_eq!(memory.read_vec(0x11000, 4).unwrap(), [0; 4]);
        memory.unmap(0x10000, 0x4000).unwrap();
        assert_eq!(memory.mappings().count(), 0);
        assert!(memory.allocations.is_empty());
    }

    #[test]
    fn protect_splits_and_merges() {
        let mut memory = Memory::new(32);
        memory.map(0x20000, 0x3000, Perms::RW, "x").unwrap();
        memory.protect(0x21000, 0x1000, Perms::READ).unwrap();
        assert_eq!(memory.mappings().count(), 3);
        memory.protect(0x21000, 0x1000, Perms::RW).unwrap();
        assert_eq!(memory.mappings().count(), 1);
        assert!(memory.protect(0x22000, 0x2000, Perms::RW).is_err());
    }

    #[test]
    fn find_free_skips_mappings() {
        let mut memory = Memory::new(32);
        memory.map(0x40000, 0x1000, Perms::RW, "").unwrap();
        memory.map(0x42000, 0x1000, Perms::RW, "").unwrap();
        assert_eq!(memory.find_free(0x1000, 0x40000, 0x100000).unwrap(), 0x41000);
        assert_eq!(memory.find_free(0x2000, 0x40000, 0x100000).unwrap(), 0x43000);
    }

    #[test]
    fn reads_c_strings_across_pages() {
        let mut memory = Memory::new(32);
        memory.map(0x1000, 0x2000, Perms::RW, "").unwrap();
        memory.write(0x1ffd, b"abc\0").unwrap();
        assert_eq!(memory.read_cstr(0x1ffd, 16).unwrap(), b"abc");
        assert!(matches!(memory.read_cstr(0x1ffd, 2), Err(Error::Unterminated(..))));
    }

    #[test]
    fn remap_inside_partly_unmapped_allocation_keeps_the_rest_alive() {
        let mut memory = Memory::new(32);
        let base = 0x1000_0000;
        memory.map(base, 0x8000, Perms::RW, "a").unwrap();
        memory.write_u32(base + 0x7000, 0x1234_5678).unwrap();
        memory.unmap(base, 0x2000).unwrap();
        memory.map(base, 0x1000, Perms::RW, "b").unwrap();
        memory.write_u32(base, 1).unwrap();
        assert_eq!(memory.read_u32(base + 0x7000).unwrap(), 0x1234_5678);
        memory.unmap(base, 0x8000).unwrap();
        assert_eq!(memory.committed(), 0);
        assert!(memory.allocations.is_empty());
    }
}
