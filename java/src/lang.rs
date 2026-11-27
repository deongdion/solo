//! java.lang: String, StringBuilder, boxed primitives, Object, Class, Throwable, System, and
//! java.nio.charset.Charset.

use javavm::{Call, CallKind, Data, FieldAccess, JResult, JType, ObjectId, Value, Vm};

use crate::{object_arg, throw};

/// Host data of a StringBuilder.
pub(crate) struct Builder(pub Vec<u16>);

/// Host data of a Charset: its canonical name.
pub(crate) struct Charset(pub String);

const BOXES: &[(&str, JType)] = &[
    ("java/lang/Integer", JType::Int),
    ("java/lang/Long", JType::Long),
    ("java/lang/Short", JType::Short),
    ("java/lang/Byte", JType::Byte),
    ("java/lang/Float", JType::Float),
    ("java/lang/Double", JType::Double),
    ("java/lang/Boolean", JType::Boolean),
    ("java/lang/Character", JType::Char),
];

/// Boxes a primitive as `Integer.valueOf` and friends would (a new object each time).
pub fn box_value(vm: &mut Vm, value: Value) -> Option<ObjectId> {
    let class = match value {
        Value::Int(_) => "java/lang/Integer",
        Value::Long(_) => "java/lang/Long",
        Value::Short(_) => "java/lang/Short",
        Value::Byte(_) => "java/lang/Byte",
        Value::Float(_) => "java/lang/Float",
        Value::Double(_) => "java/lang/Double",
        Value::Boolean(_) => "java/lang/Boolean",
        Value::Char(_) => "java/lang/Character",
        Value::Object(id) => return Some(id),
        Value::Null | Value::Void => return None,
    };
    let class = vm.class(class);
    let object = vm.new_object(class);
    vm.heap.get_mut(object).expect("just allocated").fields.insert("value".into(), value);
    Some(object)
}

/// The primitive inside a boxed object.
pub fn unbox(vm: &Vm, id: ObjectId) -> Option<Value> {
    let object = vm.heap.get(id)?;
    let name = vm.class_name(object.class);
    let (_, t) = BOXES.iter().find(|(c, _)| *c == name)?;
    object.fields.get("value").map(|v| v.coerce(t))
}

/// The text of a String object.
pub fn string_value(vm: &Vm, id: ObjectId) -> Option<String> {
    vm.string(id)
}

/// `a.equals(b)` for strings, boxed primitives and identity otherwise.
pub fn java_equals(vm: &Vm, a: Option<ObjectId>, b: Option<ObjectId>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(a), Some(b)) if a == b => true,
        (Some(a), Some(b)) => {
            if let (Some(x), Some(y)) = (vm.string_units(a), vm.string_units(b)) {
                return x == y;
            }
            let same_class = vm.object_class(a) == vm.object_class(b);
            match (same_class, unbox(vm, a), unbox(vm, b)) {
                (true, Some(Value::Float(x)), Some(Value::Float(y))) => x.to_bits() == y.to_bits(),
                (true, Some(Value::Double(x)), Some(Value::Double(y))) => x.to_bits() == y.to_bits(),
                (true, Some(x), Some(y)) => x == y,
                _ => false,
            }
        }
        _ => false,
    }
}

/// `hashCode()` as Java computes it for strings and boxed primitives; a stable identity hash for
/// other objects.
pub fn java_hash(vm: &Vm, id: Option<ObjectId>) -> i32 {
    let Some(id) = id else { return 0 };
    if let Some(units) = vm.string_units(id) {
        return units.iter().fold(0i32, |h, &u| h.wrapping_mul(31).wrapping_add(u as i32));
    }
    match unbox(vm, id) {
        Some(Value::Int(v)) => v,
        Some(Value::Short(v)) => v as i32,
        Some(Value::Byte(v)) => v as i32,
        Some(Value::Char(v)) => v as i32,
        Some(Value::Boolean(v)) => {
            if v {
                1231
            } else {
                1237
            }
        }
        Some(Value::Long(v)) => (v ^ ((v as u64) >> 32) as i64) as i32,
        Some(Value::Float(v)) => float_bits(v) as i32,
        Some(Value::Double(v)) => {
            let bits = double_bits(v);
            (bits ^ (bits >> 32)) as i32
        }
        _ => identity_hash(id),
    }
}

