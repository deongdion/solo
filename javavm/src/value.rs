//! Java values and type descriptors.

use crate::heap::ObjectId;

/// A field or parameter type from a JNI descriptor.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum JType {
    Void,
    Boolean,
    Byte,
    Char,
    Short,
    Int,
    Long,
    Float,
    Double,
    /// Class name in internal form (`java/lang/String`) or an array descriptor (`[B`, `[[I`,
    /// `[Ljava/lang/String;`).
    Object(String),
}

impl JType {
    /// Parses one type at the start of `text`; returns it and the rest.
    pub fn parse(text: &str) -> Option<(JType, &str)> {
        let first = text.chars().next()?;
        let rest = &text[1..];
        Some(match first {
            'V' => (JType::Void, rest),
            'Z' => (JType::Boolean, rest),
            'B' => (JType::Byte, rest),
            'C' => (JType::Char, rest),
            'S' => (JType::Short, rest),
            'I' => (JType::Int, rest),
            'J' => (JType::Long, rest),
            'F' => (JType::Float, rest),
            'D' => (JType::Double, rest),
            'L' => {
                let end = rest.find(';')?;
                (JType::Object(rest[..end].to_string()), &rest[end + 1..])
            }
            '[' => {
                let (element, after) = JType::parse(rest)?;
                if element == JType::Void {
                    return None;
                }
                let consumed = text.len() - after.len();
                (JType::Object(text[..consumed].to_string()), after)
            }
            _ => return None,
        })
    }

    /// Descriptor text: `I`, `Ljava/lang/String;`, `[B`.
    pub fn descriptor(&self) -> String {
        match self {
            JType::Void => "V".into(),
            JType::Boolean => "Z".into(),
            JType::Byte => "B".into(),
            JType::Char => "C".into(),
            JType::Short => "S".into(),
            JType::Int => "I".into(),
            JType::Long => "J".into(),
            JType::Float => "F".into(),
            JType::Double => "D".into(),
            JType::Object(name) if name.starts_with('[') => name.clone(),
            JType::Object(name) => format!("L{name};"),
        }
    }

    pub fn is_wide(&self) -> bool {
        matches!(self, JType::Long | JType::Double)
    }

    pub fn is_floating(&self) -> bool {
        matches!(self, JType::Float | JType::Double)
    }

    /// The zero value of this type (what a JNI call returns when it fails).
    pub fn zero(&self) -> Value {
        match self {
            JType::Void => Value::Void,
            JType::Boolean => Value::Boolean(false),
            JType::Byte => Value::Byte(0),
            JType::Char => Value::Char(0),
            JType::Short => Value::Short(0),
            JType::Int => Value::Int(0),
            JType::Long => Value::Long(0),
            JType::Float => Value::Float(0.0),
            JType::Double => Value::Double(0.0),
            JType::Object(_) => Value::Null,
        }
    }
}

/// A parsed method descriptor `(params)return`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct MethodSig {
    pub params: Vec<JType>,
    pub ret: JType,
}

impl MethodSig {
    pub fn parse(text: &str) -> Option<MethodSig> {
        let mut rest = text.strip_prefix('(')?;
        let mut params = Vec::new();
        while !rest.starts_with(')') {
            let (t, after) = JType::parse(rest)?;
            if t == JType::Void {
                return None;
            }
            params.push(t);
            rest = after;
        }
        let (ret, after) = JType::parse(&rest[1..])?;
        after.is_empty().then_some(MethodSig { params, ret })
    }
}

/// A Java value. Objects are heap ids; `Null` is the null reference.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Value {
    Void,
    Boolean(bool),
    Byte(i8),
    Char(u16),
    Short(i16),
    Int(i32),
    Long(i64),
    Float(f32),
    Double(f64),
    Object(ObjectId),
    Null,
}

impl Value {
    pub fn object(self) -> Option<ObjectId> {
        match self {
            Value::Object(id) => Some(id),
            _ => None,
        }
    }

    pub fn from_object(id: Option<ObjectId>) -> Value {
        id.map_or(Value::Null, Value::Object)
    }

    /// Integral view, widened as Java would (booleans are 0 or 1, chars unsigned).
    pub fn as_i64(self) -> i64 {
        match self {
            Value::Boolean(b) => b as i64,
            Value::Byte(v) => v as i64,
            Value::Char(v) => v as i64,
            Value::Short(v) => v as i64,
            Value::Int(v) => v as i64,
            Value::Long(v) => v,
            Value::Float(v) => v as i64,
            Value::Double(v) => v as i64,
            _ => 0,
        }
    }

    pub fn as_i32(self) -> i32 {
        self.as_i64() as i32
    }

    pub fn as_f64(self) -> f64 {
        match self {
            Value::Float(v) => v as f64,
            Value::Double(v) => v,
            other => other.as_i64() as f64,
        }
    }

    pub fn as_bool(self) -> bool {
        self.as_i64() != 0
    }

    /// Converts to the declared type, as a cast in Java would for primitives. Objects and null
    /// pass through unchanged.
    pub fn coerce(self, to: &JType) -> Value {
        match to {
            JType::Void => Value::Void,
            JType::Boolean => Value::Boolean(self.as_bool()),
            JType::Byte => Value::Byte(self.as_i64() as i8),
            JType::Char => Value::Char(self.as_i64() as u16),
            JType::Short => Value::Short(self.as_i64() as i16),
            JType::Int => Value::Int(self.as_i64() as i32),
            JType::Long => Value::Long(self.as_i64()),
            JType::Float => Value::Float(self.as_f64() as f32),
            JType::Double => Value::Double(self.as_f64()),
            JType::Object(_) => match self {
                Value::Object(_) | Value::Null => self,
                _ => Value::Null,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_method_descriptors() {
        let sig = MethodSig::parse("([B[B[BIIII)V").unwrap();
        assert_eq!(sig.params.len(), 7);
        assert_eq!(sig.params[0], JType::Object("[B".into()));
        assert_eq!(sig.ret, JType::Void);
        let sig = MethodSig::parse("(Ljava/lang/String;J[[Ljava/util/Map;)Ljava/lang/Object;").unwrap();
        assert_eq!(
            sig.params,
            vec![JType::Object("java/lang/String".into()), JType::Long, JType::Object("[[Ljava/util/Map;".into())]
        );
        assert_eq!(sig.ret, JType::Object("java/lang/Object".into()));
        assert!(MethodSig::parse("(V)V").is_none());
        assert!(MethodSig::parse("(I").is_none());
        assert!(MethodSig::parse("(I)Vx").is_none());
        assert_eq!(JType::Object("[I".into()).descriptor(), "[I");
        assert_eq!(JType::Object("a/B".into()).descriptor(), "La/B;");
    }

    #[test]
    fn coerces_like_java() {
        assert_eq!(Value::Int(300).coerce(&JType::Byte), Value::Byte(44));
        assert_eq!(Value::Int(-1).coerce(&JType::Char), Value::Char(0xffff));
        assert_eq!(Value::Double(2.9).coerce(&JType::Int), Value::Int(2));
        assert_eq!(Value::Boolean(true).coerce(&JType::Long), Value::Long(1));
    }
}
