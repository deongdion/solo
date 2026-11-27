//! Built-in java.lang, java.util and java.nio models, so native code that uses everyday Java
//! classes runs without handler code: strings, boxed primitives, StringBuilder, exceptions, and
//! the collection protocol (`Map`, `Set`, `List`, `Iterator`, `Map.Entry`, PLAN.md F11).
//!
//! Methods are matched by name and descriptor on the receiver's model, not on the class named in
//! GetMethodID, so `Map.get`, `HashMap.get` and `AbstractMap.get` all reach the same code, and a
//! private class such as `HashMap$KeySet` needs no reflection.
//!
//! The models answer only what they can answer as Java would; anything else stays unhandled and
//! reaches the embedder's policy with its signature.

mod lang;
mod util;

use javavm::{Call, Fail, JResult, Jni, ObjectId, Value, Vm};

pub use lang::{box_value, java_equals, java_hash, string_value, unbox};
pub use util::{MapKind, list_items, map_entries, new_list, new_map, new_set};

/// Declares the standard class hierarchy and adds the models to `vm` (after the embedder's
/// handler).
pub fn install(vm: &mut Vm) {
    for (class, superclass, interfaces) in HIERARCHY {
        vm.define_class(class, superclass, interfaces);
    }
    vm.add_model(Box::new(Builtins));
}

const HIERARCHY: &[(&str, &str, &[&str])] = &[
    ("java/lang/String", "java/lang/Object", &["java/lang/CharSequence", "java/lang/Comparable", "java/io/Serializable"]),
    ("java/lang/StringBuilder", "java/lang/Object", &["java/lang/CharSequence", "java/lang/Appendable"]),
    ("java/lang/Number", "java/lang/Object", &["java/io/Serializable"]),
    ("java/lang/Integer", "java/lang/Number", &["java/lang/Comparable"]),
    ("java/lang/Long", "java/lang/Number", &["java/lang/Comparable"]),
    ("java/lang/Short", "java/lang/Number", &["java/lang/Comparable"]),
    ("java/lang/Byte", "java/lang/Number", &["java/lang/Comparable"]),
    ("java/lang/Float", "java/lang/Number", &["java/lang/Comparable"]),
    ("java/lang/Double", "java/lang/Number", &["java/lang/Comparable"]),
    ("java/lang/Boolean", "java/lang/Object", &["java/lang/Comparable", "java/io/Serializable"]),
    ("java/lang/Character", "java/lang/Object", &["java/lang/Comparable", "java/io/Serializable"]),
    ("java/lang/Throwable", "java/lang/Object", &["java/io/Serializable"]),
    ("java/lang/Exception", "java/lang/Throwable", &[]),
    ("java/lang/Error", "java/lang/Throwable", &[]),
    ("java/lang/RuntimeException", "java/lang/Exception", &[]),
    ("java/lang/IllegalStateException", "java/lang/RuntimeException", &[]),
    ("java/lang/IllegalArgumentException", "java/lang/RuntimeException", &[]),
    ("java/lang/NumberFormatException", "java/lang/IllegalArgumentException", &[]),
    ("java/lang/NullPointerException", "java/lang/RuntimeException", &[]),
    ("java/lang/UnsupportedOperationException", "java/lang/RuntimeException", &[]),
    ("java/lang/ClassCastException", "java/lang/RuntimeException", &[]),
    ("java/lang/IndexOutOfBoundsException", "java/lang/RuntimeException", &[]),
    ("java/lang/ArrayIndexOutOfBoundsException", "java/lang/IndexOutOfBoundsException", &[]),
    ("java/lang/StringIndexOutOfBoundsException", "java/lang/IndexOutOfBoundsException", &[]),
    ("java/lang/NegativeArraySizeException", "java/lang/RuntimeException", &[]),
    ("java/util/NoSuchElementException", "java/lang/RuntimeException", &[]),
    ("java/lang/LinkageError", "java/lang/Error", &[]),
    ("java/lang/NoClassDefFoundError", "java/lang/LinkageError", &[]),
    ("java/lang/IncompatibleClassChangeError", "java/lang/LinkageError", &[]),
    ("java/lang/NoSuchMethodError", "java/lang/IncompatibleClassChangeError", &[]),
    ("java/lang/NoSuchFieldError", "java/lang/IncompatibleClassChangeError", &[]),
    ("java/io/IOException", "java/lang/Exception", &[]),
    ("java/io/UnsupportedEncodingException", "java/io/IOException", &[]),
    ("java/lang/Iterable", "java/lang/Object", &[]),
    ("java/util/Collection", "java/lang/Object", &["java/lang/Iterable"]),
    ("java/util/Set", "java/lang/Object", &["java/util/Collection"]),
    ("java/util/List", "java/lang/Object", &["java/util/Collection"]),
    ("java/util/SortedMap", "java/lang/Object", &["java/util/Map"]),
    ("java/util/NavigableMap", "java/lang/Object", &["java/util/SortedMap"]),
    ("java/util/AbstractCollection", "java/lang/Object", &["java/util/Collection"]),
    ("java/util/AbstractSet", "java/util/AbstractCollection", &["java/util/Set"]),
    ("java/util/AbstractList", "java/util/AbstractCollection", &["java/util/List"]),
    ("java/util/AbstractMap", "java/lang/Object", &["java/util/Map"]),
    ("java/util/HashMap", "java/util/AbstractMap", &["java/util/Map", "java/lang/Cloneable", "java/io/Serializable"]),
    ("java/util/LinkedHashMap", "java/util/HashMap", &["java/util/Map"]),
    ("java/util/TreeMap", "java/util/AbstractMap", &["java/util/NavigableMap"]),
    ("java/util/HashSet", "java/util/AbstractSet", &["java/util/Set"]),
    ("java/util/LinkedHashSet", "java/util/HashSet", &["java/util/Set"]),
    ("java/util/TreeSet", "java/util/AbstractSet", &["java/util/Set"]),
    ("java/util/ArrayList", "java/util/AbstractList", &["java/util/List"]),
    ("java/util/HashMap$KeySet", "java/util/AbstractSet", &[]),
    ("java/util/HashMap$Values", "java/util/AbstractCollection", &[]),
    ("java/util/HashMap$EntrySet", "java/util/AbstractSet", &[]),
    ("java/util/HashMap$Node", "java/lang/Object", &["java/util/Map$Entry"]),
    ("java/util/HashMap$HashIterator", "java/lang/Object", &["java/util/Iterator"]),
    ("java/nio/charset/Charset", "java/lang/Object", &["java/lang/Comparable"]),
];

