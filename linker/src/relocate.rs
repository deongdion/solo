//! Relocation processing (REL, RELA, RELR; AArch64 and ARM types).

use std::collections::HashMap;

use backend::Cpu;

use crate::elf::*;
use crate::{Error, Hooks, Linker};

const R_AARCH64_NONE: u32 = 0;
const R_AARCH64_ABS64: u32 = 257;
const R_AARCH64_GLOB_DAT: u32 = 1025;
const R_AARCH64_JUMP_SLOT: u32 = 1026;
const R_AARCH64_RELATIVE: u32 = 1027;

const R_ARM_NONE: u32 = 0;
const R_ARM_ABS32: u32 = 2;
const R_ARM_REL32: u32 = 3;
const R_ARM_GLOB_DAT: u32 = 21;
const R_ARM_JUMP_SLOT: u32 = 22;
const R_ARM_RELATIVE: u32 = 23;

struct Relocation {
    offset: u64,
    kind: u32,
    symbol: u32,
    addend: Option<i64>,
}

impl Linker {
    pub(crate) fn relocate(
        &mut self,
        cpu: &mut dyn Cpu,
        hooks: &mut dyn Hooks,
        index: usize,
        scope: &[usize],
    ) -> Result<(), Error> {
        let module = &self.modules[index];
        let is64 = module.symbols.is64;
        let bias = module.bias;
        let tags = module.tags.clone();
        let get = |t: u64| tags.iter().find(|(tag, _)| *tag == t).map(|(_, v)| *v);
        let mut relocations = Vec::new();
        {
            let memory = cpu.memory();
            if let (Some(at), Some(size)) = (get(DT_RELA), get(DT_RELASZ)) {
                read_table(memory, bias.wrapping_add(at), size, is64, true, &mut relocations)?;
            }
            if let (Some(at), Some(size)) = (get(DT_REL), get(DT_RELSZ)) {
                read_table(memory, bias.wrapping_add(at), size, is64, false, &mut relocations)?;
            }
            if let (Some(at), Some(size)) = (get(DT_JMPREL), get(DT_PLTRELSZ)) {
                let rela = get(DT_PLTREL).map_or(is64, |kind| kind == DT_RELA);
                read_table(memory, bias.wrapping_add(at), size, is64, rela, &mut relocations)?;
            }
        }
        let packed = [(DT_ANDROID_RELA, DT_ANDROID_RELASZ, true), (DT_ANDROID_REL, DT_ANDROID_RELSZ, false)];
        for (tag, size_tag, rela) in packed {
            if let (Some(at), Some(size)) = (get(tag), get(size_tag)) {
                let raw = cpu.memory().read_vec(bias.wrapping_add(at), size as usize)?;
                read_packed(&raw, is64, rela, &mut relocations).map_err(|e| Error::Malformed(format!("{}: {e}", module.name)))?;
            }
        }
        if let (Some(at), Some(size)) = (get(DT_RELR), get(DT_RELRSZ)) {
            apply_relr(cpu, bias, bias.wrapping_add(at), size, is64)?;
        }

        let name = module.name.clone();
        let symbolic = module.symbolic;
        let mut order: Vec<usize> = Vec::with_capacity(scope.len() + 1);
        if symbolic {
            order.push(index);
        }
        order.extend(scope.iter().copied().filter(|i| !symbolic || *i != index));
        if !order.contains(&index) {
            order.push(index);
        }

        let mut resolved: HashMap<u32, u64> = HashMap::new();
        let mut imports: Vec<(u64, String)> = Vec::new();
        for r in relocations {
            let place = bias.wrapping_add(r.offset);
            let symbol_value = if r.symbol == 0 {
                0
            } else if let Some(v) = resolved.get(&r.symbol) {
                *v
            } else {
                let v = self.resolve(cpu, hooks, index, &order, r.symbol, &name);
                resolved.insert(r.symbol, v);
                v
            };
            let addend = |cpu: &dyn Cpu| -> Result<i64, Error> {
                Ok(match r.addend {
                    Some(a) => a,
                    None => cpu.memory().read_u32(place)? as i32 as i64,
                })
            };
            let value = if is64 {
                match r.kind {
                    R_AARCH64_NONE => continue,
                    R_AARCH64_RELATIVE => bias.wrapping_add(addend(cpu)? as u64),
                    R_AARCH64_ABS64 | R_AARCH64_GLOB_DAT | R_AARCH64_JUMP_SLOT => symbol_value.wrapping_add(addend(cpu)? as u64),
                    other => {
                        self.report.warnings.push(format!("{name}: relocation type {other} at {place:#x} not supported"));
                        continue;
                    }
                }
            } else {
                match r.kind {
                    R_ARM_NONE => continue,
                    R_ARM_RELATIVE => bias.wrapping_add(addend(cpu)? as u64) & 0xffff_ffff,
                    R_ARM_ABS32 => symbol_value.wrapping_add(addend(cpu)? as u64) & 0xffff_ffff,
                    R_ARM_REL32 => symbol_value.wrapping_add(addend(cpu)? as u64).wrapping_sub(place) & 0xffff_ffff,
                    R_ARM_GLOB_DAT | R_ARM_JUMP_SLOT => symbol_value & 0xffff_ffff,
                    other => {
                        self.report.warnings.push(format!("{name}: relocation type {other} at {place:#x} not supported"));
                        continue;
                    }
                }
            };
            if r.symbol != 0 && matches!(r.kind, R_AARCH64_ABS64 | R_AARCH64_GLOB_DAT | R_AARCH64_JUMP_SLOT | R_ARM_ABS32 | R_ARM_GLOB_DAT | R_ARM_JUMP_SLOT) {
                let symbols = &self.modules[index].symbols;
                if let Some(s) = symbols.symbol(r.symbol as usize) {
                    imports.push((place, String::from_utf8_lossy(symbols.name(s.name)).into_owned()));
                }
            }
            if is64 {
                cpu.memory_mut().write_u64(place, value)?;
            } else {
                cpu.memory_mut().write_u32(place, value as u32)?;
            }
        }
        self.modules[index].imports = imports;
        Ok(())
    }

