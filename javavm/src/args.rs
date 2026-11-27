//! Reads JNI call arguments from guest registers and memory: named parameters, C varargs
//! (`CallXxxMethod`), `va_list` (`CallXxxMethodV`) and `jvalue` arrays (`CallXxxMethodA`).
//!
//! Calling conventions: AArch64 AAPCS64 as used on Linux and Android (variadic arguments follow
//! the same register rules as named ones), and the 32-bit AAPCS base standard (softfp), which is
//! what armeabi-v7a uses for every call.

use backend::{Arch, Cpu, Reg};

use crate::value::{JType, Value};

/// A source of raw argument slots.
pub struct Args<'a> {
    cpu: &'a dyn Cpu,
    source: Source,
}

enum Source {
    /// Parameters of the current call: next general and FP register, then the stack.
    Call { ngrn: usize, nsrn: usize, stack: u64 },
    /// AArch64 `va_list` (struct with stack pointer, register save areas and offsets).
    VaList64 { stack: u64, gr_top: u64, vr_top: u64, gr_offs: i64, vr_offs: i64 },
    /// 32-bit `va_list`: a pointer walking the saved arguments.
    VaList32 { at: u64 },
    /// `const jvalue*`.
    Jvalues { at: u64 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ArgFault(pub u64);

impl<'a> Args<'a> {
    /// The parameters of the call that just trapped, from the first one.
    pub fn call(cpu: &'a dyn Cpu) -> Args<'a> {
        let stack = cpu.reg(Reg::Sp);
        Args { cpu, source: Source::Call { ngrn: 0, nsrn: 0, stack } }
    }

    /// Skips `count` pointer-sized named parameters (JNIEnv*, jclass, jmethodID...).
    pub fn skip(&mut self, count: usize) -> Result<(), ArgFault> {
        for _ in 0..count {
            self.word()?;
        }
        Ok(())
    }

    pub fn va_list(cpu: &'a dyn Cpu, va: u64) -> Result<Args<'a>, ArgFault> {
        let m = cpu.memory();
        let source = match cpu.arch() {
            Arch::Arm64 => {
                let r = |o: u64| m.read_u64(va + o).map_err(|_| ArgFault(va + o));
                let offs = |o: u64| m.read_u32(va + o).map(|v| v as i32 as i64).map_err(|_| ArgFault(va + o));
                Source::VaList64 { stack: r(0)?, gr_top: r(8)?, vr_top: r(16)?, gr_offs: offs(24)?, vr_offs: offs(28)? }
            }
            Arch::Arm32 => Source::VaList32 { at: va },
        };
        Ok(Args { cpu, source })
    }

    pub fn jvalues(cpu: &'a dyn Cpu, at: u64) -> Args<'a> {
        Args { cpu, source: Source::Jvalues { at } }
    }

    fn read(&self, at: u64, size: usize) -> Result<u64, ArgFault> {
        let m = self.cpu.memory();
        let value = match size {
            4 => m.read_u32(at).map(u64::from),
            _ => m.read_u64(at),
        };
        value.map_err(|_| ArgFault(at))
    }

    fn arm64(&self) -> bool {
        self.cpu.arch() == Arch::Arm64
    }

    /// One pointer-sized integer argument (or `jint`, promoted).
    pub fn word(&mut self) -> Result<u64, ArgFault> {
        self.integer(false)
    }

    /// An integer slot; `wide` is a 64-bit value (jlong) on 32-bit.
    fn integer(&mut self, wide: bool) -> Result<u64, ArgFault> {
        let arm64 = self.arm64();
        let cpu = self.cpu;
        match &mut self.source {
            Source::Call { ngrn, stack, .. } => {
                if arm64 {
                    if *ngrn < 8 {
                        *ngrn += 1;
                        return Ok(cpu.reg(Reg::General(*ngrn as u8 - 1)));
                    }
                    let at = *stack;
                    *stack += 8;
                    return self.read(at, 8);
                }
                if wide {
                    *ngrn = (*ngrn + 1) & !1;
                    if *ngrn <= 2 {
                        let lo = cpu.reg(Reg::General(*ngrn as u8)) & 0xffff_ffff;
                        let hi = cpu.reg(Reg::General(*ngrn as u8 + 1)) & 0xffff_ffff;
                        *ngrn += 2;
                        return Ok(lo | hi << 32);
                    }
                    *ngrn = 4;
                    *stack = (*stack + 7) & !7;
                    let at = *stack;
                    *stack += 8;
                    return self.read(at, 8);
                }
                if *ngrn < 4 {
                    *ngrn += 1;
                    return Ok(cpu.reg(Reg::General(*ngrn as u8 - 1)) & 0xffff_ffff);
                }
                let at = *stack;
                *stack += 4;
                self.read(at, 4)
            }
            Source::VaList64 { stack, gr_top, gr_offs, .. } => {
                if *gr_offs < 0 {
                    let at = gr_top.wrapping_add(*gr_offs as u64);
                    *gr_offs += 8;
                    return self.read(at, 8);
                }
                let at = *stack;
                *stack += 8;
                self.read(at, 8)
            }
            Source::VaList32 { at } => {
                if wide {
                    *at = (*at + 7) & !7;
                }
                let here = *at;
                *at += if wide { 8 } else { 4 };
                self.read(here, if wide { 8 } else { 4 })
            }
            Source::Jvalues { .. } => unreachable!("jvalue arrays are read by type"),
        }
    }

    /// A floating-point slot holding a double (or, for a named AArch64 parameter, a float in the
    /// low 32 bits when `single`).
    fn floating(&mut self, single: bool) -> Result<u64, ArgFault> {
        let arm64 = self.arm64();
        let cpu = self.cpu;
        match &mut self.source {
            Source::Call { nsrn, stack, .. } if arm64 => {
                if *nsrn < 8 {
                    *nsrn += 1;
                    return Ok(cpu.reg(Reg::Fp(*nsrn as u8 - 1)));
                }
                let at = *stack;
                *stack += 8;
                self.read(at, 8)
            }
            Source::VaList64 { stack, vr_top, vr_offs, .. } => {
                if *vr_offs < 0 {
                    let at = vr_top.wrapping_add(*vr_offs as u64);
                    *vr_offs += 16;
                    return self.read(at, 8);
                }
                let at = *stack;
                *stack += 8;
                self.read(at, 8)
            }
            // softfp: floats travel in core registers, doubles as 64-bit integers.
            _ => self.integer(!single),
        }
    }

    /// A named parameter of type `t` (SetFloatField's value, for example).
    pub fn named(&mut self, t: &JType) -> Result<Value, ArgFault> {
        Ok(match t {
            JType::Float => Value::Float(f32::from_bits(self.floating(true)? as u32)),
            JType::Double => Value::Double(f64::from_bits(self.floating(false)?)),
            JType::Long => Value::Long(self.integer(true)? as i64),
            other => primitive(other, self.integer(false)?),
        })
    }

    /// A variadic or `va_list` argument of type `t`: floats arrive promoted to double, and
    /// sub-int types promoted to int.
    pub fn variadic(&mut self, t: &JType) -> Result<Value, ArgFault> {
        if let Source::Jvalues { at } = &mut self.source {
            let here = *at;
            *at += 8;
            let raw = self.read(here, 8)?;
            return Ok(match t {
                JType::Float => Value::Float(f32::from_bits(raw as u32)),
                JType::Double => Value::Double(f64::from_bits(raw)),
                JType::Long => Value::Long(raw as i64),
                JType::Object(_) if !self.arm64() => primitive(t, raw & 0xffff_ffff),
                other => primitive(other, raw),
            });
        }
        Ok(match t {
            JType::Float => Value::Float(f64::from_bits(self.floating(false)?) as f32),
            JType::Double => Value::Double(f64::from_bits(self.floating(false)?)),
            JType::Long => Value::Long(self.integer(true)? as i64),
            other => primitive(other, self.integer(false)?),
        })
    }
}

/// Narrows a raw integer slot to `t`. Object references are returned as `Value::Long` holding
/// the raw reference; the caller decodes them through the reference table.
fn primitive(t: &JType, raw: u64) -> Value {
    match t {
        JType::Boolean => Value::Boolean(raw as u8 != 0),
        JType::Byte => Value::Byte(raw as i8),
        JType::Char => Value::Char(raw as u16),
        JType::Short => Value::Short(raw as i16),
        JType::Int => Value::Int(raw as i32),
        JType::Long => Value::Long(raw as i64),
        JType::Object(_) => Value::Long(raw as i64),
        JType::Float => Value::Float(f32::from_bits(raw as u32)),
        JType::Double => Value::Double(f64::from_bits(raw)),
        JType::Void => Value::Void,
    }
}

/// Writes a return value where the caller expects it.
pub fn set_return(cpu: &mut dyn Cpu, t: &JType, raw: u64, value: Value) {
    let arm64 = cpu.arch() == Arch::Arm64;
    match (t, arm64) {
        (JType::Float, true) => cpu.set_reg(Reg::Fp(0), (value.as_f64() as f32).to_bits() as u64),
        (JType::Double, true) => cpu.set_reg(Reg::Fp(0), value.as_f64().to_bits()),
        (JType::Float, false) => cpu.set_reg(Reg::General(0), (value.as_f64() as f32).to_bits() as u64),
        (JType::Double, false) => {
            let bits = value.as_f64().to_bits();
            cpu.set_reg(Reg::General(0), bits & 0xffff_ffff);
            cpu.set_reg(Reg::General(1), bits >> 32);
        }
        (JType::Long, false) => {
            cpu.set_reg(Reg::General(0), raw & 0xffff_ffff);
            cpu.set_reg(Reg::General(1), raw >> 32);
        }
        (_, true) => cpu.set_reg(Reg::General(0), raw),
        (_, false) => cpu.set_reg(Reg::General(0), raw & 0xffff_ffff),
    }
}

/// Raw integer form of a primitive return value (Java zero- or sign-extends as C would).
pub fn raw_primitive(value: Value) -> u64 {
    match value {
        Value::Boolean(b) => b as u64,
        Value::Byte(v) => v as i64 as u64,
        Value::Char(v) => v as u64,
        Value::Short(v) => v as i64 as u64,
        Value::Int(v) => v as i64 as u64,
        Value::Long(v) => v as u64,
        Value::Float(v) => v.to_bits() as u64,
        Value::Double(v) => v.to_bits(),
        _ => 0,
    }
}

/// One outgoing argument of a call into the guest.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Slot {
    Word(u64),
    Long(u64),
    Float(f32),
    Double(f64),
}

impl Slot {
    /// The slot for a primitive Java value. Object references are passed as `Slot::Word`.
    pub fn from_value(value: Value) -> Slot {
        match value {
            Value::Long(v) => Slot::Long(v as u64),
            Value::Float(v) => Slot::Float(v),
            Value::Double(v) => Slot::Double(v),
            other => Slot::Word(raw_primitive(other)),
        }
    }
}

/// Where outgoing arguments go: general registers from 0, FP registers from 0, and the stack
/// image starting at the stack pointer.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Layout {
    pub general: Vec<u64>,
    pub fp: Vec<u64>,
    pub stack: Vec<u8>,
}

pub fn layout(arch: Arch, slots: &[Slot]) -> Layout {
    let mut out = Layout::default();
    match arch {
        Arch::Arm64 => {
            for slot in slots {
                let (fp, bits) = match *slot {
                    Slot::Word(v) | Slot::Long(v) => (false, v),
                    Slot::Float(v) => (true, v.to_bits() as u64),
                    Slot::Double(v) => (true, v.to_bits()),
                };
                let registers = if fp { &mut out.fp } else { &mut out.general };
                if registers.len() < 8 {
                    registers.push(bits);
                } else {
                    out.stack.extend_from_slice(&bits.to_le_bytes());
                }
            }
        }
        Arch::Arm32 => {
            let mut ngrn = 0usize;
            for slot in slots {
                let (wide, bits) = match *slot {
                    Slot::Word(v) => (false, v & 0xffff_ffff),
                    Slot::Float(v) => (false, v.to_bits() as u64),
                    Slot::Long(v) => (true, v),
                    Slot::Double(v) => (true, v.to_bits()),
                };
                if wide {
                    ngrn = (ngrn + 1) & !1;
                    if ngrn <= 2 {
                        while out.general.len() < ngrn {
                            out.general.push(0);
                        }
                        out.general.push(bits & 0xffff_ffff);
                        out.general.push(bits >> 32);
                        ngrn += 2;
                    } else {
                        ngrn = 4;
                        while out.stack.len() % 8 != 0 {
                            out.stack.push(0);
                        }
                        out.stack.extend_from_slice(&bits.to_le_bytes());
                    }
                } else if ngrn < 4 {
                    while out.general.len() < ngrn {
                        out.general.push(0);
                    }
                    out.general.push(bits);
                    ngrn += 1;
                } else {
                    out.stack.extend_from_slice(&(bits as u32).to_le_bytes());
                }
            }
        }
    }
    out
}

/// A guest function's result registers: x0/r0, r1 (high half on 32-bit), and d0.
#[derive(Debug, Clone, Copy, Default)]
pub struct Returned {
    pub r0: u64,
    pub r1: u64,
    pub fp0: u64,
}

impl Returned {
    pub fn read(cpu: &dyn Cpu) -> Returned {
        Returned { r0: cpu.reg(Reg::General(0)), r1: cpu.reg(Reg::General(1)), fp0: cpu.reg(Reg::Fp(0)) }
    }

    /// The value of type `t` (objects come back as `Value::Long(reference)`).
    pub fn value(&self, arch: Arch, t: &JType) -> Value {
        let arm64 = arch == Arch::Arm64;
        let word = if arm64 { self.r0 } else { self.r0 & 0xffff_ffff };
        let pair = (self.r0 & 0xffff_ffff) | (self.r1 & 0xffff_ffff) << 32;
        match t {
            JType::Float if arm64 => Value::Float(f32::from_bits(self.fp0 as u32)),
            JType::Double if arm64 => Value::Double(f64::from_bits(self.fp0)),
            JType::Float => Value::Float(f32::from_bits(word as u32)),
            JType::Double => Value::Double(f64::from_bits(pair)),
            JType::Long if !arm64 => Value::Long(pair as i64),
            other => primitive(other, word),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arm32_aligns_wide_arguments() {
        // (env, obj, fid, jlong): the long skips r3 and goes to the stack, 8-aligned.
        let l = layout(Arch::Arm32, &[Slot::Word(1), Slot::Word(2), Slot::Word(3), Slot::Long(0x1122_3344_5566_7788)]);
        assert_eq!(l.general, vec![1, 2, 3]);
        assert_eq!(l.stack, 0x1122_3344_5566_7788u64.to_le_bytes().to_vec());
        // (env, jlong, int): the long takes r2:r3, the int goes to the stack.
        let l = layout(Arch::Arm32, &[Slot::Word(1), Slot::Long(5), Slot::Word(9)]);
        assert_eq!(l.general, vec![1, 0, 5, 0]);
        assert_eq!(l.stack, 9u32.to_le_bytes().to_vec());
    }

    #[test]
    fn arm64_separates_fp_registers() {
        let mut slots = vec![Slot::Double(1.5), Slot::Float(2.0)];
        slots.extend((0..9).map(Slot::Word));
        let l = layout(Arch::Arm64, &slots);
        assert_eq!(l.fp, vec![1.5f64.to_bits(), 2.0f32.to_bits() as u64]);
        assert_eq!(l.general.len(), 8);
        assert_eq!(l.stack, 8u64.to_le_bytes().to_vec());
    }
}
