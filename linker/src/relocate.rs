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
        if get(DT_ANDROID_REL).is_some() || get(DT_ANDROID_RELA).is_some() {
            self.report.warnings.push(format!("{}: Android packed relocations are not supported yet", module.name));
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
            if is64 {
                cpu.memory_mut().write_u64(place, value)?;
            } else {
                cpu.memory_mut().write_u32(place, value as u32)?;
            }
        }
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
        hooks.unresolved(cpu, module_name, &text)
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
