//! ELF loader: placement, relocation, symbol lookup and init order for Android shared objects.
//!
//! Lookup follows bionic: a library resolves its undefined symbols in the local group of the load
//! that brought it in (breadth-first from the root, the root first), after itself when it is
//! `DT_SYMBOLIC`. Missing libraries and unresolved symbols are recorded instead of failing the load,
//! because Android apps routinely link against libraries an emulator does not provide.

pub mod elf;
mod relocate;

use std::collections::{HashMap, HashSet, VecDeque};

use backend::Cpu;
use elf::*;
use machine::{Perms, page_ceil, page_floor};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("malformed ELF: {0}")]
    Malformed(String),
    #[error("library {0} not found")]
    NotFound(String),
    #[error("{name} is built for machine {machine}, expected {expected}")]
    WrongMachine { name: String, machine: u16, expected: u16 },
    #[error(transparent)]
    Memory(#[from] machine::Error),
    #[error(transparent)]
    Backend(#[from] backend::Error),
}

/// Finds library files by name (`libc.so`) or path.
pub trait Resolver {
    fn find(&self, name: &str) -> Option<(String, Vec<u8>)>;
}

/// Lets the runtime replace symbols and provide stubs for unresolved ones.
pub trait Hooks {
    /// Called for every definition found in `library`; returning an address replaces it.
    fn intercept(&mut self, cpu: &mut dyn Cpu, library: &str, symbol: &str) -> Option<u64>;
    /// Address to bind for a strong reference nothing defines: a stub that reports the call, or
    /// zeroed storage of at least `size` bytes when the reference is to data.
    fn unresolved(&mut self, cpu: &mut dyn Cpu, module: &str, symbol: &str, data: bool, size: u64) -> u64;
}

/// A loaded shared object.
#[derive(Debug, Clone)]
pub struct Module {
    pub name: String,
    pub path: String,
    pub soname: Option<String>,
    pub base: u64,
    pub size: u64,
    /// Load bias: runtime address minus link-time address.
    pub bias: u64,
    pub symbols: SymbolTable,
    pub needed: Vec<String>,
    /// DT_INIT first, then DT_INIT_ARRAY entries, as absolute addresses.
    pub init: Vec<u64>,
    pub symbolic: bool,
    pub phdr: u64,
    pub phnum: u16,
    pub exidx: Option<(u64, u64)>,
    pub dynamic: u64,
    /// Raw dynamic section entries.
    pub tags: Vec<(u64, u64)>,
}

impl Module {
    pub fn contains(&self, address: u64) -> bool {
        address >= self.base && address < self.base + self.size
    }

    fn matches(&self, name: &str) -> bool {
        let file = name.rsplit('/').next().unwrap_or(name);
        self.name == file || self.path == name || self.soname.as_deref() == Some(file)
    }

    /// Exported symbol address.
    pub fn symbol(&self, name: &str) -> Option<u64> {
        self.symbols.lookup(name.as_bytes()).map(|s| self.bias.wrapping_add(s.value))
    }

    /// Nearest exported or local symbol at or below `address`.
    pub fn closest_symbol(&self, address: u64) -> Option<(String, u64)> {
        let mut best: Option<(u64, u32)> = None;
        for i in 0..self.symbols.len() {
            let Some(s) = self.symbols.symbol(i) else { continue };
            if !s.is_defined() || s.value == 0 {
                continue;
            }
            let at = self.bias.wrapping_add(s.value);
            if at <= address && best.is_none_or(|(b, _)| at > b) {
                best = Some((at, s.name));
            }
        }
        best.map(|(at, name)| (String::from_utf8_lossy(self.symbols.name(name)).into_owned(), at))
    }
}

/// Result of a load: the module asked for and the new modules in init order (dependencies first).
#[derive(Debug)]
pub struct Loaded {
    pub module: usize,
    pub init_order: Vec<usize>,
}

/// Everything the linker could not satisfy, for the end-of-run summary.
#[derive(Debug, Default, Clone)]
pub struct Report {
    pub missing_libraries: Vec<(String, String)>,
    pub unresolved_symbols: Vec<(String, String)>,
    pub warnings: Vec<String>,
}

#[derive(Clone)]
pub struct Linker {
    pub modules: Vec<Module>,
    pub report: Report,
    low: u64,
    high: u64,
}

impl Linker {
    /// Modules are placed at the lowest free address in `[low, high)`.
    pub fn new(low: u64, high: u64) -> Linker {
        Linker { modules: Vec::new(), report: Report::default(), low, high }
    }

    pub fn find(&self, name: &str) -> Option<usize> {
        self.modules.iter().position(|m| m.matches(name))
    }

    pub fn module_at(&self, address: u64) -> Option<&Module> {
        self.modules.iter().find(|m| m.contains(address))
    }

    pub fn load(
        &mut self,
        cpu: &mut dyn Cpu,
        resolver: &dyn Resolver,
        hooks: &mut dyn Hooks,
        name: &str,
    ) -> Result<Loaded, Error> {
        if let Some(index) = self.find(name) {
            return Ok(Loaded { module: index, init_order: Vec::new() });
        }
        let first_new = self.modules.len();
        let mut queue = VecDeque::from([(name.to_string(), String::new())]);
        let mut seen = HashSet::new();
        while let Some((wanted, by)) = queue.pop_front() {
            if !seen.insert(wanted.clone()) || self.find(&wanted).is_some() {
                continue;
            }
            let Some((path, data)) = resolver.find(&wanted) else {
                if self.modules.len() == first_new {
                    return Err(Error::NotFound(wanted));
                }
                self.report.missing_libraries.push((by, wanted));
                continue;
            };
            let module = self.map_module(cpu, &wanted, &path, &data)?;
            for needed in &module.needed {
                queue.push_back((needed.clone(), module.name.clone()));
            }
            self.modules.push(module);
        }
        let root = first_new;
        let scope = self.local_group(root);
        for index in first_new..self.modules.len() {
            self.relocate(cpu, hooks, index, &scope)?;
        }
        // init_array holds relocated pointers, so it is read only after relocation.
        for index in first_new..self.modules.len() {
            let init = self.init_array(cpu, index)?;
            self.modules[index].init.extend(init);
        }
        let mut order = Vec::new();
        let mut visited = HashSet::new();
        self.post_order(root, first_new, &mut visited, &mut order);
        Ok(Loaded { module: root, init_order: order })
    }

    fn local_group(&self, root: usize) -> Vec<usize> {
        let mut group = vec![root];
        let mut i = 0;
        while i < group.len() {
            for needed in &self.modules[group[i]].needed {
                if let Some(dep) = self.find(needed)
                    && !group.contains(&dep) {
                        group.push(dep);
                    }
            }
            i += 1;
        }
        group
    }

    fn post_order(&self, index: usize, first_new: usize, visited: &mut HashSet<usize>, out: &mut Vec<usize>) {
        if !visited.insert(index) {
            return;
        }
        for needed in &self.modules[index].needed {
            if let Some(dep) = self.find(needed) {
                self.post_order(dep, first_new, visited, out);
            }
        }
        if index >= first_new {
            out.push(index);
        }
    }

    fn map_module(&mut self, cpu: &mut dyn Cpu, wanted: &str, path: &str, data: &[u8]) -> Result<Module, Error> {
        let elf = Elf::parse(data)?;
        let file = path.rsplit(['/', '\\']).next().unwrap_or(wanted).to_string();
        let expected = match cpu.arch() {
            backend::Arch::Arm64 => EM_AARCH64,
            backend::Arch::Arm32 => EM_ARM,
        };
        if elf.machine != expected {
            return Err(Error::WrongMachine { name: file, machine: elf.machine, expected });
        }
        let low = elf.loads().map(|h| page_floor(h.vaddr)).min().ok_or_else(|| Error::Malformed("no PT_LOAD".into()))?;
        let high = elf.loads().map(|h| page_ceil(h.vaddr + h.memsz)).max().unwrap_or(low);
        let span = high - low;
        let base = cpu.memory().find_free(span, self.low, self.high)?;
        let bias = base.wrapping_sub(low);
        cpu.map(base, span, Perms::RWX, &file)?;

        for h in elf.loads() {
            let start = h.offset as usize;
            let mut size = h.filesz as usize;
            // Packed libraries sometimes declare segments that run past the end of the file.
            if start.saturating_add(size) > data.len() {
                self.report.warnings.push(format!(
                    "{file}: segment at {:#x} reads past the end of the file; truncated",
                    h.vaddr
                ));
                size = data.len().saturating_sub(start);
            }
            if size > 0 {
                cpu.memory_mut().write(bias.wrapping_add(h.vaddr), &data[start..start + size])?;
            }
        }
        // Page permissions follow the segments; pages shared by two segments get both.
        let mut perms: HashMap<u64, u8> = HashMap::new();
        for h in elf.loads() {
            let bits = (if h.flags & PF_R != 0 { 1 } else { 0 })
                | (if h.flags & PF_W != 0 { 2 } else { 0 })
                | (if h.flags & PF_X != 0 { 4 } else { 0 });
            let mut page = page_floor(bias.wrapping_add(h.vaddr));
            while page < page_ceil(bias.wrapping_add(h.vaddr + h.memsz)) {
                *perms.entry(page).or_default() |= bits;
                page += machine::PAGE_SIZE;
            }
        }
        let mut pages: Vec<(u64, u8)> = perms.into_iter().collect();
        pages.sort();
        // One call per run of adjacent pages with the same permissions: unicorn splits a region
        // on every protect and aborts past about a thousand regions on arm32 (1 KiB QEMU pages).
        let mut runs: Vec<(u64, u64, u8)> = Vec::new();
        for (page, bits) in pages {
            match runs.last_mut() {
                Some((start, length, b)) if *b == bits && *start + *length == page => *length += machine::PAGE_SIZE,
                _ => runs.push((page, machine::PAGE_SIZE, bits)),
            }
        }
        for (start, length, bits) in runs {
            cpu.protect(start, length, Perms::from_bits(bits))?;
        }

        let mut module = Module {
            name: file,
            path: path.to_string(),
            soname: None,
            base,
            size: span,
            bias,
            symbols: SymbolTable { is64: elf.is64, ..Default::default() },
            needed: Vec::new(),
            init: Vec::new(),
            symbolic: false,
            phdr: bias.wrapping_add(
                elf.loads().find(|h| h.offset == 0).map(|h| h.vaddr).unwrap_or(low) + elf.phoff,
            ),
            phnum: elf.headers.len() as u16,
            exidx: elf.header(PT_ARM_EXIDX).map(|h| (bias.wrapping_add(h.vaddr), h.memsz)),
            dynamic: 0,
            tags: Vec::new(),
        };
        if let Some(dynamic) = elf.header(PT_DYNAMIC) {
            module.dynamic = bias.wrapping_add(dynamic.vaddr);
            self.read_dynamic(cpu, &mut module, dynamic.memsz)?;
        }
        Ok(module)
    }

    fn read_dynamic(&mut self, cpu: &mut dyn Cpu, module: &mut Module, size: u64) -> Result<(), Error> {
        let memory = cpu.memory();
        let is64 = module.symbols.is64;
        let word = if is64 { 8 } else { 4 };
        let read_word = |at: u64| -> Result<u64, Error> {
            Ok(if is64 { memory.read_u64(at)? } else { memory.read_u32(at)? as u64 })
        };
        let mut tags: Vec<(u64, u64)> = Vec::new();
        let mut at = module.dynamic;
        while at < module.dynamic + size {
            let tag = read_word(at)?;
            let value = read_word(at + word)?;
            if tag == DT_NULL {
                break;
            }
            tags.push((tag, value));
            at += 2 * word;
        }
        let get = |t: u64| tags.iter().find(|(tag, _)| *tag == t).map(|(_, v)| *v);
        let addr = |v: u64| module.bias.wrapping_add(v);

        let strsz = get(DT_STRSZ).unwrap_or(0) as usize;
        if let Some(strtab) = get(DT_STRTAB) {
            module.symbols.strtab = memory.read_vec(addr(strtab), strsz)?;
        }
        let count = if let Some(hash) = get(DT_GNU_HASH) {
            let (gnu, count) = read_gnu_hash(memory, addr(hash), is64)?;
            module.symbols.gnu = Some(gnu);
            count
        } else if let Some(hash) = get(DT_HASH) {
            let nbucket = memory.read_u32(addr(hash))? as usize;
            let nchain = memory.read_u32(addr(hash) + 4)? as usize;
            let words = memory.read_vec(addr(hash), (2 + nbucket + nchain) * 4)?;
            module.symbols.sysv = Some(words.chunks_exact(4).map(|c| u32::from_le_bytes(c.try_into().unwrap())).collect());
            nchain
        } else {
            0
        };
        if let Some(symtab) = get(DT_SYMTAB) {
            module.symbols.symtab = memory.read_vec(addr(symtab), count * if is64 { 24 } else { 16 })?;
        }
        for (tag, value) in &tags {
            if *tag == DT_NEEDED {
                module.needed.push(String::from_utf8_lossy(module.symbols.name(*value as u32)).into_owned());
            }
        }
        module.soname = get(DT_SONAME).map(|v| String::from_utf8_lossy(module.symbols.name(v as u32)).into_owned());
        module.symbolic = get(DT_SYMBOLIC).is_some() || get(DT_FLAGS).is_some_and(|f| f & DF_SYMBOLIC != 0);
        if let Some(init) = get(DT_INIT) {
            module.init.push(addr(init));
        }
        module.symbols.strtab.shrink_to_fit();
        module.tags = tags;
        Ok(())
    }
}

impl Linker {
    fn init_array(&self, cpu: &dyn Cpu, index: usize) -> Result<Vec<u64>, Error> {
        let module = &self.modules[index];
        let get = |t: u64| module.tags.iter().find(|(tag, _)| *tag == t).map(|(_, v)| *v);
        let mut out = Vec::new();
        let (Some(array), Some(bytes)) = (get(DT_INIT_ARRAY), get(DT_INIT_ARRAYSZ)) else { return Ok(out) };
        let is64 = module.symbols.is64;
        let word = if is64 { 8 } else { 4 };
        let at = module.bias.wrapping_add(array);
        for i in 0..bytes / word {
            let entry = if is64 { cpu.memory().read_u64(at + i * word)? } else { cpu.memory().read_u32(at + i * word)? as u64 };
            if entry != 0 && entry != u64::MAX && entry != u32::MAX as u64 {
                out.push(entry);
            }
        }
        Ok(out)
    }
}

fn read_gnu_hash(memory: &machine::Memory, at: u64, is64: bool) -> Result<(GnuHash, usize), Error> {
    let nbuckets = memory.read_u32(at)? as usize;
    let symoffset = memory.read_u32(at + 4)?;
    let bloom_size = memory.read_u32(at + 8)? as u64;
    let buckets_at = at + 16 + bloom_size * if is64 { 8 } else { 4 };
    let raw = memory.read_vec(buckets_at, nbuckets * 4)?;
    let buckets: Vec<u32> = raw.chunks_exact(4).map(|c| u32::from_le_bytes(c.try_into().unwrap())).collect();
    let chains_at = buckets_at + nbuckets as u64 * 4;
    let mut count = symoffset as usize;
    let mut chains = Vec::new();
    if let Some(&last) = buckets.iter().max()
        && last >= symoffset {
            let mut index = last;
            loop {
                let chain = memory.read_u32(chains_at + (index - symoffset) as u64 * 4)?;
                index += 1;
                if chain & 1 != 0 {
                    break;
                }
            }
            count = index as usize;
            let raw = memory.read_vec(chains_at, (count - symoffset as usize) * 4)?;
            chains = raw.chunks_exact(4).map(|c| u32::from_le_bytes(c.try_into().unwrap())).collect();
        }
    Ok((GnuHash { symoffset, buckets, chains }, count))
}
