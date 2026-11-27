//! The JNI function table: every `JNINativeInterface` and `JNIInvokeInterface` slot, decoded
//! from guest registers and applied to the VM.
//!
//! A call that reaches a registered native (`CallXxxMethod` on a method bound with
//! RegisterNatives) cannot run here, because running guest code needs the emulator: `dispatch`
//! returns `Action::Native` and the emulator calls the function, then `complete_native`.

use backend::{Arch, Cpu, Reg};

use crate::args::{self, ArgFault, Args, Returned, Slot};
use crate::heap::{Array, Data, ObjectId};
use crate::refs::BadRef;
use crate::value::{JType, Value};
use crate::{CallKind, JNI_VERSION, JniError, JniEvent, Lent, LentKind, Method, Vm, mutf8, table};

const JNI_OK: u64 = 0;
const JNI_ERR: u64 = -1i64 as u64;
const JNI_EVERSION: u64 = -3i64 as u64;
const JNI_COMMIT: u64 = 1;
const JNI_ABORT: u64 = 2;

const METHOD_BASE: u64 = 0x1000_0000;
const FIELD_BASE: u64 = 0x2000_0000;
const ID_SPAN: u64 = 0x0800_0000;

/// What the emulator must do after a JNI slot ran.
#[derive(Debug)]
pub enum Action {
    /// Return registers are set; resume the guest.
    Done,
    /// Call a registered native. A local frame holding the arguments' references is pushed;
    /// pass the result to `complete_native`, which pops it.
    Native { function: u64, slots: Vec<Slot>, ret: JType },
}

fn encode_method(index: usize) -> u64 {
    METHOD_BASE + (index as u64 + 1) * 8
}

fn encode_field(index: usize) -> u64 {
    FIELD_BASE + (index as u64 + 1) * 8
}

fn decode_id(value: u64, base: u64) -> Option<usize> {
    let offset = value.checked_sub(base)?;
    ((8..ID_SPAN).contains(&offset) && offset % 8 == 0).then(|| (offset / 8 - 1) as usize)
}

/// Writes the JNIEnv and JavaVM structures. `native` and `invoke` hold one host stub address
/// per slot (0 for the reserved slots).
pub fn install(vm: &mut Vm, cpu: &mut dyn Cpu, native: &[u64], invoke: &[u64]) -> Result<(), JniError> {
    let word = if cpu.arch() == Arch::Arm64 { 8 } else { 4 };
    let image = |entries: &[u64]| -> Vec<u8> {
        entries.iter().flat_map(|&e| if word == 8 { e.to_le_bytes().to_vec() } else { (e as u32).to_le_bytes().to_vec() }).collect()
    };
    let native_table = vm.arena.alloc_bytes(cpu, &image(native))?;
    let invoke_table = vm.arena.alloc_bytes(cpu, &image(invoke))?;
    vm.env = vm.arena.alloc_bytes(cpu, &image(&[native_table]))?;
    vm.java_vm = vm.arena.alloc_bytes(cpu, &image(&[invoke_table]))?;
    Ok(())
}

fn fault(function: &'static str, f: ArgFault) -> JniError {
    JniError::Misuse { function, problem: format!("argument read failed at {:#x}", f.0) }
}

struct Ctx<'a> {
    vm: &'a mut Vm,
    cpu: &'a mut dyn Cpu,
    name: &'static str,
}