    fn resolve(
        &mut self,
        cpu: &mut dyn Cpu,
        hooks: &mut dyn Hooks,
        index: usize,
        order: &[usize],
        symbol: u32,
        module_name: &str,
    ) -> u64 {
        let module = &self.modules[index];
        let Some(sym) = module.symbols.symbol(symbol as usize) else { return 0 };
        let name = module.symbols.name(sym.name).to_vec();
        // Local symbols bind to their own definition.
        if sym.binding() == 0 && sym.is_defined() {
            return module.bias.wrapping_add(sym.value);
        }
        for &candidate in order {
            let m = &self.modules[candidate];
            if let Some(found) = m.symbols.lookup(&name) {
                let library = m.soname.clone().unwrap_or_else(|| m.name.clone());
                let address = m.bias.wrapping_add(found.value);
                let text = String::from_utf8_lossy(&name).into_owned();
                return hooks.intercept(cpu, &library, &text).unwrap_or(address);
            }
        }
        if sym.is_weak() {
            return 0;
        }
        let text = String::from_utf8_lossy(&name).into_owned();
        self.report.unresolved_symbols.push((module_name.to_string(), text.clone()));
        // STT_OBJECT and STT_TLS references are data; everything else may be called.
        let data = matches!(sym.kind(), 1 | 6);
        hooks.unresolved(cpu, module_name, &text, data, sym.size)
    }
}

fn read_table(
    memory: &machine::Memory,
    at: u64,
    size: u64,
    is64: bool,
    rela: bool,
    out: &mut Vec<Relocation>,
) -> Result<(), Error> {
    let entry = match (is64, rela) {
        (true, true) => 24,
        (true, false) => 16,
        (false, true) => 12,
        (false, false) => 8,
    };
    let raw = memory.read_vec(at, size as usize)?;
    for chunk in raw.chunks_exact(entry) {
        let word = |i: usize| -> u64 {
            if is64 {
                u64::from_le_bytes(chunk[i * 8..i * 8 + 8].try_into().unwrap())
            } else {
                u32::from_le_bytes(chunk[i * 4..i * 4 + 4].try_into().unwrap()) as u64
            }
        };
        let (offset, info) = (word(0), word(1));
        let (symbol, kind) = if is64 { ((info >> 32) as u32, info as u32) } else { ((info >> 8) as u32, (info & 0xff) as u32) };
        let addend = rela.then(|| if is64 { word(2) as i64 } else { word(2) as u32 as i32 as i64 });
        out.push(Relocation { offset, kind, symbol, addend });
    }
    Ok(())
}

struct Leb<'a> {
    data: &'a [u8],
    at: usize,
}

impl Leb<'_> {
    fn next(&mut self) -> Result<i64, String> {
        let mut value: i64 = 0;
        let mut shift = 0u32;
        loop {
            let byte = *self.data.get(self.at).ok_or("packed relocations end early")?;
            self.at += 1;
            if shift < 64 {
                value |= ((byte & 0x7f) as i64) << shift;
            }
            shift += 7;
            if byte & 0x80 == 0 {
                if shift < 64 && byte & 0x40 != 0 {
                    value |= -1i64 << shift;
                }
                return Ok(value);
            }
        }
    }
}

