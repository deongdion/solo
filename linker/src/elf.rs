//! ELF parsing for 32-bit ARM and 64-bit AArch64 shared objects.

use crate::Error;

pub const EM_ARM: u16 = 40;
pub const EM_AARCH64: u16 = 183;

pub const PT_LOAD: u32 = 1;
pub const PT_DYNAMIC: u32 = 2;
pub const PT_TLS: u32 = 7;
pub const PT_ARM_EXIDX: u32 = 0x7000_0001;

pub const PF_X: u32 = 1;
pub const PF_W: u32 = 2;
pub const PF_R: u32 = 4;

pub const DT_NULL: u64 = 0;
pub const DT_NEEDED: u64 = 1;
pub const DT_PLTRELSZ: u64 = 2;
pub const DT_HASH: u64 = 4;
pub const DT_STRTAB: u64 = 5;
pub const DT_SYMTAB: u64 = 6;
pub const DT_RELA: u64 = 7;
pub const DT_RELASZ: u64 = 8;
pub const DT_STRSZ: u64 = 10;
pub const DT_INIT: u64 = 12;
pub const DT_SONAME: u64 = 14;
pub const DT_SYMBOLIC: u64 = 16;
pub const DT_REL: u64 = 17;
pub const DT_RELSZ: u64 = 18;
pub const DT_PLTREL: u64 = 20;
pub const DT_JMPREL: u64 = 23;
pub const DT_INIT_ARRAY: u64 = 25;
pub const DT_INIT_ARRAYSZ: u64 = 27;
pub const DT_FLAGS: u64 = 30;
pub const DT_PREINIT_ARRAY: u64 = 32;
pub const DT_PREINIT_ARRAYSZ: u64 = 33;
pub const DT_RELRSZ: u64 = 35;
pub const DT_RELR: u64 = 36;
pub const DT_ANDROID_REL: u64 = 0x6000_000f;
pub const DT_ANDROID_RELSZ: u64 = 0x6000_0010;
pub const DT_ANDROID_RELA: u64 = 0x6000_0011;
pub const DT_ANDROID_RELASZ: u64 = 0x6000_0012;
pub const DT_GNU_HASH: u64 = 0x6fff_fef5;
pub const DF_SYMBOLIC: u64 = 2;

#[derive(Debug, Clone, Copy)]
pub struct ProgramHeader {
    pub kind: u32,
    pub flags: u32,
    pub offset: u64,
    pub vaddr: u64,
    pub filesz: u64,
    pub memsz: u64,
}

/// A parsed ELF file header and program headers; the file bytes stay with the caller.
#[derive(Debug, Clone)]
pub struct Elf {
    pub is64: bool,
    pub machine: u16,
    pub phoff: u64,
    pub phentsize: u16,
    pub headers: Vec<ProgramHeader>,
}

fn u16_at(data: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(data.get(at..at + 2)?.try_into().ok()?))
}

fn u32_at(data: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(data.get(at..at + 4)?.try_into().ok()?))
}

fn u64_at(data: &[u8], at: usize) -> Option<u64> {
    Some(u64::from_le_bytes(data.get(at..at + 8)?.try_into().ok()?))
}

impl Elf {
    pub fn parse(data: &[u8]) -> Result<Elf, Error> {
        let bad = |what: &str| Error::Malformed(what.to_string());
        if data.get(..4) != Some(b"\x7fELF") {
            return Err(bad("missing ELF magic"));
        }
        let is64 = match data.get(4) {
            Some(1) => false,
            Some(2) => true,
            _ => return Err(bad("unknown ELF class")),
        };
        if data.get(5) != Some(&1) {
            return Err(bad("not little endian"));
        }
        let machine = u16_at(data, 18).ok_or_else(|| bad("truncated header"))?;
        let (phoff, phentsize, phnum) = if is64 {
            (u64_at(data, 32), u16_at(data, 54), u16_at(data, 56))
        } else {
            (u32_at(data, 28).map(u64::from), u16_at(data, 42), u16_at(data, 44))
        };
        let (phoff, phentsize, phnum) = match (phoff, phentsize, phnum) {
            (Some(a), Some(b), Some(c)) => (a, b, c),
            _ => return Err(bad("truncated header")),
        };
        let mut headers = Vec::with_capacity(phnum as usize);
        for i in 0..phnum as usize {
            let at = phoff as usize + i * phentsize as usize;
            let header = if is64 {
                (|| {
                    Some(ProgramHeader {
                        kind: u32_at(data, at)?,
                        flags: u32_at(data, at + 4)?,
                        offset: u64_at(data, at + 8)?,
                        vaddr: u64_at(data, at + 16)?,
                        filesz: u64_at(data, at + 32)?,
                        memsz: u64_at(data, at + 40)?,
                    })
                })()
            } else {
                (|| {
                    Some(ProgramHeader {
                        kind: u32_at(data, at)?,
                        offset: u32_at(data, at + 4)? as u64,
                        vaddr: u32_at(data, at + 8)? as u64,
                        filesz: u32_at(data, at + 16)? as u64,
                        memsz: u32_at(data, at + 20)? as u64,
                        flags: u32_at(data, at + 24)?,
                    })
                })()
            };
            headers.push(header.ok_or_else(|| bad("truncated program header"))?);
        }
        Ok(Elf { is64, machine, phoff, phentsize, headers })
    }