impl Ctx<'_> {
    fn arm64(&self) -> bool {
        self.cpu.arch() == Arch::Arm64
    }

    fn word_size(&self) -> u64 {
        if self.arm64() { 8 } else { 4 }
    }

    fn misuse(&self, problem: impl Into<String>) -> JniError {
        JniError::Misuse { function: self.name, problem: problem.into() }
    }

    /// Reads the named parameters after JNIEnv*: `count` words.
    fn words<const N: usize>(&self) -> Result<[u64; N], JniError> {
        let mut a = Args::call(&*self.cpu);
        a.skip(1).map_err(|f| fault(self.name, f))?;
        let mut out = [0u64; N];
        for o in out.iter_mut() {
            *o = a.word().map_err(|f| fault(self.name, f))?;
        }
        Ok(out)
    }

    fn object(&self, reference: u64) -> Result<Option<ObjectId>, JniError> {
        self.vm.refs.get(reference).map_err(|e| match e {
            BadRef::Invalid(r) => self.misuse(format!("{r:#x} is not a reference")),
            BadRef::Stale(r) => self.misuse(format!("{r:#x} is a deleted or out-of-frame reference")),
        })
    }

    fn non_null(&self, reference: u64, what: &str) -> Result<ObjectId, JniError> {
        self.object(reference)?.ok_or_else(|| self.misuse(format!("{what} is null")))
    }

    fn class(&self, reference: u64) -> Result<crate::ClassId, JniError> {
        let id = self.non_null(reference, "class")?;
        self.vm.class_of_object(id).ok_or_else(|| self.misuse(format!("{reference:#x} is not a class")))
    }

    fn local(&mut self, id: Option<ObjectId>) -> u64 {
        self.vm.refs.new_local(id)
    }

    fn cstr(&self, at: u64) -> Result<Vec<u8>, JniError> {
        if at == 0 {
            return Err(self.misuse("string pointer is null"));
        }
        Ok(self.cpu.memory().read_cstr(at, 1 << 20)?)
    }

    fn cstring(&self, at: u64) -> Result<String, JniError> {
        Ok(String::from_utf16_lossy(&mutf8::decode(&self.cstr(at)?)))
    }

    fn write_word(&mut self, at: u64, value: u64) -> Result<(), JniError> {
        if at == 0 {
            return Ok(());
        }
        if self.arm64() {
            self.cpu.memory_mut().write_u64(at, value)?;
        } else {
            self.cpu.memory_mut().write_u32(at, value as u32)?;
        }
        Ok(())
    }

    fn set_is_copy(&mut self, at: u64) -> Result<(), JniError> {
        if at != 0 {
            self.cpu.memory_mut().write(at, &[1])?;
        }
        Ok(())
    }

    fn ret(&mut self, value: u64) {
        let value = if self.arm64() { value } else { value & 0xffff_ffff };
        self.cpu.set_reg(Reg::General(0), value);
    }

    fn ret_value(&mut self, t: &JType, value: Value) {
        let raw = match t {
            JType::Object(_) => self.vm.refs.new_local(value.object()),
            _ => args::raw_primitive(value.coerce(t)),
        };
        args::set_return(self.cpu, t, raw, value.coerce(t));
    }

    fn throw(&mut self, class: &str, message: &str) {
        let e = self.vm.new_throwable(class, Some(message));
        self.vm.throw(e);
    }

    fn method(&self, id: u64) -> Result<Method, JniError> {
        decode_id(id, METHOD_BASE)
            .and_then(|i| self.vm.method(i))
            .cloned()
            .ok_or_else(|| self.misuse(format!("{id:#x} is not a method ID")))
    }

    fn field(&self, id: u64) -> Result<crate::Field, JniError> {
        decode_id(id, FIELD_BASE)
            .and_then(|i| self.vm.field(i))
            .cloned()
            .ok_or_else(|| self.misuse(format!("{id:#x} is not a field ID")))
    }

    /// Decodes call arguments per the method descriptor: object slots become objects.
    fn convert(&self, raw: Vec<Value>, params: &[JType]) -> Result<Vec<Value>, JniError> {
        raw.into_iter()
            .zip(params)
            .map(|(v, t)| match t {
                JType::Object(_) => Ok(Value::from_object(self.object(v.as_i64() as u64 & self.mask())?)),
                _ => Ok(v),
            })
            .collect()
    }

    fn mask(&self) -> u64 {
        if self.arm64() { u64::MAX } else { 0xffff_ffff }
    }
}

/// Type order of `Call<Type>Method`, `Get<Type>Field` and friends.
fn call_type(index: usize) -> JType {
    [
        JType::Object(String::new()),
        JType::Boolean,
        JType::Byte,
        JType::Char,
        JType::Short,
        JType::Int,
        JType::Long,
        JType::Float,
        JType::Double,
        JType::Void,
    ][index]
    .clone()
}

/// Type order of the primitive array functions.
fn array_type(index: usize) -> JType {
    [JType::Boolean, JType::Byte, JType::Char, JType::Short, JType::Int, JType::Long, JType::Float, JType::Double][index].clone()
}

fn empty_array(t: &JType, length: usize) -> Array {
    match t {
        JType::Boolean => Array::Boolean(vec![0; length]),
        JType::Byte => Array::Byte(vec![0; length]),
        JType::Char => Array::Char(vec![0; length]),
        JType::Short => Array::Short(vec![0; length]),
        JType::Int => Array::Int(vec![0; length]),
        JType::Long => Array::Long(vec![0; length]),
        JType::Float => Array::Float(vec![0.0; length]),
        JType::Double => Array::Double(vec![0.0; length]),
        _ => Array::Object(vec![None; length]),
    }
}

