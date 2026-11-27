//! Dumps of modules that change their own file-backed bytes at run time (packed libraries that
//! decrypt their code or data in an init function, `JNI_OnLoad` or any later call).
//!
//! A tracked module's file-backed segment bytes are copied once its relocation is done and
//! before its init functions run. A dump compares guest memory against that copy and writes the
//! changed bytes into the original file at their file offsets. Nothing runs during execution, so
//! the cost and the result are the same on every backend, and writes from any source count:
//! stores of any width, copies the kernel makes (`read` into a mapping) and host writes.
//! The linker's own writes (relocations) are in the copy, so they never reach the dump.

use std::collections::HashMap;
use std::sync::Arc;

use linker::elf::{Elf, PF_W};

use crate::{Emulator, Error};

/// What a dump writes back.
#[derive(Debug, Clone, Copy, Default)]
pub struct UnpackConfig {
    /// Also writes changed bytes of writable segments (`.data`, `.got`). Off by default: these
    /// change in every library as it runs (globals, constructors' results), and a pointer stored
    /// at run time carries this load's base address.
    pub writable: bool,
}

/// A dumped module.
#[derive(Debug, Clone)]
pub struct Unpacked {
    /// The original file with the changed bytes written in.
    pub file: Vec<u8>,
    /// Changed runs as (file offset, length), in file order.
    pub patched: Vec<(u64, u64)>,
    /// Changed bytes of the ELF header and program header table, left as in the file: a packer
    /// that clears them in memory (an anti-dump step) would otherwise make the dump unloadable.
    pub header_bytes_kept: u64,
}

#[derive(Debug, Clone)]
struct Segment {
    address: u64,
    offset: u64,
    writable: bool,
    /// Memory as the linker left it.
    baseline: Arc<[u8]>,
}

#[derive(Debug, Clone)]
struct Tracked {
    file: Arc<[u8]>,
    /// File ranges never patched: the ELF header and the program header table.
    headers: [(u64, u64); 2],
    segments: Vec<Segment>,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct Unpack {
    /// Library names (file name, path or soname) to track when they load.
    names: Vec<String>,
    /// By module path.
    tracked: HashMap<String, Tracked>,
}

impl Emulator {
    /// Tracks `name` (a file name, path or soname) for `unpack` when it loads, directly or as a
    /// dependency or through `dlopen`. Call before the load: what a module's init functions
    /// change is visible only against the bytes from before they ran.
    pub fn track_unpack(&mut self, name: &str) {
        self.unpack.names.push(name.to_string());
    }

    /// Takes the baselines of the modules just loaded (`new`) that are tracked.
    pub(crate) fn unpack_baselines(&mut self, new: &[usize]) -> Result<(), Error> {
        if self.unpack.names.is_empty() {
            return Ok(());
        }
        for &index in new {
            let module = &self.linker.modules[index];
            if !self.unpack.names.iter().any(|name| module.matches(name)) {
                continue;
            }
            let Some((_, data)) = linker::Resolver::find(&self.resolver, &module.path) else { continue };
            let elf = Elf::parse(&data)?;
            let memory = self.cpu.memory();
            let mut segments = Vec::new();
            for h in elf.loads() {
                // As the linker maps it: a segment past the end of the file is truncated.
                let size = h.filesz.min((data.len() as u64).saturating_sub(h.offset));
                if size == 0 {
                    continue;
                }
                let address = module.bias.wrapping_add(h.vaddr);
                let baseline = memory.read_vec(address, size as usize)?;
                segments.push(Segment { address, offset: h.offset, writable: h.flags & PF_W != 0, baseline: baseline.into() });
            }
            let ehdr = if elf.is64 { 64 } else { 52 };
            let phdrs = (elf.phoff, elf.phoff + elf.headers.len() as u64 * elf.phentsize as u64);
            let tracked = Tracked { file: data.into(), headers: [(0, ehdr), phdrs], segments };
            self.unpack.tracked.insert(module.path.clone(), tracked);
        }
        Ok(())
    }

    /// The module's file with every file-backed byte changed since its load written in. The
    /// module must have been tracked (`track_unpack`) when it loaded.
    pub fn unpack(&self, module: usize, config: UnpackConfig) -> Result<Unpacked, Error> {
        let path = &self.linker.modules[module].path;
        let tracked = self
            .unpack
            .tracked
            .get(path)
            .ok_or_else(|| Error::Unpack(format!("{path} was not tracked when it loaded")))?;
        let mut file = tracked.file.to_vec();
        let mut patched: Vec<(u64, u64)> = Vec::new();
        let mut header_bytes_kept = 0;
        let memory = self.cpu.memory();
        for segment in &tracked.segments {
            if segment.writable && !config.writable {
                continue;
            }
            let now = memory.read_vec(segment.address, segment.baseline.len())?;
            for (i, (&was, &is)) in segment.baseline.iter().zip(&now).enumerate() {
                if was == is {
                    continue;
                }
                let offset = segment.offset + i as u64;
                if tracked.headers.iter().any(|&(start, end)| offset >= start && offset < end) {
                    header_bytes_kept += 1;
                    continue;
                }
                file[offset as usize] = is;
                match patched.last_mut() {
                    Some((start, length)) if *start + *length == offset => *length += 1,
                    _ => patched.push((offset, 1)),
                }
            }
        }
        patched.sort_unstable();
        Ok(Unpacked { file, patched, header_bytes_kept })
    }
}
