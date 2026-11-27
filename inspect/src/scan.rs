//! Finds dex, zip and ELF images in mapped guest memory and dumps them (PLAN.md F6).
//!
//! Each hit is validated against its own header and sized from it, so a stray magic inside other
//! data is skipped instead of producing a garbage file. Hits never extend past their mapping.

use std::path::{Path, PathBuf};

use machine::Memory;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Kind {
    Dex,
    Zip,
    Elf,
}

impl Kind {
    pub fn extension(self) -> &'static str {
        match self {
            Kind::Dex => "dex",
            Kind::Zip => "zip",
            Kind::Elf => "so",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hit {
    pub kind: Kind,
    pub address: u64,
    pub size: u64,
    /// Label of the mapping the image lies in (`/proc/self/maps` name or `[anon:...]`).
    pub region: String,
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Memory(#[from] machine::Error),
    #[error("writing {path}: {source}")]
    Io { path: PathBuf, source: std::io::Error },
}

fn u16_at(b: &[u8], at: usize) -> Option<u64> {
    b.get(at..at + 2).map(|s| u16::from_le_bytes(s.try_into().unwrap()) as u64)
}

fn u32_at(b: &[u8], at: usize) -> Option<u64> {
    b.get(at..at + 4).map(|s| u32::from_le_bytes(s.try_into().unwrap()) as u64)
}

fn u64_at(b: &[u8], at: usize) -> Option<u64> {
    b.get(at..at + 8).map(|s| u64::from_le_bytes(s.try_into().unwrap()))
}

/// `dex\n035\0` style header with a plausible file size.
fn dex_size(b: &[u8]) -> Option<u64> {
    if b.len() < 0x70 || !b[4..7].iter().all(u8::is_ascii_digit) || b[7] != 0 {
        return None;
    }
    let size = u32_at(b, 0x20)?;
    let header = u32_at(b, 0x24)?;
    (header == 0x70 && size >= 0x70 && size <= b.len() as u64).then_some(size)
}

/// Extent of an ELF image: the furthest of its section header table and its segments' file data.
fn elf_size(b: &[u8]) -> Option<u64> {
    let is64 = match b.get(4)? {
        1 => false,
        2 => true,
        _ => return None,
    };
    if *b.get(5)? != 1 || *b.get(6)? != 1 {
        return None;
    }
    let (phoff, shoff, phentsize, phnum, shentsize, shnum) = if is64 {
        (u64_at(b, 0x20)?, u64_at(b, 0x28)?, u16_at(b, 0x36)?, u16_at(b, 0x38)?, u16_at(b, 0x3a)?, u16_at(b, 0x3c)?)
    } else {
        (u32_at(b, 0x1c)?, u32_at(b, 0x20)?, u16_at(b, 0x2a)?, u16_at(b, 0x2c)?, u16_at(b, 0x2e)?, u16_at(b, 0x30)?)
    };
    let expected = if is64 { 56 } else { 32 };
    if phnum == 0 || phentsize != expected {
        return None;
    }
    let mut end = phoff.checked_add(phnum * phentsize)?;
    for i in 0..phnum {
        let at = (phoff + i * phentsize) as usize;
        let (offset, filesz) = if is64 { (u64_at(b, at + 8)?, u64_at(b, at + 32)?) } else { (u32_at(b, at + 4)?, u32_at(b, at + 16)?) };
        end = end.max(offset.checked_add(filesz)?);
    }
    if shnum != 0 && shoff != 0 {
        end = end.max(shoff.checked_add(shnum * shentsize)?);
    }
    // A loaded image is laid out by address, not by file offset; the extent is still bounded by
    // the mapping, which `scan` enforces.
    Some(end.min(b.len() as u64))
}

/// From a local file header to the end of the end-of-central-directory record that points back at
/// a central directory inside the same range.
fn zip_size(b: &[u8]) -> Option<u64> {
    const EOCD: &[u8] = b"PK\x05\x06";
    let mut from = 30;
    while let Some(found) = find(&b[from..], EOCD) {
        let at = from + found;
        let cd_size = u32_at(b, at + 12)?;
        let cd_offset = u32_at(b, at + 16)?;
        let comment = u16_at(b, at + 20)?;
        if cd_offset + cd_size == at as u64 {
            let end = at as u64 + 22 + comment;
            return (end <= b.len() as u64).then_some(end);
        }
        from = at + 4;
    }
    None
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    memchr::memmem::find(haystack, needle)
}

/// Scans every readable mapping for dex, zip and ELF images.
pub fn scan(memory: &Memory) -> Result<Vec<Hit>, Error> {
    let magics: [(&[u8], Kind); 3] = [(b"dex\n", Kind::Dex), (b"PK\x03\x04", Kind::Zip), (b"\x7fELF", Kind::Elf)];
    let mut hits = Vec::new();
    let regions: Vec<(u64, u64, String)> = memory
        .mappings()
        .filter(|m| m.perms.contains(machine::Perms::READ))
        .map(|m| (m.start, m.end, m.label.clone()))
        .collect();
    for (start, end, label) in regions {
        let bytes = memory.read_vec(start, (end - start) as usize)?;
        for (magic, kind) in magics {
            let finder = memchr::memmem::Finder::new(magic);
            let mut from = 0;
            while let Some(found) = finder.find(&bytes[from..]) {
                let at = from + found;
                let rest = &bytes[at..];
                let size = match kind {
                    Kind::Dex => dex_size(rest),
                    Kind::Zip => zip_size(rest),
                    Kind::Elf => elf_size(rest),
                };
                match size {
                    Some(size) => {
                        hits.push(Hit { kind, address: start + at as u64, size, region: label.clone() });
                        // Zip entries contain further local headers; skip past the whole archive.
                        from = at + if kind == Kind::Zip { size as usize } else { magic.len() };
                    }
                    None => from = at + 1,
                }
            }
        }
    }
    hits.sort_by_key(|h| h.address);
    Ok(hits)
}

/// Writes each hit to `directory` as `<kind>_<address>.<ext>` and returns the paths.
pub fn dump(memory: &Memory, hits: &[Hit], directory: &Path) -> Result<Vec<PathBuf>, Error> {
    std::fs::create_dir_all(directory).map_err(|source| Error::Io { path: directory.to_path_buf(), source })?;
    let mut paths = Vec::new();
    for hit in hits {
        let data = memory.read_vec(hit.address, hit.size as usize)?;
        let path = directory.join(format!("{:?}_{:x}.{}", hit.kind, hit.address, hit.kind.extension()).to_lowercase());
        std::fs::write(&path, &data).map_err(|source| Error::Io { path: path.clone(), source })?;
        paths.push(path);
    }
    Ok(paths)
}

#[cfg(test)]
mod tests {
    use super::*;
    use machine::Perms;

    fn dex(size: u32) -> Vec<u8> {
        let mut d = vec![0u8; size as usize];
        d[..8].copy_from_slice(b"dex\n035\0");
        d[0x20..0x24].copy_from_slice(&size.to_le_bytes());
        d[0x24..0x28].copy_from_slice(&0x70u32.to_le_bytes());
        d
    }

    fn zip() -> Vec<u8> {
        let mut z = b"PK\x03\x04".to_vec();
        z.extend_from_slice(&[0u8; 26]);
        z.extend_from_slice(b"PK\x05\x06");
        z.extend_from_slice(&[0u8; 8]);
        z.extend_from_slice(&0u32.to_le_bytes()); // central directory size
        z.extend_from_slice(&30u32.to_le_bytes()); // central directory offset
        z.extend_from_slice(&0u16.to_le_bytes());
        z
    }

    #[test]
    fn finds_and_sizes_each_kind() {
        let mut memory = Memory::new(32);
        memory.map(0x1000_0000, 0x4000, Perms::RW, "[anon:decrypted]").unwrap();
        memory.write(0x1000_0100, &dex(0x200)).unwrap();
        memory.write(0x1000_1000, &zip()).unwrap();
        // A magic with an invalid header is ignored.
        memory.write(0x1000_2000, b"dex\nabc\0").unwrap();
        let hits = scan(&memory).unwrap();
        let found: Vec<(Kind, u64, u64)> = hits.iter().map(|h| (h.kind, h.address, h.size)).collect();
        assert_eq!(found, vec![(Kind::Dex, 0x1000_0100, 0x200), (Kind::Zip, 0x1000_1000, 52)]);
        assert!(hits.iter().all(|h| h.region == "[anon:decrypted]"));

        let dir = std::env::temp_dir().join(format!("scan_test_{}", std::process::id())).join("한글 경로");
        let paths = dump(&memory, &hits, &dir).unwrap();
        assert_eq!(std::fs::read(&paths[0]).unwrap(), dex(0x200));
        assert_eq!(std::fs::read(&paths[1]).unwrap(), zip());
        std::fs::remove_dir_all(dir.parent().unwrap()).unwrap();
    }

    #[test]
    fn dex_running_past_its_mapping_is_rejected() {
        let mut memory = Memory::new(32);
        memory.map(0x2000_0000, 0x1000, Perms::RW, "short").unwrap();
        memory.write(0x2000_0f00, &dex(0x100)[..0x100]).unwrap();
        let mut long = dex(0x100);
        long[0x20..0x24].copy_from_slice(&0x1001u32.to_le_bytes());
        memory.write(0x2000_0000, &long).unwrap();
        let hits = scan(&memory).unwrap();
        assert_eq!(hits.iter().map(|h| h.address).collect::<Vec<_>>(), vec![0x2000_0f00]);
    }
}