fn array_matches(array: &Array, t: &JType) -> bool {
    matches!(
        (array, t),
        (Array::Boolean(_), JType::Boolean)
            | (Array::Byte(_), JType::Byte)
            | (Array::Char(_), JType::Char)
            | (Array::Short(_), JType::Short)
            | (Array::Int(_), JType::Int)
            | (Array::Long(_), JType::Long)
            | (Array::Float(_), JType::Float)
            | (Array::Double(_), JType::Double)
    )
}

/// Runs one `JNINativeInterface` slot.
pub fn dispatch(vm: &mut Vm, cpu: &mut dyn Cpu, slot: usize) -> Result<Action, JniError> {
    let name = table::NATIVE.get(slot).copied().unwrap_or("unknown");
    vm.caller = cpu.reg(Reg::Lr);
    let mut c = Ctx { vm, cpu, name };
    match slot {
        4 => c.ret(JNI_VERSION as u64),
        5 => {
            let [name, _loader, _buf, _len] = c.words()?;
            let class_name = c.cstring(name)?;
            let id = c.vm.class(&class_name);
            let object = c.vm.class_info(id).object;
            let r = c.local(Some(object));
            c.ret(r);
        }
        6 => {
            let [name] = c.words()?;
            let class_name = c.cstring(name)?.replace('.', "/");
            if c.vm.class_exists(&class_name) {
                let id = c.vm.class(&class_name);
                let object = c.vm.class_info(id).object;
                let r = c.local(Some(object));
                c.ret(r);
            } else {
                c.throw("java/lang/NoClassDefFoundError", &class_name);
                c.ret(0);
            }
        }
        7 | 8 => {
            let [reflected] = c.words()?;
            let id = c.non_null(reflected, "reflected member")?;
            let value = match (slot, c.vm.host_data::<Reflected>(id)) {
                (7, Some(Reflected::Method(i))) => encode_method(*i),
                (8, Some(Reflected::Field(i))) => encode_field(*i),
                _ => return Err(c.misuse("not a reflected member made by ToReflected*")),
            };
            c.ret(value);
        }
        9 | 12 => {
            let [_class, id, _is_static] = c.words()?;
            let (class, data) = if slot == 9 {
                let index = decode_id(id, METHOD_BASE).ok_or_else(|| c.misuse("bad method ID"))?;
                let m = c.vm.method(index).ok_or_else(|| c.misuse("bad method ID"))?;
                let class = if m.name == "<init>" { "java/lang/reflect/Constructor" } else { "java/lang/reflect/Method" };
                (class, Reflected::Method(index))
            } else {
                let index = decode_id(id, FIELD_BASE).ok_or_else(|| c.misuse("bad field ID"))?;
                ("java/lang/reflect/Field", Reflected::Field(index))
            };
            let object = c.vm.new_host_object(class, Box::new(data));
            let r = c.local(Some(object));
            c.ret(r);
        }
        10 => {
            let [class] = c.words()?;
            let id = c.class(class)?;
            let sup = c.vm.class_info(id).superclass.map(|s| c.vm.class_info(s).object);
            let r = c.local(sup);
            c.ret(r);
        }
        11 => {
            let [a, b] = c.words()?;
            let (a, b) = (c.class(a)?, c.class(b)?);
            let yes = c.vm.is_assignable(a, b);
            c.ret(yes as u64);
        }
        13 => {
            let [obj] = c.words()?;
            let id = c.non_null(obj, "throwable")?;
            c.vm.throw(id);
            c.ret(JNI_OK);
        }
        14 => {
            let [class, message] = c.words()?;
            let id = c.class(class)?;
            let class_name = c.vm.class_name(id).to_string();
            let text = if message == 0 { None } else { Some(c.cstring(message)?) };
            let e = c.vm.new_throwable(&class_name, text.as_deref());
            c.vm.throw(e);
            c.ret(JNI_OK);
        }
        15 => {
            let pending = c.vm.pending;
            let r = c.local(pending);
            c.ret(r);
        }
        16 => {
            if let Some(e) = c.vm.pending.take() {
                let text = c.vm.describe_throwable(e);
                c.vm.warnings.push(format!("ExceptionDescribe: {text}"));
            }
        }
        17 => c.vm.pending = None,
        18 => {
            let [message] = c.words()?;
            let text = c.cstring(message).unwrap_or_default();
            let caller = c.vm.caller;
            c.vm.events.push(JniEvent::FatalError { message: text.clone(), caller });
            return Err(JniError::Fatal(text));
        }
        19 => {
            c.vm.refs.push_frame();
            c.ret(JNI_OK);
        }
        20 => {
            let [result] = c.words()?;
            let id = c.object(result)?;
            if c.vm.refs.depth() == 0 {
                return Err(c.misuse("no local frame to pop"));
            }
            c.vm.refs.pop_frame();
            let r = c.local(id);
            c.ret(r);
        }
        21 | 25 | 226 => {
            let [obj] = c.words()?;
            let id = c.object(obj)?;
            let r = match slot {
                21 => c.vm.refs.new_global(id),
                25 => c.vm.refs.new_local(id),
                _ => c.vm.refs.new_weak(id),
            };
            c.ret(r);
        }
        22 | 23 | 227 => {
            let [obj] = c.words()?;
            if !c.vm.refs.delete(obj) {
                c.vm.warnings.push(format!("{name}: {obj:#x} was not a live reference"));
            }
        }
        24 => {
            let [a, b] = c.words()?;
            let same = c.object(a)? == c.object(b)?;
            c.ret(same as u64);
        }
        26 => c.ret(JNI_OK),
        27 => {
            let [class] = c.words()?;
            let id = c.class(class)?;
            let object = c.vm.new_object(id);
            let r = c.local(Some(object));
            c.ret(r);
        }
        28..=30 => return call(&mut c, CallKind::Constructor, slot - 28, JType::Object(String::new())),
        31 => {
            let [obj] = c.words()?;
            let id = c.non_null(obj, "object")?;
            let class = c.vm.object_class(id).expect("live object");
            let object = c.vm.class_info(class).object;
            let r = c.local(Some(object));
            c.ret(r);
        }
        32 => {
            let [obj, class] = c.words()?;
            let target = c.class(class)?;
            let yes = match c.object(obj)? {
                None => true,
                Some(id) => {
                    let runtime = c.vm.object_class(id).expect("live object");
                    c.vm.is_assignable(runtime, target)
                }
            };
            c.ret(yes as u64);
        }
        33 | 113 => {
            let [class, name, desc] = c.words()?;
            let id = c.class(class)?;
            let (name, desc) = (c.cstring(name)?, c.cstring(desc)?);
            match c.vm.method_id(id, &name, &desc, slot == 113) {
                Some(i) => c.ret(encode_method(i)),
                None => {
                    c.throw("java/lang/NoSuchMethodError", &format!("{name}{desc}"));
                    c.ret(0);
                }
            }
        }
        34..=63 => return call(&mut c, CallKind::Virtual, (slot - 34) % 3, call_type((slot - 34) / 3)),
        64..=93 => return call(&mut c, CallKind::Nonvirtual, (slot - 64) % 3, call_type((slot - 64) / 3)),
        114..=143 => return call(&mut c, CallKind::Static, (slot - 114) % 3, call_type((slot - 114) / 3)),
        94 | 144 => {
            let [class, name, desc] = c.words()?;
            let id = c.class(class)?;
            let (name, desc) = (c.cstring(name)?, c.cstring(desc)?);
            match c.vm.field_id(id, &name, &desc, slot == 144) {
                Some(i) => c.ret(encode_field(i)),
                None => {
                    c.throw("java/lang/NoSuchFieldError", &name);
                    c.ret(0);
                }
            }
        }
        95..=103 | 145..=153 => {
            let is_static = slot >= 145;
            let t = call_type(slot - if is_static { 145 } else { 95 });
            let [target, fid] = c.words()?;
            let field = c.field(fid)?;
            let this = if is_static {
                c.class(target)?;
                None
            } else {
                Some(c.non_null(target, "object")?)
            };
            let value = c.vm.read_field(&field, this)?;
            c.ret_value(&t, value);
        }
        104..=112 | 154..=162 => {
            let is_static = slot >= 154;
            let t = call_type(slot - if is_static { 154 } else { 104 });
            let (target, fid, raw) = {
                let mut a = Args::call(&*c.cpu);
                a.skip(1).map_err(|f| fault(name, f))?;
                let target = a.word().map_err(|f| fault(name, f))?;
                let fid = a.word().map_err(|f| fault(name, f))?;
                let raw = a.named(&t).map_err(|f| fault(name, f))?;
                (target, fid, raw)
            };
            let field = c.field(fid)?;
            let value = match t {
                JType::Object(_) => Value::from_object(c.object(raw.as_i64() as u64 & c.mask())?),
                _ => raw,
            };
            let this = if is_static {
                c.class(target)?;
                None
            } else {
                Some(c.non_null(target, "object")?)
            };
            c.vm.write_field(&field, this, value)?;
        }
        163 => {
            let [chars, length] = c.words()?;
            let mut bytes = vec![0u8; (length as usize & 0x7fff_ffff) * 2];
            c.cpu.memory().read(chars, &mut bytes)?;
            let units = bytes.chunks_exact(2).map(|b| u16::from_le_bytes([b[0], b[1]])).collect();
            let s = c.vm.new_string_utf16(units);
            let r = c.local(Some(s));
            c.ret(r);
        }
        164 | 168 => {
            let [s] = c.words()?;
            let id = c.non_null(s, "string")?;
            let units = c.vm.string_units(id).ok_or_else(|| c.misuse("not a string"))?;
            let n = if slot == 164 { units.len() } else { mutf8::encode(units).len() };
            c.ret(n as u64);
        }
        165 | 169 | 224 => {
            let [s, is_copy] = c.words()?;
            let id = c.non_null(s, "string")?;
            let units = c.vm.string_units(id).ok_or_else(|| c.misuse("not a string"))?.to_vec();
            let (bytes, kind) = if slot == 169 {
                let mut b = mutf8::encode(&units);
                b.push(0);
                (b, LentKind::StringUtf)
            } else {
                let mut b: Vec<u8> = units.iter().flat_map(|u| u.to_le_bytes()).collect();
                b.extend_from_slice(&[0, 0]);
                (b, LentKind::StringChars)
            };
            let at = c.vm.arena.alloc_bytes(c.cpu, &bytes)?;
            c.vm.lent.insert(at, Lent { object: id, kind });
            if slot != 224 {
                c.set_is_copy(is_copy)?;
            }
            c.ret(at);
        }
        166 | 170 | 225 => {
            let [_s, chars] = c.words()?;
            let kind = if slot == 170 { LentKind::StringUtf } else { LentKind::StringChars };
            release(&mut c, chars, kind);
        }
        167 => {
            let [text] = c.words()?;
            if text == 0 {
                c.ret(0);
            } else {
                let units = mutf8::decode(&c.cstr(text)?);
                let s = c.vm.new_string_utf16(units);
                let r = c.local(Some(s));
                c.ret(r);
            }
        }
        171 => {
            let [a] = c.words()?;
            let id = c.non_null(a, "array")?;
            let n = c.vm.array(id).ok_or_else(|| c.misuse("not an array"))?.len();
            c.ret(n as u64);
        }
        172 => {
            let [length, class, init] = c.words()?;
            let length = length as i32;
            if length < 0 {
                c.throw("java/lang/NegativeArraySizeException", &length.to_string());
                c.ret(0);
                return Ok(Action::Done);
            }
            let element = c.class(class)?;
            let element_name = c.vm.class_name(element).to_string();
            let fill = c.object(init)?;
            let array = c.vm.new_array(Array::Object(vec![fill; length as usize]), Some(&element_name));
            let r = c.local(Some(array));
            c.ret(r);
        }
        173 | 174 => {
            let [a, index, value] = c.words()?;
            let id = c.non_null(a, "array")?;
            let index = index as i32;
            let value = if slot == 174 { c.object(value)? } else { None };
            let result = match c.vm.array_mut(id) {
                Some(Array::Object(items)) => match usize::try_from(index).ok().filter(|&i| i < items.len()) {
                    Some(i) if slot == 173 => Ok(Some(items[i])),
                    Some(i) => {
                        items[i] = value;
                        Ok(None)
                    }
                    None => Err(items.len()),
                },
                _ => return Err(c.misuse("not an object array")),
            };
            match result {
                Ok(Some(item)) => {
                    let r = c.local(item);
                    c.ret(r);
                }
                Ok(None) => {}
                Err(len) => {
                    c.throw("java/lang/ArrayIndexOutOfBoundsException", &format!("length={len}; index={index}"));
                    c.ret(0);
                }
            }
        }
        175..=182 => {
            let [length] = c.words()?;
            let length = length as i32;
            if length < 0 {
                c.throw("java/lang/NegativeArraySizeException", &length.to_string());
                c.ret(0);
                return Ok(Action::Done);
            }
            let array = c.vm.new_array(empty_array(&array_type(slot - 175), length as usize), None);
            let r = c.local(Some(array));
            c.ret(r);
        }
        183..=190 | 222 => {
            let [a, is_copy] = c.words()?;
            let id = c.non_null(a, "array")?;
            let array = c.vm.array(id).ok_or_else(|| c.misuse("not an array"))?;
            if slot != 222 && !array_matches(array, &array_type(slot - 183)) {
                return Err(c.misuse("array type does not match the function"));
            }
            let bytes = array.bytes(0, array.len()).ok_or_else(|| c.misuse("not a primitive array"))?;
            let at = c.vm.arena.alloc_bytes(c.cpu, &bytes)?;
            c.vm.lent.insert(at, Lent { object: id, kind: LentKind::ArrayElements });
            if slot != 222 {
                c.set_is_copy(is_copy)?;
            }
            c.ret(at);
        }
        191..=198 | 223 => {
            let [a, elements, mode] = c.words()?;
            let id = c.non_null(a, "array")?;
            let mode = mode & 0xffff_ffff;
            if mode != JNI_ABORT
                && let Some(Lent { object, .. }) = c.vm.lent.get(&elements).copied() {
                    if object != id {
                        return Err(c.misuse("elements belong to another array"));
                    }
                    let length = c.vm.array(id).map(|a| a.len() * a.element_size()).unwrap_or(0);
                    let mut bytes = vec![0u8; length];
                    c.cpu.memory().read(elements, &mut bytes)?;
                    c.vm.array_mut(id).expect("checked").set_bytes(0, &bytes);
                }
            if mode != JNI_COMMIT {
                release(&mut c, elements, LentKind::ArrayElements);
            }
        }
        199..=214 => {
            let set = slot >= 207;
            let t = array_type(slot - if set { 207 } else { 199 });
            let [a, start, length, buffer] = c.words()?;
            let id = c.non_null(a, "array")?;
            let (start, length) = (start as i32, length as i32);
            let array = c.vm.array(id).ok_or_else(|| c.misuse("not an array"))?;
            if !array_matches(array, &t) {
                return Err(c.misuse("array type does not match the function"));
            }
            let size = array.element_size();
            let total = array.len();
            let in_bounds = start >= 0 && length >= 0 && (start as usize).checked_add(length as usize).is_some_and(|e| e <= total);
            if !in_bounds {
                c.throw(
                    "java/lang/ArrayIndexOutOfBoundsException",
                    &format!("length={total}; regionStart={start}; regionLength={length}"),
                );
                return Ok(Action::Done);
            }
            let (start, length) = (start as usize, length as usize);
            if set {
                let mut bytes = vec![0u8; length * size];
                c.cpu.memory().read(buffer, &mut bytes)?;
                c.vm.array_mut(id).expect("checked").set_bytes(start, &bytes);
            } else {
                let bytes = array.bytes(start, length).expect("in bounds");
                c.cpu.memory_mut().write(buffer, &bytes)?;
            }
        }
        215 => {
            let [class, methods, count] = c.words()?;
            let id = c.class(class)?;
            let word = c.word_size();
            let mut bound = Vec::new();
            for i in 0..(count & 0xffff_ffff) {
                let entry = methods + i * 3 * word;
                let read = |at: u64| -> Result<u64, JniError> {
                    Ok(if word == 8 { c.cpu.memory().read_u64(at)? } else { c.cpu.memory().read_u32(at)? as u64 })
                };
                let (name_at, desc_at, function) = (read(entry)?, read(entry + word)?, read(entry + 2 * word)?);
                let (name, desc) = (c.cstring(name_at)?, c.cstring(desc_at)?);
                if crate::MethodSig::parse(&desc).is_none() || function == 0 {
                    let class_name = c.vm.class_name(id).to_string();
                    c.throw("java/lang/NoSuchMethodError", &format!("{class_name}.{name}{desc}"));
                    c.ret(JNI_ERR);
                    return Ok(Action::Done);
                }
                bound.push((name, desc, function));
            }
            let class_name = c.vm.class_name(id).to_string();
            for (name, desc, function) in &bound {
                c.vm.class_info_mut(id).natives.insert((name.clone(), desc.clone()), *function);
            }
            let caller = c.vm.caller;
            c.vm.events.push(JniEvent::RegisterNatives { class: class_name, methods: bound, caller });
            c.ret(JNI_OK);
        }
        216 => {
            let [class] = c.words()?;
            let id = c.class(class)?;
            c.vm.class_info_mut(id).natives.clear();
            let class_name = c.vm.class_name(id).to_string();
            let caller = c.vm.caller;
            c.vm.events.push(JniEvent::UnregisterNatives { class: class_name, caller });
            c.ret(JNI_OK);
        }
        217 | 218 => c.ret(JNI_OK),
        219 => {
            let [out] = c.words()?;
            let java_vm = c.vm.java_vm;
            c.write_word(out, java_vm)?;
            c.ret(JNI_OK);
        }
        220 | 221 => {
            let [s, start, length, buffer] = c.words()?;
            let id = c.non_null(s, "string")?;
            let (start, length) = (start as i32, length as i32);
            let units = c.vm.string_units(id).ok_or_else(|| c.misuse("not a string"))?;
            let total = units.len();
            let in_bounds = start >= 0 && length >= 0 && (start as usize).checked_add(length as usize).is_some_and(|e| e <= total);
            if !in_bounds {
                c.throw("java/lang/StringIndexOutOfBoundsException", &format!("length={total}; regionStart={start}; regionLength={length}"));
                return Ok(Action::Done);
            }
            let part = &units[start as usize..start as usize + length as usize];
            let bytes: Vec<u8> = if slot == 220 { part.iter().flat_map(|u| u.to_le_bytes()).collect() } else { mutf8::encode(part) };
            c.cpu.memory_mut().write(buffer, &bytes)?;
        }
        228 => {
            let pending = c.vm.pending.is_some();
            c.ret(pending as u64);
        }
        229 => {
            let [address, capacity] = c.words()?;
            let capacity = if c.arm64() { capacity } else { capacity & 0xffff_ffff };
            let object = c.vm.new_host_object("java/nio/DirectByteBuffer", Box::new(DirectBuffer { address, capacity }));
            let r = c.local(Some(object));
            c.ret(r);
        }
        230 | 231 => {
            let [b] = c.words()?;
            let id = c.object(b)?;
            let buffer = id.and_then(|id| c.vm.host_data::<DirectBuffer>(id)).copied();
            let value = match (slot, buffer) {
                (230, Some(b)) => b.address,
                (231, Some(b)) => b.capacity,
                (230, None) => 0,
                _ => -1i64 as u64,
            };
            c.ret(value);
        }
        232 => {
            let [obj] = c.words()?;
            let kind = match c.vm.refs.get(obj) {
                Ok(Some(_)) => c.vm.refs.kind(obj).map_or(0, |k| k as u64),
                _ => 0,
            };
            c.ret(kind);
        }
        _ => {
            let signature = format!("JNIEnv->{name}");
            c.vm.unhandled.entry(signature.clone()).or_insert(crate::UnhandledJni { count: 0, first_caller: c.vm.caller }).count += 1;
            return Err(JniError::Unhandled { signature });
        }
    }
    Ok(Action::Done)
}