/// Android packed relocations ("APS2"): groups of SLEB128-encoded entries sharing info, offset
/// delta or addend.
fn read_packed(raw: &[u8], is64: bool, rela: bool, out: &mut Vec<Relocation>) -> Result<(), String> {
    const BY_INFO: i64 = 1;
    const BY_OFFSET_DELTA: i64 = 2;
    const BY_ADDEND: i64 = 4;
    const HAS_ADDEND: i64 = 8;
    if raw.len() < 4 || &raw[..4] != b"APS2" {
        return Err("packed relocations without APS2 magic".into());
    }
    let mut leb = Leb { data: raw, at: 4 };
    let mut remaining = leb.next()?;
    let mut offset = leb.next()? as u64;
    let mut info = 0u64;
    let mut addend = 0i64;
    if remaining < 0 || remaining as usize > raw.len() * 8 {
        return Err(format!("implausible packed relocation count {remaining}"));
    }
    out.reserve(remaining as usize);
    while remaining > 0 {
        let size = leb.next()?;
        let flags = leb.next()?;
        if size <= 0 || size > remaining {
            return Err(format!("bad packed relocation group size {size}"));
        }
        let delta = if flags & BY_OFFSET_DELTA != 0 { leb.next()? as u64 } else { 0 };
        if flags & BY_INFO != 0 {
            info = leb.next()? as u64;
        }
        if flags & HAS_ADDEND != 0 {
            if !rela {
                return Err("REL packed relocations carry addends".into());
            }
            if flags & BY_ADDEND != 0 {
                addend = addend.wrapping_add(leb.next()?);
            }
        } else {
            addend = 0;
        }
        for _ in 0..size {
            offset = offset.wrapping_add(if flags & BY_OFFSET_DELTA != 0 { delta } else { leb.next()? as u64 });
            if flags & BY_INFO == 0 {
                info = leb.next()? as u64;
            }
            if flags & HAS_ADDEND != 0 && flags & BY_ADDEND == 0 {
                addend = addend.wrapping_add(leb.next()?);
            }
            let (symbol, kind) = if is64 { ((info >> 32) as u32, info as u32) } else { ((info as u32 >> 8), (info & 0xff) as u32) };
            let place = if is64 { offset } else { offset & 0xffff_ffff };
            out.push(Relocation { offset: place, kind, symbol, addend: rela.then_some(addend) });
        }
        remaining -= size;
    }
    Ok(())
}

/// RELR: compressed relative relocations (an address entry, then bitmaps of following words).
fn apply_relr(cpu: &mut dyn Cpu, bias: u64, at: u64, size: u64, is64: bool) -> Result<(), Error> {
    let word = if is64 { 8u64 } else { 4 };
    let bits = word * 8 - 1;
    let raw = cpu.memory().read_vec(at, size as usize)?;
    let mut base = 0u64;
    let patch = |cpu: &mut dyn Cpu, place: u64| -> Result<(), Error> {
        if is64 {
            let v = cpu.memory().read_u64(place)?;
            cpu.memory_mut().write_u64(place, v.wrapping_add(bias))?;
        } else {
            let v = cpu.memory().read_u32(place)?;
            cpu.memory_mut().write_u32(place, v.wrapping_add(bias as u32))?;
        }
        Ok(())
    };
    for chunk in raw.chunks_exact(word as usize) {
        let entry = if is64 { u64::from_le_bytes(chunk.try_into().unwrap()) } else { u32::from_le_bytes(chunk.try_into().unwrap()) as u64 };
        if entry & 1 == 0 {
            let place = bias.wrapping_add(entry);
            patch(cpu, place)?;
            base = place + word;
        } else {
            let mut bitmap = entry >> 1;
            let mut place = base;
            while bitmap != 0 {
                if bitmap & 1 != 0 {
                    patch(cpu, place)?;
                }
                bitmap >>= 1;
                place += word;
            }
            base += bits * word;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packed_group_by_info_and_offset_delta() {
        // count 2, start offset 0x1000; one group of 2 sharing info R_ARM_RELATIVE and delta 4.
        let raw = [b'A', b'P', b'S', b'2', 0x02, 0x80, 0x20, 0x02, 0x03, 0x04, 0x17];
        let mut out = Vec::new();
        read_packed(&raw, false, false, &mut out).unwrap();
        let got: Vec<(u64, u32, u32)> = out.iter().map(|r| (r.offset, r.kind, r.symbol)).collect();
        assert_eq!(got, vec![(0x1004, 23, 0), (0x1008, 23, 0)]);
        assert!(out.iter().all(|r| r.addend.is_none()));
    }

    #[test]
    fn packed_rejects_bad_magic_and_truncation() {
        assert!(read_packed(b"APS1\x00\x00", false, false, &mut Vec::new()).is_err());
        assert!(read_packed(b"APS2\x05\x00\x01", false, false, &mut Vec::new()).is_err());
    }
}