/// The models, asked after the embedder's handler.
pub struct Builtins;

impl Jni for Builtins {
    fn call(&mut self, vm: &mut Vm, call: &Call) -> JResult<Value> {
        let key = format!("{}{}", call.name, call.desc);
        if let Some(this) = call.this {
            if let Some(result) = util::instance(vm, call, this, &key)? {
                return Ok(result);
            }
            if let Some(result) = lang::instance(vm, call, this, &key)? {
                return Ok(result);
            }
        } else if let Some(result) = lang::static_call(vm, call, &key)? {
            return Ok(result);
        } else if let Some(result) = util::static_call(vm, call, &key)? {
            return Ok(result);
        }
        Err(Fail::Unhandled)
    }

    fn get_field(&mut self, vm: &mut Vm, field: &javavm::FieldAccess) -> JResult<Value> {
        lang::static_field(vm, field).ok_or(Fail::Unhandled)
    }
}

/// Throws `class` with `message` from a model.
pub(crate) fn throw(vm: &mut Vm, class: &str, message: &str) -> Fail {
    Fail::Throw(vm.new_throwable(class, Some(message)))
}

/// The object argument `index`, or a NullPointerException.
pub(crate) fn object_arg(vm: &mut Vm, call: &Call, index: usize) -> JResult<ObjectId> {
    call.args.get(index).and_then(|v| v.object()).ok_or_else(|| throw(vm, "java/lang/NullPointerException", "argument is null"))
}