fn release(c: &mut Ctx, at: u64, kind: LentKind) {
    if let Some(lent) = c.vm.lent.remove(&at) {
        if lent.kind != kind {
            c.vm.warnings.push(format!("{}: {at:#x} came from a different Get function ({:?})", c.name, lent.kind));
        }
        c.vm.arena.release(c.cpu, at);
    } else {
        c.vm.warnings.push(format!("{}: {at:#x} was not handed out by this VM", c.name));
    }
}

/// A `CallXxxMethod`, `CallNonvirtualXxxMethod`, `CallStaticXxxMethod` or `NewObject` variant.
/// `variant` is 0 for C varargs, 1 for `va_list`, 2 for a `jvalue` array; `t` is the type the
/// function returns.
fn call(c: &mut Ctx, kind: CallKind, variant: usize, t: JType) -> Result<Action, JniError> {
    let name = c.name;
    let fixed = if kind == CallKind::Nonvirtual { 4 } else { 3 };
    let (words, raw, method) = {
        let mut a = Args::call(&*c.cpu);
        a.skip(1).map_err(|f| fault(name, f))?;
        let mut words = Vec::new();
        for _ in 1..fixed {
            words.push(a.word().map_err(|f| fault(name, f))?);
        }
        let method = c.method(*words.last().expect("method ID"))?;
        let mut source = match variant {
            0 => a,
            1 => {
                let va = if c.cpu.arch() == Arch::Arm64 {
                    a.word().map_err(|f| fault(name, f))?
                } else {
                    a.word().map_err(|f| fault(name, f))? & 0xffff_ffff
                };
                Args::va_list(&*c.cpu, va).map_err(|f| fault(name, f))?
            }
            _ => Args::jvalues(&*c.cpu, a.word().map_err(|f| fault(name, f))?),
        };
        let mut raw = Vec::with_capacity(method.sig.params.len());
        for p in &method.sig.params {
            raw.push(source.variadic(p).map_err(|f| fault(name, f))?);
        }
        (words, raw, method)
    };
    if (kind == CallKind::Static) != method.is_static {
        return Err(c.misuse(format!(
            "{}{} is {}static",
            method.name,
            method.desc,
            if method.is_static { "" } else { "not " }
        )));
    }
    let args = c.convert(raw, &method.sig.params)?;
    let this = match kind {
        CallKind::Static => {
            c.class(words[0])?;
            None
        }
        CallKind::Constructor => {
            let class = c.class(words[0])?;
            Some(c.vm.new_object(class))
        }
        _ => Some(c.non_null(words[0], "receiver")?),
    };
    if let Some(function) = c.vm.native_for(&method, this) {
        c.vm.refs.push_frame();
        let target = match this {
            Some(t) if kind != CallKind::Static => c.vm.refs.new_local(Some(t)),
            _ => c.vm.refs.new_local(Some(c.vm.class_info(method.class).object)),
        };
        let mut slots = vec![Slot::Word(c.vm.env), Slot::Word(target)];
        for (value, p) in args.iter().zip(&method.sig.params) {
            slots.push(match p {
                JType::Object(_) => Slot::Word(c.vm.refs.new_local(value.object())),
                _ => Slot::from_value(value.coerce(p)),
            });
        }
        let ret = if kind == CallKind::Constructor { JType::Void } else { t };
        return Ok(Action::Native { function, slots, ret });
    }
    let result = c.vm.invoke(kind, &method, this, &args)?;
    match kind {
        CallKind::Constructor => {
            let r = c.vm.refs.new_local(this);
            c.ret(r);
        }
        _ if t == JType::Void => {}
        _ => c.ret_value(&t, result),
    }
    Ok(Action::Done)
}