    pub fn loads(&self) -> impl Iterator<Item = &ProgramHeader> {
        self.headers.iter().filter(|h| h.kind == PT_LOAD)
    }

    pub fn header(&self, kind: u32) -> Option<&ProgramHeader> {
        self.headers.iter().find(|h| h.kind == kind)
    }
}

/// One dynamic symbol table entry.
#[derive(Debug, Clone, Copy)]
pub struct Symbol {
    pub name: u32,
    pub value: u64,
    pub size: u64,
    pub info: u8,
    pub shndx: u16,
}

impl Symbol {
    pub fn is_defined(&self) -> bool {
        self.shndx != 0
    }

    pub fn binding(&self) -> u8 {
        self.info >> 4
    }

    pub fn kind(&self) -> u8 {
        self.info & 0xf
    }

    pub fn is_weak(&self) -> bool {
        self.binding() == 2
    }

    /// Global or weak (local symbols never satisfy other modules).
    pub fn is_exported(&self) -> bool {
        matches!(self.binding(), 1 | 2 | 10)
    }
}

pub fn read_symbol(table: &[u8], is64: bool, index: usize) -> Option<Symbol> {
    if is64 {
        let at = index * 24;
        Some(Symbol {
            name: u32_at(table, at)?,
            info: *table.get(at + 4)?,
            shndx: u16_at(table, at + 6)?,
            value: u64_at(table, at + 8)?,
            size: u64_at(table, at + 16)?,
        })
    } else {
        let at = index * 16;
        Some(Symbol {
            name: u32_at(table, at)?,
            value: u32_at(table, at + 4)? as u64,
            size: u32_at(table, at + 8)? as u64,
            info: *table.get(at + 12)?,
            shndx: u16_at(table, at + 14)?,
        })
    }
}

pub fn sysv_hash(name: &[u8]) -> u32 {
    let mut h: u32 = 0;
    for &c in name {
        h = (h << 4).wrapping_add(c as u32);
        let g = h & 0xf000_0000;
        if g != 0 {
            h ^= g >> 24;
        }
        h &= !g;
    }
    h
}

pub fn gnu_hash(name: &[u8]) -> u32 {
    name.iter().fold(5381u32, |h, &c| h.wrapping_mul(33).wrapping_add(c as u32))
}

/// Symbol lookup tables copied out of a loaded module.
#[derive(Debug, Clone, Default)]
pub struct SymbolTable {
    pub is64: bool,
    pub strtab: Vec<u8>,
    pub symtab: Vec<u8>,
    pub sysv: Option<Vec<u32>>,
    pub gnu: Option<GnuHash>,
}

#[derive(Debug, Clone, Default)]
pub struct GnuHash {
    pub symoffset: u32,
    pub buckets: Vec<u32>,
    pub chains: Vec<u32>,
}

impl SymbolTable {
    pub fn name(&self, offset: u32) -> &[u8] {
        let start = offset as usize;
        let rest = self.strtab.get(start..).unwrap_or(&[]);
        let end = rest.iter().position(|b| *b == 0).unwrap_or(rest.len());
        &rest[..end]
    }

    pub fn symbol(&self, index: usize) -> Option<Symbol> {
        read_symbol(&self.symtab, self.is64, index)
    }

    pub fn len(&self) -> usize {
        self.symtab.len() / if self.is64 { 24 } else { 16 }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Exported, defined symbol by name.
    pub fn lookup(&self, name: &[u8]) -> Option<Symbol> {
        let found = |index: usize| {
            self.symbol(index).filter(|s| s.is_defined() && s.is_exported() && self.name(s.name) == name)
        };
        if let Some(gnu) = &self.gnu {
            if gnu.buckets.is_empty() {
                return None;
            }
            let h = gnu_hash(name);
            let mut index = gnu.buckets[(h as usize) % gnu.buckets.len()];
            if index < gnu.symoffset {
                return None;
            }
            loop {
                let chain = *gnu.chains.get((index - gnu.symoffset) as usize)?;
                if (chain | 1) == (h | 1) {
                    if let Some(s) = found(index as usize) {
                        return Some(s);
                    }
                }
                if chain & 1 != 0 {
                    return None;
                }
                index += 1;
            }
        }
        if let Some(sysv) = &self.sysv {
            let nbucket = *sysv.first()? as usize;
            if nbucket == 0 {
                return None;
            }
            let (buckets, chains) = sysv[2..].split_at(nbucket);
            let mut index = buckets[(sysv_hash(name) as usize) % nbucket] as usize;
            while index != 0 {
                if let Some(s) = found(index) {
                    return Some(s);
                }
                index = *chains.get(index)? as usize;
            }
            return None;
        }
        (0..self.len()).find_map(found)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hashes_match_reference_values() {
        // Values from the System V ABI and the GNU hash documentation.
        assert_eq!(sysv_hash(b""), 0);
        assert_eq!(sysv_hash(b"printf"), 0x077905a6);
        assert_eq!(gnu_hash(b""), 0x00001505);
        assert_eq!(gnu_hash(b"printf"), 0x156b2bb8);
    }
}
