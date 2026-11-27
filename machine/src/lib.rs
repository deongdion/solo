//! Guest memory (pages, permissions, dirty tracking), registers and shared error types.
//!
//! Guest memory is owned here, not by a CPU backend. Each backend sees the same host pages: dynarmic
//! through a page table of host pointers, unicorn through `uc_mem_map_ptr`.

use std::alloc::{self, Layout};
use std::collections::BTreeMap;
use std::ptr::NonNull;

pub const PAGE_BITS: u32 = 12;
pub const PAGE_SIZE: u64 = 1 << PAGE_BITS;
const PAGE_MASK: u64 = PAGE_SIZE - 1;

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
    #[error("host allocation of {0:#x} bytes failed")]
    Allocation(u64),
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

/// One contiguous, page-aligned host allocation backing a guest range.
#[derive(Debug)]
pub struct Region {
    pub address: u64,
    pub length: u64,
    pub perms: Perms,
    host: NonNull<u8>,
}

impl Region {
    pub fn host_ptr(&self) -> *mut u8 {
        self.host.as_ptr()
    }

    fn layout(length: u64) -> Layout {
        Layout::from_size_align(length as usize, PAGE_SIZE as usize).expect("page aligned layout")
    }
}

impl Drop for Region {
    fn drop(&mut self) {
        // SAFETY: allocated in `Memory::map` with the same layout.
        unsafe { alloc::dealloc(self.host.as_ptr(), Region::layout(self.length)) };
    }
}

/// Guest address space with a flat page table of host pointers.
pub struct Memory {
    address_bits: u32,
    table: Box<[*mut u8]>,
    regions: BTreeMap<u64, Region>,
}

// SAFETY: the raw pointers only refer to allocations owned by this value.
unsafe impl Send for Memory {}

/// Allocates the page table with `alloc_zeroed`, so the OS hands out zero pages lazily instead of
/// the table being written entry by entry.
fn zeroed_table(entries: usize) -> Box<[*mut u8]> {
    let layout = Layout::array::<*mut u8>(entries).expect("page table layout");
    // SAFETY: non-zero size; a null pointer is all zero bits, so the zeroed memory is a valid slice.
    unsafe {
        let ptr = alloc::alloc_zeroed(layout) as *mut *mut u8;
        if ptr.is_null() {
            alloc::handle_alloc_error(layout);
        }
        Box::from_raw(std::ptr::slice_from_raw_parts_mut(ptr, entries))
    }
}

impl Memory {
    /// Creates an empty address space. A64 guests use 36 bits; A32 guests use 32 bits.
    pub fn new(address_bits: u32) -> Memory {
        assert!((PAGE_BITS + 1..=48).contains(&address_bits), "unsupported address space size");
        let entries = 1usize << (address_bits - PAGE_BITS);
        Memory { address_bits, table: zeroed_table(entries), regions: BTreeMap::new() }
    }

    pub fn address_bits(&self) -> u32 {
        self.address_bits
    }

    /// Host pointer per guest page, null when unmapped. Valid until the next map or unmap.
    pub fn page_table(&mut self) -> *mut *mut u8 {
        self.table.as_mut_ptr()
    }

    pub fn regions(&self) -> impl Iterator<Item = &Region> {
        self.regions.values()
    }

    /// Maps a zero-filled range.
    pub fn map(&mut self, address: u64, length: u64, perms: Perms) -> Result<&Region, Error> {
        if address & PAGE_MASK != 0 || length & PAGE_MASK != 0 || length == 0 {
            return Err(Error::Unaligned { address, length });
        }
        let end = address.checked_add(length).ok_or(Error::OutOfRange { address, length, bits: self.address_bits })?;
        if end > 1u64 << self.address_bits {
            return Err(Error::OutOfRange { address, length, bits: self.address_bits });
        }
        if self.overlaps(address, end) {
            return Err(Error::Overlap { address, length });
        }
        // SAFETY: non-zero size, valid alignment.
        let host = unsafe { alloc::alloc_zeroed(Region::layout(length)) };
        let host = NonNull::new(host).ok_or(Error::Allocation(length))?;
        for page in 0..length >> PAGE_BITS {
            let index = ((address >> PAGE_BITS) + page) as usize;
            // SAFETY: page lies within the allocation.
            self.table[index] = unsafe { host.as_ptr().add((page << PAGE_BITS) as usize) };
        }
        self.regions.insert(address, Region { address, length, perms, host });
        Ok(&self.regions[&address])
    }

    /// Unmaps a whole region previously returned by `map`.
    pub fn unmap(&mut self, address: u64) -> Result<(), Error> {
        let region = self.regions.remove(&address).ok_or(Error::Unmapped(address))?;
        for page in 0..region.length >> PAGE_BITS {
            self.table[((address >> PAGE_BITS) + page) as usize] = std::ptr::null_mut();
        }
        Ok(())
    }

    fn overlaps(&self, start: u64, end: u64) -> bool {
        self.regions.range(..end).next_back().is_some_and(|(_, r)| r.address + r.length > start)
    }

    fn host(&self, address: u64) -> Option<*mut u8> {
        let index = (address >> PAGE_BITS) as usize;
        let page = *self.table.get(index)?;
        // SAFETY: offset stays inside the page.
        (!page.is_null()).then(|| unsafe { page.add((address & PAGE_MASK) as usize) })
    }

    pub fn read(&self, address: u64, buf: &mut [u8]) -> Result<(), Error> {
        let mut done = 0usize;
        while done < buf.len() {
            let at = address + done as u64;
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
            let at = address + done as u64;
            let dst = self.host(at).ok_or(Error::Unmapped(at))?;
            let chunk = (data.len() - done).min((PAGE_SIZE - (at & PAGE_MASK)) as usize);
            // SAFETY: dst is valid for `chunk` bytes within one page.
            unsafe { std::ptr::copy_nonoverlapping(data[done..].as_ptr(), dst, chunk) };
            done += chunk;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn map_read_write_across_pages() {
        let mut memory = Memory::new(32);
        memory.map(0x1000, 0x2000, Perms::RW).unwrap();
        memory.write(0x1ffe, &[1, 2, 3, 4]).unwrap();
        let mut out = [0u8; 4];
        memory.read(0x1ffe, &mut out).unwrap();
        assert_eq!(out, [1, 2, 3, 4]);
        assert!(matches!(memory.read(0x3000, &mut out), Err(Error::Unmapped(0x3000))));
    }

    #[test]
    fn rejects_overlap_and_unaligned() {
        let mut memory = Memory::new(32);
        memory.map(0x4000, 0x2000, Perms::RW).unwrap();
        assert!(matches!(memory.map(0x5000, 0x1000, Perms::RW), Err(Error::Overlap { .. })));
        assert!(matches!(memory.map(0x3000, 0x2000, Perms::RW), Err(Error::Overlap { .. })));
        assert!(matches!(memory.map(0x8001, 0x1000, Perms::RW), Err(Error::Unaligned { .. })));
        memory.unmap(0x4000).unwrap();
        memory.map(0x5000, 0x1000, Perms::RW).unwrap();
    }
}