/// Finishes `Action::Native`: decodes the native's result, pops its frame and sets the return
/// registers for the JNI caller.
pub fn complete_native(vm: &mut Vm, cpu: &mut dyn Cpu, ret: &JType, returned: Returned) {
    let value = returned.value(cpu.arch(), ret);
    let value = match ret {
        JType::Object(_) => Value::from_object(vm.refs.get(value.as_i64() as u64).ok().flatten()),
        _ => value,
    };
    vm.refs.pop_frame();
    if *ret != JType::Void {
        let raw = match ret {
            JType::Object(_) => vm.refs.new_local(value.object()),
            _ => args::raw_primitive(value),
        };
        args::set_return(cpu, ret, raw, value);
    }
}

/// Runs one `JNIInvokeInterface` slot.
pub fn dispatch_invoke(vm: &mut Vm, cpu: &mut dyn Cpu, slot: usize) -> Result<(), JniError> {
    let name = table::INVOKE.get(slot).copied().unwrap_or("unknown");
    vm.caller = cpu.reg(Reg::Lr);
    let arm64 = cpu.arch() == Arch::Arm64;
    let arg = |i: u8| {
        let v = cpu.reg(Reg::General(i));
        if arm64 { v } else { v & 0xffff_ffff }
    };
    let (out, version) = (arg(1), arg(2));
    let result = match slot {
        3 => JNI_OK,
        4 | 7 | 6 => {
            if slot == 6 && !matches!(version & 0xffff_ffff, 0x0001_0001 | 0x0001_0002 | 0x0001_0004 | 0x0001_0006) {
                JNI_EVERSION
            } else {
                if out != 0 {
                    if arm64 {
                        cpu.memory_mut().write_u64(out, vm.env)?;
                    } else {
                        cpu.memory_mut().write_u32(out, vm.env as u32)?;
                    }
                }
                JNI_OK
            }
        }
        5 => JNI_OK,
        _ => {
            let signature = format!("JavaVM->{name}");
            vm.unhandled.entry(signature.clone()).or_insert(crate::UnhandledJni { count: 0, first_caller: vm.caller }).count += 1;
            return Err(JniError::Unhandled { signature });
        }
    };
    cpu.set_reg(Reg::General(0), if arm64 { result } else { result & 0xffff_ffff });
    Ok(())
}

/// Host data of `java/lang/reflect` objects made by ToReflectedMethod and ToReflectedField.
#[derive(Debug, Clone, Copy)]
pub enum Reflected {
    Method(usize),
    Field(usize),
}

/// Host data of buffers made by NewDirectByteBuffer.
#[derive(Debug, Clone, Copy)]
pub struct DirectBuffer {
    pub address: u64,
    pub capacity: u64,
}

/// Guest form of a method ID, for callers outside the table (the emulator's call API).
pub fn method_id_value(index: usize) -> u64 {
    encode_method(index)
}

/// Strings of a JNI result for reports.
pub fn describe(vm: &Vm, value: Value) -> String {
    match value {
        Value::Object(id) => match &vm.heap.get(id).map(|o| &o.data) {
            Some(Data::String(_)) => format!("{:?}", vm.string(id).unwrap_or_default()),
            _ => format!("<{}>", vm.object_class(id).map(|c| vm.class_name(c).to_string()).unwrap_or_default()),
        },
        other => format!("{other:?}"),
    }
}