/// `Float.floatToIntBits`: NaN is canonical.
fn float_bits(v: f32) -> u32 {
    if v.is_nan() { 0x7fc0_0000 } else { v.to_bits() }
}

fn double_bits(v: f64) -> u64 {
    if v.is_nan() { 0x7ff8_0000_0000_0000 } else { v.to_bits() }
}

/// A stable, well-mixed identity hash (as `System.identityHashCode`, never 0).
pub(crate) fn identity_hash(id: ObjectId) -> i32 {
    let mut x = (id.0 as u64).wrapping_add(0x9e37_79b9_7f4a_7c15);
    x = (x ^ (x >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    ((x ^ (x >> 31)) as i32 & 0x7fff_ffff) | 1
}

/// `Double.toString`: shortest round-trip digits, Java's layout (`1.0`, `1.0E20`, `1.0E-5`).
pub(crate) fn double_to_string(v: f64, single: bool) -> String {
    if v.is_nan() {
        return "NaN".into();
    }
    if v.is_infinite() {
        return if v > 0.0 { "Infinity".into() } else { "-Infinity".into() };
    }
    if v == 0.0 {
        return if v.is_sign_negative() { "-0.0".into() } else { "0.0".into() };
    }
    // Shortest digits and decimal exponent.
    let sci = if single { format!("{:e}", v as f32) } else { format!("{v:e}") };
    let (mantissa, exponent) = sci.split_once('e').expect("{:e} has an exponent");
    let exponent: i32 = exponent.parse().expect("exponent");
    let negative = mantissa.starts_with('-');
    let digits: String = mantissa.chars().filter(|c| c.is_ascii_digit()).collect();
    let mut out = String::new();
    if negative {
        out.push('-');
    }
    if (-3..7).contains(&exponent) {
        if exponent < 0 {
            out.push_str("0.");
            for _ in 0..(-exponent - 1) {
                out.push('0');
            }
            out.push_str(&digits);
        } else {
            let point = exponent as usize + 1;
            if digits.len() <= point {
                out.push_str(&digits);
                for _ in digits.len()..point {
                    out.push('0');
                }
                out.push_str(".0");
            } else {
                out.push_str(&digits[..point]);
                out.push('.');
                out.push_str(&digits[point..]);
            }
        }
    } else {
        out.push_str(&digits[..1]);
        out.push('.');
        out.push_str(if digits.len() > 1 { &digits[1..] } else { "0" });
        out.push('E');
        out.push_str(&exponent.to_string());
    }
    out
}

/// `String.valueOf(value)` / `toString()` of a primitive.
fn primitive_to_string(value: Value) -> String {
    match value {
        Value::Boolean(b) => b.to_string(),
        Value::Char(c) => String::from_utf16_lossy(&[c]),
        Value::Float(f) => double_to_string(f as f64, true),
        Value::Double(d) => double_to_string(d, false),
        other => other.as_i64().to_string(),
    }
}

/// `String.valueOf(Object)`: "null", the string, a boxed value's text, or `Object.toString`.
fn object_to_string(vm: &Vm, id: Option<ObjectId>) -> String {
    let Some(id) = id else { return "null".into() };
    if let Some(s) = vm.string(id) {
        return s;
    }
    if let Some(v) = unbox(vm, id) {
        return primitive_to_string(v);
    }
    if let Some(Builder(units)) = vm.host_data::<Builder>(id) {
        return String::from_utf16_lossy(units);
    }
    let class = vm.object_class(id).map(|c| vm.class_name(c).replace('/', ".")).unwrap_or_default();
    format!("{class}@{:x}", identity_hash(id))
}

/// Encodes for `getBytes(charset)`; `None` for a charset the model does not know.
fn encode(units: &[u16], charset: &str) -> Option<Vec<u8>> {
    let text = String::from_utf16_lossy(units);
    Some(match charset.to_ascii_lowercase().replace('_', "-").as_str() {
        "utf-8" | "utf8" => text.into_bytes(),
        "iso-8859-1" | "latin1" | "iso8859-1" => units.iter().map(|&u| if u < 256 { u as u8 } else { b'?' }).collect(),
        "us-ascii" | "ascii" => units.iter().map(|&u| if u < 128 { u as u8 } else { b'?' }).collect(),
        "utf-16be" => units.iter().flat_map(|u| u.to_be_bytes()).collect(),
        "utf-16le" => units.iter().flat_map(|u| u.to_le_bytes()).collect(),
        "utf-16" => [0xfe, 0xff].into_iter().chain(units.iter().flat_map(|u| u.to_be_bytes())).collect(),
        _ => return None,
    })
}

fn decode(bytes: &[u8], charset: &str) -> Option<Vec<u16>> {
    Some(match charset.to_ascii_lowercase().replace('_', "-").as_str() {
        "utf-8" | "utf8" => String::from_utf8_lossy(bytes).encode_utf16().collect(),
        "iso-8859-1" | "latin1" | "iso8859-1" => bytes.iter().map(|&b| b as u16).collect(),
        "us-ascii" | "ascii" => bytes.iter().map(|&b| if b < 128 { b as u16 } else { 0xfffd }).collect(),
        "utf-16be" => bytes.chunks_exact(2).map(|c| u16::from_be_bytes([c[0], c[1]])).collect(),
        "utf-16le" => bytes.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect(),
        _ => return None,
    })
}

fn charset_name(vm: &Vm, value: Value) -> Option<String> {
    let id = value.object()?;
    vm.string(id).or_else(|| vm.host_data::<Charset>(id).map(|c| c.0.clone()))
}

fn new_string(vm: &mut Vm, text: &str) -> Value {
    Value::Object(vm.new_string(text))
}

fn units_arg(vm: &mut Vm, call: &Call, index: usize) -> JResult<Vec<u16>> {
    let id = object_arg(vm, call, index)?;
    match vm.string_units(id) {
        Some(u) => Ok(u.to_vec()),
        None => Ok(object_to_string(vm, Some(id)).encode_utf16().collect()),
    }
}

fn find(haystack: &[u16], needle: &[u16], from: usize) -> Option<usize> {
    if needle.is_empty() {
        return Some(from.min(haystack.len()));
    }
    (from..haystack.len().saturating_sub(needle.len() - 1)).find(|&i| haystack[i..].starts_with(needle))
}

fn out_of_bounds(vm: &mut Vm, message: String) -> javavm::Fail {
    throw(vm, "java/lang/StringIndexOutOfBoundsException", &message)
}

fn string_method(vm: &mut Vm, call: &Call, this: ObjectId, key: &str) -> JResult<Option<Value>> {
    let units = vm.string_units(this).expect("checked").to_vec();
    let a = call.args;
    Ok(Some(match key {
        "length()I" => Value::Int(units.len() as i32),
        "isEmpty()Z" => Value::Boolean(units.is_empty()),
        "hashCode()I" => Value::Int(java_hash(vm, Some(this))),
        "toString()Ljava/lang/String;" | "intern()Ljava/lang/String;" => Value::Object(this),
        "equals(Ljava/lang/Object;)Z" => Value::Boolean(java_equals(vm, Some(this), a[0].object())),
        "equalsIgnoreCase(Ljava/lang/String;)Z" => {
            let other = a[0].object().and_then(|o| vm.string(o));
            Value::Boolean(other.is_some_and(|o| o.to_lowercase() == String::from_utf16_lossy(&units).to_lowercase()))
        }
        "charAt(I)C" => match units.get(a[0].as_i32() as usize).filter(|_| a[0].as_i32() >= 0) {
            Some(&c) => Value::Char(c),
            None => return Err(out_of_bounds(vm, format!("index {}, length {}", a[0].as_i32(), units.len()))),
        },
        "getBytes()[B" => Value::Object(vm.new_bytes(&encode(&units, "utf-8").expect("utf-8"))),
        "getBytes(Ljava/lang/String;)[B" | "getBytes(Ljava/nio/charset/Charset;)[B" => {
            let name = charset_name(vm, a[0]).ok_or_else(|| throw(vm, "java/lang/NullPointerException", "charset"))?;
            match encode(&units, &name) {
                Some(bytes) => Value::Object(vm.new_bytes(&bytes)),
                None if key.contains("String") => return Err(throw(vm, "java/io/UnsupportedEncodingException", &name)),
                None => return Ok(None),
            }
        }
        "toCharArray()[C" => Value::Object(vm.new_array(javavm::Array::Char(units), None)),
        "substring(I)Ljava/lang/String;" | "substring(II)Ljava/lang/String;" => {
            let start = a[0].as_i32();
            let end = if a.len() > 1 { a[1].as_i32() } else { units.len() as i32 };
            if start < 0 || end > units.len() as i32 || start > end {
                return Err(out_of_bounds(vm, format!("begin {start}, end {end}, length {}", units.len())));
            }
            Value::Object(vm.new_string_utf16(units[start as usize..end as usize].to_vec()))
        }
        "indexOf(Ljava/lang/String;)I" | "contains(Ljava/lang/CharSequence;)Z" | "startsWith(Ljava/lang/String;)Z" | "endsWith(Ljava/lang/String;)Z" => {
            let needle = units_arg(vm, call, 0)?;
            match call.name {
                "indexOf" => Value::Int(find(&units, &needle, 0).map_or(-1, |i| i as i32)),
                "contains" => Value::Boolean(find(&units, &needle, 0).is_some()),
                "startsWith" => Value::Boolean(units.starts_with(&needle)),
                _ => Value::Boolean(units.ends_with(&needle)),
            }
        }
        "indexOf(I)I" => {
            let c = a[0].as_i32();
            Value::Int(units.iter().position(|&u| u as i32 == c).map_or(-1, |i| i as i32))
        }
        "lastIndexOf(I)I" => {
            let c = a[0].as_i32();
            Value::Int(units.iter().rposition(|&u| u as i32 == c).map_or(-1, |i| i as i32))
        }
        "concat(Ljava/lang/String;)Ljava/lang/String;" => {
            let mut all = units;
            all.extend(units_arg(vm, call, 0)?);
            Value::Object(vm.new_string_utf16(all))
        }
        "toLowerCase()Ljava/lang/String;" => new_string(vm, &String::from_utf16_lossy(&units).to_lowercase()),
        "toUpperCase()Ljava/lang/String;" => new_string(vm, &String::from_utf16_lossy(&units).to_uppercase()),
        "trim()Ljava/lang/String;" => {
            let start = units.iter().position(|&u| u > 0x20).unwrap_or(units.len());
            let end = units.iter().rposition(|&u| u > 0x20).map_or(start, |e| e + 1);
            Value::Object(vm.new_string_utf16(units[start..end].to_vec()))
        }
        "replace(CC)Ljava/lang/String;" => {
            let (from, to) = (a[0].as_i32() as u16, a[1].as_i32() as u16);
            Value::Object(vm.new_string_utf16(units.iter().map(|&u| if u == from { to } else { u }).collect()))
        }
        "compareTo(Ljava/lang/String;)I" | "compareTo(Ljava/lang/Object;)I" => {
            let other = units_arg(vm, call, 0)?;
            let differing = units.iter().zip(&other).find(|(x, y)| x != y);
            Value::Int(match differing {
                Some((x, y)) => *x as i32 - *y as i32,
                None => units.len() as i32 - other.len() as i32,
            })
        }
        _ => return Ok(None),
    }))
}

fn builder_method(vm: &mut Vm, call: &Call, this: ObjectId, key: &str) -> JResult<Option<Value>> {
    let a = call.args;
    let appended: Option<String> = match key {
        "append(Ljava/lang/String;)Ljava/lang/StringBuilder;"
        | "append(Ljava/lang/Object;)Ljava/lang/StringBuilder;"
        | "append(Ljava/lang/CharSequence;)Ljava/lang/StringBuilder;" => Some(object_to_string(vm, a[0].object())),
        k if k.starts_with("append(") && k.ends_with(")Ljava/lang/StringBuilder;") && a.len() == 1 => {
            if let JType::Object(_) = call.sig.params[0] {
                return Ok(None);
            }
            Some(primitive_to_string(a[0]))
        }
        _ => None,
    };
    let builder = vm.host_data_mut::<Builder>(this).expect("checked");
    Ok(Some(match (key, appended) {
        (_, Some(text)) => {
            builder.0.extend(text.encode_utf16());
            Value::Object(this)
        }
        ("toString()Ljava/lang/String;", None) => {
            let units = builder.0.clone();
            Value::Object(vm.new_string_utf16(units))
        }
        ("length()I", None) => Value::Int(builder.0.len() as i32),
        ("setLength(I)V", None) => {
            builder.0.resize(a[0].as_i32().max(0) as usize, 0);
            Value::Void
        }
        _ => return Ok(None),
    }))
}

fn is_throwable(vm: &Vm, id: ObjectId) -> bool {
    let Some(object) = vm.heap.get(id) else { return false };
    object.fields.contains_key("detailMessage")
        || vm.find_class("java/lang/Throwable").is_some_and(|t| vm.is_assignable(object.class, t))
}

/// Constructors and instance methods of java.lang classes.
pub(crate) fn instance(vm: &mut Vm, call: &Call, this: ObjectId, key: &str) -> JResult<Option<Value>> {
    let class = vm.object_class(this).map(|c| vm.class_name(c).to_string()).unwrap_or_default();
    let a = call.args;
    if call.kind == CallKind::Constructor {
        let data = match (class.as_str(), key) {
            ("java/lang/String", "<init>()V") => Data::String(Vec::new()),
            ("java/lang/String", "<init>(Ljava/lang/String;)V") => Data::String(units_arg(vm, call, 0)?),
            ("java/lang/String", "<init>([C)V") => match a[0].object().and_then(|o| vm.array(o)) {
                Some(javavm::Array::Char(c)) => Data::String(c.clone()),
                _ => return Err(throw(vm, "java/lang/NullPointerException", "chars")),
            },
            ("java/lang/String", "<init>([B)V")
            | ("java/lang/String", "<init>([BLjava/lang/String;)V")
            | ("java/lang/String", "<init>([BLjava/nio/charset/Charset;)V")
            | ("java/lang/String", "<init>([BII)V") => {
                let bytes = object_arg(vm, call, 0).map(|b| vm.bytes(b).unwrap_or_default())?;
                let bytes = if key == "<init>([BII)V" {
                    let (offset, length) = (a[1].as_i32(), a[2].as_i32());
                    if offset < 0 || length < 0 || (offset as usize + length as usize) > bytes.len() {
                        return Err(out_of_bounds(vm, format!("offset {offset}, count {length}, length {}", bytes.len())));
                    }
                    bytes[offset as usize..(offset + length) as usize].to_vec()
                } else {
                    bytes
                };
                let charset = if a.len() == 2 { charset_name(vm, a[1]).unwrap_or_default() } else { "utf-8".into() };
                match decode(&bytes, &charset) {
                    Some(units) => Data::String(units),
                    None if key.contains("String;") => return Err(throw(vm, "java/io/UnsupportedEncodingException", &charset)),
                    None => return Ok(None),
                }
            }
            ("java/lang/StringBuilder", "<init>()V") | ("java/lang/StringBuilder", "<init>(I)V") => Data::Host(Box::new(Builder(Vec::new()))),
            ("java/lang/StringBuilder", "<init>(Ljava/lang/String;)V") => Data::Host(Box::new(Builder(units_arg(vm, call, 0)?))),
            _ if BOXES.iter().any(|(c, t)| *c == class && key == format!("<init>({})V", t.descriptor())) => {
                vm.heap.get_mut(this).expect("live").fields.insert("value".into(), a[0]);
                return Ok(Some(Value::Void));
            }
            _ if is_throwable(vm, this) || class.ends_with("Exception") || class.ends_with("Error") => {
                let message = match key {
                    "<init>()V" => None,
                    "<init>(Ljava/lang/String;)V" | "<init>(Ljava/lang/String;Ljava/lang/Throwable;)V" => a[0].object(),
                    _ => return Ok(None),
                };
                let object = vm.heap.get_mut(this).expect("live");
                object.fields.insert("detailMessage".into(), Value::from_object(message));
                if key.contains("Throwable;)") {
                    object.fields.insert("cause".into(), a[1]);
                }
                return Ok(Some(Value::Void));
            }
            _ => return Ok(None),
        };
        vm.heap.get_mut(this).expect("live").data = data;
        return Ok(Some(Value::Void));
    }

    if vm.string_units(this).is_some()
        && let Some(v) = string_method(vm, call, this, key)? {
            return Ok(Some(v));
        }
    if vm.host_data::<Builder>(this).is_some()
        && let Some(v) = builder_method(vm, call, this, key)? {
            return Ok(Some(v));
        }
    if let Some(value) = unbox(vm, this) {
        let result = match key {
            "intValue()I" => Some(Value::Int(value.as_i32())),
            "longValue()J" => Some(Value::Long(value.as_i64())),
            "shortValue()S" => Some(Value::Short(value.as_i64() as i16)),
            "byteValue()B" => Some(Value::Byte(value.as_i64() as i8)),
            "floatValue()F" => Some(Value::Float(value.as_f64() as f32)),
            "doubleValue()D" => Some(Value::Double(value.as_f64())),
            "booleanValue()Z" => Some(Value::Boolean(value.as_bool())),
            "charValue()C" => Some(Value::Char(value.as_i64() as u16)),
            "hashCode()I" => Some(Value::Int(java_hash(vm, Some(this)))),
            "equals(Ljava/lang/Object;)Z" => Some(Value::Boolean(java_equals(vm, Some(this), a[0].object()))),
            "toString()Ljava/lang/String;" => Some(new_string(vm, &primitive_to_string(value))),
            _ => None,
        };
        if result.is_some() {
            return Ok(result);
        }
    }
    if let Some(class_id) = vm.class_of_object(this) {
        let name = vm.class_name(class_id).to_string();
        let result = match key {
            "getName()Ljava/lang/String;" => Some(new_string(vm, &name.replace('/', "."))),
            "getSimpleName()Ljava/lang/String;" => {
                let simple = name.rsplit(['/', '$']).next().unwrap_or(&name).to_string();
                Some(new_string(vm, &simple))
            }
            _ => None,
        };
        if result.is_some() {
            return Ok(result);
        }
    }
    if let Some(Charset(name)) = vm.host_data::<Charset>(this) {
        let name = name.clone();
        if key == "name()Ljava/lang/String;" || key == "toString()Ljava/lang/String;" {
            return Ok(Some(new_string(vm, &name)));
        }
    }
    if is_throwable(vm, this) {
        let message = vm.heap.get(this).and_then(|o| o.fields.get("detailMessage").copied()).unwrap_or(Value::Null);
        match key {
            "getMessage()Ljava/lang/String;" | "getLocalizedMessage()Ljava/lang/String;" => return Ok(Some(message)),
            "getCause()Ljava/lang/Throwable;" => {
                return Ok(Some(vm.heap.get(this).and_then(|o| o.fields.get("cause").copied()).unwrap_or(Value::Null)));
            }
            "toString()Ljava/lang/String;" => {
                let text = vm.describe_throwable(this);
                return Ok(Some(new_string(vm, &text)));
            }
            "printStackTrace()V" => {
                let text = vm.describe_throwable(this);
                vm.warnings.push(format!("printStackTrace: {text}"));
                return Ok(Some(Value::Void));
            }
            _ => {}
        }
    }
    // java.lang.Object, for any receiver.
    Ok(match key {
        "hashCode()I" => Some(Value::Int(java_hash(vm, Some(this)))),
        "equals(Ljava/lang/Object;)Z" => Some(Value::Boolean(java_equals(vm, Some(this), a[0].object()))),
        "getClass()Ljava/lang/Class;" => {
            let class = vm.object_class(this).expect("live");
            Some(Value::Object(vm.class_info(class).object))
        }
        "toString()Ljava/lang/String;" => Some(new_string(vm, &object_to_string(vm, Some(this)))),
        _ => None,
    })
}

fn parse_integer(vm: &mut Vm, text: &str, radix: u32, wide: bool) -> JResult<i64> {
    let parsed = if wide { i64::from_str_radix(text, radix).ok() } else { i32::from_str_radix(text, radix).ok().map(i64::from) };
    // Java rejects a leading '+' only before Java 7; Rust accepts it too.
    parsed.ok_or_else(|| throw(vm, "java/lang/NumberFormatException", &format!("For input string: \"{text}\"")))
}

/// Static methods of java.lang classes.
pub(crate) fn static_call(vm: &mut Vm, call: &Call, key: &str) -> JResult<Option<Value>> {
    let a = call.args;
    let class = call.class;
    let text = |vm: &Vm, i: usize| a.get(i).and_then(|v| v.object()).and_then(|o| vm.string(o));
    Ok(Some(match (class, key) {
        ("java/lang/String", k) if k.starts_with("valueOf(") => match call.sig.params.first() {
            Some(JType::Object(o)) if o == "[C" => match a[0].object().and_then(|id| vm.array(id)) {
                Some(javavm::Array::Char(c)) => {
                    let c = c.clone();
                    Value::Object(vm.new_string_utf16(c))
                }
                _ => return Ok(None),
            },
            Some(JType::Object(_)) => new_string(vm, &object_to_string(vm, a[0].object())),
            Some(_) => new_string(vm, &primitive_to_string(a[0])),
            None => return Ok(None),
        },
        ("java/lang/Integer", "valueOf(I)Ljava/lang/Integer;")
        | ("java/lang/Long", "valueOf(J)Ljava/lang/Long;")
        | ("java/lang/Short", "valueOf(S)Ljava/lang/Short;")
        | ("java/lang/Byte", "valueOf(B)Ljava/lang/Byte;")
        | ("java/lang/Float", "valueOf(F)Ljava/lang/Float;")
        | ("java/lang/Double", "valueOf(D)Ljava/lang/Double;")
        | ("java/lang/Boolean", "valueOf(Z)Ljava/lang/Boolean;")
        | ("java/lang/Character", "valueOf(C)Ljava/lang/Character;") => Value::from_object(box_value(vm, a[0])),
        ("java/lang/Integer", "parseInt(Ljava/lang/String;)I") | ("java/lang/Integer", "parseInt(Ljava/lang/String;I)I") => {
            let s = text(vm, 0).ok_or_else(|| throw(vm, "java/lang/NumberFormatException", "null"))?;
            let radix = if a.len() > 1 { a[1].as_i32() as u32 } else { 10 };
            Value::Int(parse_integer(vm, &s, radix, false)? as i32)
        }
        ("java/lang/Long", "parseLong(Ljava/lang/String;)J") | ("java/lang/Long", "parseLong(Ljava/lang/String;I)J") => {
            let s = text(vm, 0).ok_or_else(|| throw(vm, "java/lang/NumberFormatException", "null"))?;
            let radix = if a.len() > 1 { a[1].as_i32() as u32 } else { 10 };
            Value::Long(parse_integer(vm, &s, radix, true)?)
        }
        ("java/lang/Integer", "toString(I)Ljava/lang/String;") | ("java/lang/Long", "toString(J)Ljava/lang/String;") => {
            new_string(vm, &a[0].as_i64().to_string())
        }
        ("java/lang/Integer", "toHexString(I)Ljava/lang/String;") => new_string(vm, &format!("{:x}", a[0].as_i32() as u32)),
        ("java/lang/Long", "toHexString(J)Ljava/lang/String;") => new_string(vm, &format!("{:x}", a[0].as_i64() as u64)),
        ("java/lang/System", "currentTimeMillis()J") => Value::Long(
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0),
        ),
        ("java/lang/System", "nanoTime()J") => {
            static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
            Value::Long(START.get_or_init(std::time::Instant::now).elapsed().as_nanos() as i64)
        }
        ("java/lang/System", "identityHashCode(Ljava/lang/Object;)I") => Value::Int(a[0].object().map_or(0, identity_hash)),
        ("java/nio/charset/Charset", "forName(Ljava/lang/String;)Ljava/nio/charset/Charset;") => {
            let name = text(vm, 0).ok_or_else(|| throw(vm, "java/lang/IllegalArgumentException", "null charset name"))?;
            if encode(&[], &name).is_none() {
                return Err(throw(vm, "java/nio/charset/UnsupportedCharsetException", &name));
            }
            Value::Object(charset(vm, &name.to_ascii_uppercase()))
        }
        _ => return Ok(None),
    }))
}

fn charset(vm: &mut Vm, name: &str) -> ObjectId {
    vm.new_host_object("java/nio/charset/Charset", Box::new(Charset(name.to_string())))
}

/// `StandardCharsets.UTF_8` and friends, `Boolean.TRUE`/`FALSE`.
pub(crate) fn static_field(vm: &mut Vm, field: &FieldAccess) -> Option<Value> {
    if field.this.is_some() {
        return None;
    }
    let name = match (field.class, field.name) {
        ("java/nio/charset/StandardCharsets", "UTF_8") => "UTF-8",
        ("java/nio/charset/StandardCharsets", "ISO_8859_1") => "ISO-8859-1",
        ("java/nio/charset/StandardCharsets", "US_ASCII") => "US-ASCII",
        ("java/nio/charset/StandardCharsets", "UTF_16BE") => "UTF-16BE",
        ("java/nio/charset/StandardCharsets", "UTF_16LE") => "UTF-16LE",
        ("java/nio/charset/StandardCharsets", "UTF_16") => "UTF-16",
        ("java/lang/Boolean", "TRUE") => return box_value(vm, Value::Boolean(true)).map(Value::Object),
        ("java/lang/Boolean", "FALSE") => return box_value(vm, Value::Boolean(false)).map(Value::Object),
        _ => return None,
    };
    let object = charset(vm, name);
    // Keep the same object for later reads, as a static final would be.
    let class = vm.class(field.class);
    vm.class_info_mut(class).statics.insert(field.name.to_string(), Value::Object(object));
    Some(Value::Object(object))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn doubles_print_like_java() {
        for (v, s) in [
            (1.0, "1.0"),
            (1.5, "1.5"),
            (100.0, "100.0"),
            (1234567.0, "1234567.0"),
            (1e7, "1.0E7"),
            (1.0e20, "1.0E20"),
            (0.001, "0.001"),
            (0.0001, "1.0E-4"),
            (-2.25, "-2.25"),
            (0.1 + 0.2, "0.30000000000000004"),
            (123456.789, "123456.789"),
        ] {
            assert_eq!(double_to_string(v, false), s, "{v}");
        }
        assert_eq!(double_to_string(0.1f32 as f64, true), "0.1");
        assert_eq!(double_to_string(1.5e10f32 as f64, true), "1.5E10");
    }
}
