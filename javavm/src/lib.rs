//! JNIEnv and JavaVM tables, reference tables, reentrant calls, exception propagation.
//!
//! The VM holds Java state on the host side: objects, classes, method and field IDs, references
//! and the pending exception. Guest code reaches it through the JNI function table (`jni`), whose
//! slots are host stubs. Java behaviour comes from handlers: one `Jni::call` sees every method
//! call variant (PLAN.md F2), and anything no handler answers is reported with its signature and
//! handled by the configured policy (F1).

pub mod args;
pub mod arena;
pub mod heap;
pub mod jni;
pub mod mangle;
pub mod mutf8;
pub mod refs;
pub mod table;
pub mod value;

use std::collections::{BTreeMap, HashMap};

pub use heap::{Array, ClassId, Data, Heap, Object, ObjectId};
pub use refs::{RefKind, Refs};
pub use value::{JType, MethodSig, Value};

/// `JNI_VERSION_1_6`, what ART reports.
pub const JNI_VERSION: u32 = 0x0001_0006;

#[derive(Debug)]
pub struct Class {
    pub name: String,
    /// The java.lang.Class object.
    pub object: ObjectId,
    pub superclass: Option<ClassId>,
    pub interfaces: Vec<ClassId>,
    pub statics: HashMap<String, Value>,
    /// Natives bound by RegisterNatives: (name, descriptor) to guest address.
    pub natives: HashMap<(String, String), u64>,
}

#[derive(Debug, Clone)]
pub struct Method {
    pub class: ClassId,
    pub name: String,
    pub desc: String,
    pub sig: MethodSig,
    pub is_static: bool,
}

#[derive(Debug, Clone)]
pub struct Field {
    pub class: ClassId,
    pub name: String,
    pub desc: String,
    pub ty: JType,
    pub is_static: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallKind {
    Static,
    /// `CallXxxMethod`: dispatched on the object.
    Virtual,
    /// `CallNonvirtualXxxMethod`: the named class's implementation.
    Nonvirtual,
    /// `NewObject`: a constructor (`<init>`) on a freshly allocated object.
    Constructor,
}

/// One Java method call from native code, whichever JNI variant made it.
#[derive(Debug)]
pub struct Call<'a> {
    pub kind: CallKind,
    /// Class the method ID was looked up on (the interface for `Map.get`, not `HashMap`).
    pub class: &'a str,
    pub name: &'a str,
    pub desc: &'a str,
    pub sig: &'a MethodSig,
    pub this: Option<ObjectId>,
    pub args: &'a [Value],
}

impl Call<'_> {
    /// `com/example/Foo->bar(I)Ljava/lang/String;`, the form unidbg handlers match on.
    pub fn signature(&self) -> String {
        format!("{}->{}{}", self.class, self.name, self.desc)
    }
}

#[derive(Debug)]
pub struct FieldAccess<'a> {
    pub class: &'a str,
    pub name: &'a str,
    pub desc: &'a str,
    pub ty: &'a JType,
    /// None for a static field.
    pub this: Option<ObjectId>,
}

impl FieldAccess<'_> {
    /// `com/example/Foo->count:I`.
    pub fn signature(&self) -> String {
        format!("{}->{}:{}", self.class, self.name, self.desc)
    }
}

/// Why a handler did not produce a value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fail {
    /// Not this handler's method; the next handler is asked.
    Unhandled,
    /// Throw this exception into the guest.
    Throw(ObjectId),
}

pub type JResult<T> = Result<T, Fail>;

/// Java behaviour supplied by the embedder or a built-in model.
pub trait Jni: Send {
    fn call(&mut self, vm: &mut Vm, call: &Call) -> JResult<Value> {
        let _ = (vm, call);
        Err(Fail::Unhandled)
    }

    fn get_field(&mut self, vm: &mut Vm, field: &FieldAccess) -> JResult<Value> {
        let _ = (vm, field);
        Err(Fail::Unhandled)
    }

    /// Unhandled stores are kept on the object (or class), as Java would.
    fn set_field(&mut self, vm: &mut Vm, field: &FieldAccess, value: Value) -> JResult<()> {
        let _ = (vm, field, value);
        Err(Fail::Unhandled)
    }

    /// Whether `FindClass` succeeds for `name`. Unknown classes exist by default.
    fn class_exists(&mut self, vm: &mut Vm, name: &str) -> Option<bool> {
        let _ = (vm, name);
        None
    }
}

/// What happens when no handler answers a call or field read (PLAN.md F1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum UnhandledPolicy {
    /// Stop the emulation with an error naming the signature.
    #[default]
    Stop,
    /// Return zero or null and continue.
    Zero,
    /// Throw `java/lang/UnsupportedOperationException` into the guest.
    Throw,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnhandledJni {
    pub count: u64,
    /// Return address of the first native caller.
    pub first_caller: u64,
}

/// Module timeline entries the VM produces (PLAN.md F8).
#[derive(Debug, Clone)]
pub enum JniEvent {
    RegisterNatives { class: String, methods: Vec<(String, String, u64)>, caller: u64 },
    UnregisterNatives { class: String, caller: u64 },
    FatalError { message: String, caller: u64 },
}

#[derive(Debug, thiserror::Error)]
pub enum JniError {
    #[error("unhandled JNI callback: {signature}")]
    Unhandled { signature: String },
    #[error("FatalError from native code: {0}")]
    Fatal(String),
    #[error("JNI {function}: {problem}")]
    Misuse { function: &'static str, problem: String },
    #[error(transparent)]
    Arena(#[from] arena::ArenaError),
    #[error(transparent)]
    Memory(#[from] machine::Error),
}

/// A guest buffer handed out by a Get call, waiting for its Release.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Lent {
    pub object: ObjectId,
    pub kind: LentKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LentKind {
    ArrayElements,
    StringChars,
    StringUtf,
}

pub struct Vm {
    pub heap: Heap,
    pub refs: Refs,
    classes: Vec<Class>,
    class_names: HashMap<String, ClassId>,
    methods: Vec<Method>,
    method_index: HashMap<(ClassId, String, String, bool), usize>,
    fields: Vec<Field>,
    field_index: HashMap<(ClassId, String, String, bool), usize>,
    pub pending: Option<ObjectId>,
    pub policy: UnhandledPolicy,
    handler: Option<Box<dyn Jni>>,
    models: Vec<Box<dyn Jni>>,
    pub unhandled: BTreeMap<String, UnhandledJni>,
    pub events: Vec<JniEvent>,
    pub warnings: Vec<String>,
    pub(crate) arena: arena::Arena,
    pub(crate) lent: HashMap<u64, Lent>,
    /// Objects kept alive regardless of references (`pin`).
    pinned: HashMap<ObjectId, u32>,
    /// Guest addresses of the JNIEnv and JavaVM structures, set by `jni::install`.
    pub env: u64,
    pub java_vm: u64,
    /// Return address of the native code that made the current JNI call.
    pub caller: u64,
}

impl Vm {
    /// JNI buffers are placed in `[low, high)` of the guest address space.
    pub fn new(low: u64, high: u64) -> Vm {
        let mut vm = Vm {
            heap: Heap::default(),
            refs: Refs::default(),
            classes: Vec::new(),
            class_names: HashMap::new(),
            methods: Vec::new(),
            method_index: HashMap::new(),
            fields: Vec::new(),
            field_index: HashMap::new(),
            pending: None,
            policy: UnhandledPolicy::default(),
            handler: None,
            models: Vec::new(),
            unhandled: BTreeMap::new(),
            events: Vec::new(),
            warnings: Vec::new(),
            arena: arena::Arena::new(low, high),
            lent: HashMap::new(),
            pinned: HashMap::new(),
            env: 0,
            java_vm: 0,
            caller: 0,
        };
        // java/lang/Class must exist before any Class object can be made.
        let class_class = ClassId(0);
        vm.classes.push(Class {
            name: "java/lang/Class".into(),
            object: ObjectId(u32::MAX),
            superclass: None,
            interfaces: Vec::new(),
            statics: HashMap::new(),
            natives: HashMap::new(),
        });
        vm.class_names.insert("java/lang/Class".into(), class_class);
        let object = vm.heap.alloc(Object { class: class_class, data: Data::Class(class_class), fields: HashMap::new() });
        vm.classes[0].object = object;
        let root = vm.class("java/lang/Object");
        vm.classes[0].superclass = Some(root);
        vm
    }

    /// The embedder's handler, asked before the built-in models.
    pub fn set_handler(&mut self, handler: Box<dyn Jni>) {
        self.handler = Some(handler);
    }

    /// A fallback model, asked after the handler and earlier models.
    pub fn add_model(&mut self, model: Box<dyn Jni>) {
        self.models.push(model);
    }

    // ---- classes ----

    /// The class named `name` (internal form, `java/lang/String` or `[B`), created on first use
    /// with java/lang/Object as its superclass.
    pub fn class(&mut self, name: &str) -> ClassId {
        let name = name.replace('.', "/");
        if let Some(&id) = self.class_names.get(&name) {
            return id;
        }
        let id = ClassId(self.classes.len() as u32);
        let object = self.heap.alloc(Object { class: ClassId(0), data: Data::Class(id), fields: HashMap::new() });
        let superclass = (name != "java/lang/Object").then(|| self.class_names.get("java/lang/Object").copied()).flatten();
        self.classes.push(Class {
            name: name.clone(),
            object,
            superclass,
            interfaces: Vec::new(),
            statics: HashMap::new(),
            natives: HashMap::new(),
        });
        self.class_names.insert(name.clone(), id);
        if superclass.is_none() && name != "java/lang/Object" {
            let root = self.class("java/lang/Object");
            self.classes[id.0 as usize].superclass = Some(root);
        }
        id
    }

    /// Declares a class's superclass and interfaces (for IsInstanceOf and IsAssignableFrom).
    pub fn define_class(&mut self, name: &str, superclass: &str, interfaces: &[&str]) -> ClassId {
        let id = self.class(name);
        let sup = self.class(superclass);
        let ifaces: Vec<ClassId> = interfaces.iter().map(|i| self.class(i)).collect();
        let class = &mut self.classes[id.0 as usize];
        class.superclass = Some(sup);
        class.interfaces = ifaces;
        id
    }

    pub fn class_info(&self, id: ClassId) -> &Class {
        &self.classes[id.0 as usize]
    }

    pub fn class_info_mut(&mut self, id: ClassId) -> &mut Class {
        &mut self.classes[id.0 as usize]
    }

    pub fn find_class(&self, name: &str) -> Option<ClassId> {
        self.class_names.get(name).copied()
    }

    pub fn class_name(&self, id: ClassId) -> &str {
        &self.classes[id.0 as usize].name
    }

    /// The class a java.lang.Class object stands for.
    pub fn class_of_object(&self, class_object: ObjectId) -> Option<ClassId> {
        match self.heap.get(class_object)?.data {
            Data::Class(id) => Some(id),
            _ => None,
        }
    }

    /// Runtime class of an object.
    pub fn object_class(&self, id: ObjectId) -> Option<ClassId> {
        self.heap.get(id).map(|o| o.class)
    }

    /// `sub` is `sup` or extends or implements it (arrays of objects are assignable to
    /// java/lang/Object only, plus identical array types).
    pub fn is_assignable(&self, sub: ClassId, sup: ClassId) -> bool {
        if sub == sup || self.class_name(sup) == "java/lang/Object" {
            return true;
        }
        let mut stack = vec![sub];
        let mut seen = std::collections::HashSet::new();
        while let Some(c) = stack.pop() {
            if c == sup {
                return true;
            }
            if !seen.insert(c) {
                continue;
            }
            let class = &self.classes[c.0 as usize];
            stack.extend(class.superclass);
            stack.extend(class.interfaces.iter().copied());
        }
        false
    }

    // ---- objects ----

    pub fn new_object(&mut self, class: ClassId) -> ObjectId {
        self.heap.alloc(Object { class, data: Data::Plain, fields: HashMap::new() })
    }

    /// An object of `class` carrying host data.
    pub fn new_host_object(&mut self, class: &str, data: Box<dyn std::any::Any + Send>) -> ObjectId {
        let class = self.class(class);
        self.heap.alloc(Object { class, data: Data::Host(data), fields: HashMap::new() })
    }

    pub fn new_string(&mut self, text: &str) -> ObjectId {
        self.new_string_utf16(text.encode_utf16().collect())
    }

    pub fn new_string_utf16(&mut self, units: Vec<u16>) -> ObjectId {
        let class = self.class("java/lang/String");
        self.heap.alloc(Object { class, data: Data::String(units), fields: HashMap::new() })
    }

    /// The text of a java.lang.String (lone surrogates become U+FFFD).
    pub fn string(&self, id: ObjectId) -> Option<String> {
        match &self.heap.get(id)?.data {
            Data::String(units) => Some(String::from_utf16_lossy(units)),
            _ => None,
        }
    }

    pub fn string_units(&self, id: ObjectId) -> Option<&[u16]> {
        match &self.heap.get(id)?.data {
            Data::String(units) => Some(units),
            _ => None,
        }
    }

    pub fn new_array(&mut self, array: Array, element_class: Option<&str>) -> ObjectId {
        let name = match (array.primitive_descriptor(), element_class) {
            (Some(d), _) => d.to_string(),
            (None, Some(e)) if e.starts_with('[') => format!("[{e}"),
            (None, Some(e)) => format!("[L{e};"),
            (None, None) => "[Ljava/lang/Object;".to_string(),
        };
        let class = self.class(&name);
        self.heap.alloc(Object { class, data: Data::Array(array), fields: HashMap::new() })
    }

    pub fn new_bytes(&mut self, bytes: &[u8]) -> ObjectId {
        self.new_array(Array::Byte(bytes.iter().map(|&b| b as i8).collect()), None)
    }

    pub fn array(&self, id: ObjectId) -> Option<&Array> {
        match &self.heap.get(id)?.data {
            Data::Array(a) => Some(a),
            _ => None,
        }
    }

    pub fn array_mut(&mut self, id: ObjectId) -> Option<&mut Array> {
        match &mut self.heap.get_mut(id)?.data {
            Data::Array(a) => Some(a),
            _ => None,
        }
    }

    /// Contents of a byte[] as unsigned bytes.
    pub fn bytes(&self, id: ObjectId) -> Option<Vec<u8>> {
        match self.array(id)? {
            Array::Byte(v) => Some(v.iter().map(|&b| b as u8).collect()),
            Array::Boolean(v) => Some(v.clone()),
            _ => None,
        }
    }

    pub fn host_data<T: 'static>(&self, id: ObjectId) -> Option<&T> {
        match &self.heap.get(id)?.data {
            Data::Host(b) => b.downcast_ref(),
            _ => None,
        }
    }

    pub fn host_data_mut<T: 'static>(&mut self, id: ObjectId) -> Option<&mut T> {
        match &mut self.heap.get_mut(id)?.data {
            Data::Host(b) => b.downcast_mut(),
            _ => None,
        }
    }

    /// A Throwable of `class` with `message` (stored in `detailMessage`, as libcore does).
    pub fn new_throwable(&mut self, class: &str, message: Option<&str>) -> ObjectId {
        let class = self.class(class);
        let object = self.new_object(class);
        if let Some(m) = message {
            let text = self.new_string(m);
            self.heap.get_mut(object).expect("just allocated").fields.insert("detailMessage".into(), Value::Object(text));
        }
        object
    }

    /// Class name and message of a Throwable, for reports.
    pub fn describe_throwable(&self, id: ObjectId) -> String {
        let Some(object) = self.heap.get(id) else { return "<collected>".into() };
        let name = self.class_name(object.class).replace('/', ".");
        match object.fields.get("detailMessage").and_then(|v| v.object()).and_then(|m| self.string(m)) {
            Some(message) => format!("{name}: {message}"),
            None => name,
        }
    }

    pub fn throw(&mut self, exception: ObjectId) {
        self.pending = Some(exception);
    }

    /// Keeps an object alive across collections until `unpin` (counted).
    pub fn pin(&mut self, id: ObjectId) {
        *self.pinned.entry(id).or_insert(0) += 1;
    }

    pub fn unpin(&mut self, id: ObjectId) {
        if let Some(count) = self.pinned.get_mut(&id) {
            *count -= 1;
            if *count == 0 {
                self.pinned.remove(&id);
            }
        }
    }

    /// Frees objects nothing can reach: roots are references, pins, class objects and statics,
    /// the pending exception, objects behind lent buffers, and `extra`.
    pub fn collect(&mut self, extra: &[ObjectId]) -> usize {
        let mut roots: Vec<ObjectId> = self.refs.roots().collect();
        roots.extend(self.pinned.keys().copied());
        roots.extend(extra.iter().copied());
        roots.extend(self.pending);
        roots.extend(self.lent.values().map(|l| l.object));
        for class in &self.classes {
            roots.push(class.object);
            roots.extend(class.statics.values().filter_map(|v| v.object()));
        }
        let freed = self.heap.collect(roots);
        let heap = &self.heap;
        self.refs.clear_weak(|id| heap.get(id).is_some());
        freed
    }

    // ---- method and field IDs ----

    pub fn method_id(&mut self, class: ClassId, name: &str, desc: &str, is_static: bool) -> Option<usize> {
        let key = (class, name.to_string(), desc.to_string(), is_static);
        if let Some(&i) = self.method_index.get(&key) {
            return Some(i);
        }
        let sig = MethodSig::parse(desc)?;
        self.methods.push(Method { class, name: name.into(), desc: desc.into(), sig, is_static });
        self.method_index.insert(key, self.methods.len() - 1);
        Some(self.methods.len() - 1)
    }

    pub fn method(&self, index: usize) -> Option<&Method> {
        self.methods.get(index)
    }

    pub fn field_id(&mut self, class: ClassId, name: &str, desc: &str, is_static: bool) -> Option<usize> {
        let key = (class, name.to_string(), desc.to_string(), is_static);
        if let Some(&i) = self.field_index.get(&key) {
            return Some(i);
        }
        let (ty, rest) = JType::parse(desc)?;
        if !rest.is_empty() || ty == JType::Void {
            return None;
        }
        self.fields.push(Field { class, name: name.into(), desc: desc.into(), ty, is_static });
        self.field_index.insert(key, self.fields.len() - 1);
        Some(self.fields.len() - 1)
    }

    pub fn field(&self, index: usize) -> Option<&Field> {
        self.fields.get(index)
    }

    /// The registered native implementing a method: on the class, or for a virtual call on
    /// the object's class or one of its superclasses.
    pub fn native_for(&self, method: &Method, this: Option<ObjectId>) -> Option<u64> {
        let key = (method.name.clone(), method.desc.clone());
        let mut class = match (this.and_then(|t| self.object_class(t)), method.is_static) {
            (Some(runtime), false) => Some(runtime),
            _ => Some(method.class),
        };
        while let Some(c) = class {
            let info = &self.classes[c.0 as usize];
            if let Some(&f) = info.natives.get(&key) {
                return Some(f);
            }
            class = info.superclass;
        }
        self.classes[method.class.0 as usize].natives.get(&key).copied()
    }

    // ---- handlers ----

    fn record_unhandled(&mut self, signature: &str) {
        let caller = self.caller;
        self.unhandled.entry(signature.to_string()).or_insert(UnhandledJni { count: 0, first_caller: caller }).count += 1;
    }

    /// Asks the handler, then each model. `Err(Fail::Unhandled)` when nobody answered.
    fn ask<T>(&mut self, mut f: impl FnMut(&mut dyn Jni, &mut Vm) -> JResult<T>) -> JResult<T> {
        if let Some(mut handler) = self.handler.take() {
            let result = f(handler.as_mut(), self);
            self.handler = Some(handler);
            if !matches!(result, Err(Fail::Unhandled)) {
                return result;
            }
        }
        let mut models = std::mem::take(&mut self.models);
        let mut result = Err(Fail::Unhandled);
        for model in models.iter_mut() {
            result = f(model.as_mut(), self);
            if !matches!(result, Err(Fail::Unhandled)) {
                break;
            }
        }
        // A model may have added models; keep them after the existing ones.
        models.append(&mut self.models);
        self.models = models;
        result
    }

    /// Applies the unhandled policy. `Ok(zero)` when the guest should continue.
    fn unhandled_result(&mut self, signature: String, zero: Value) -> Result<Value, JniError> {
        self.record_unhandled(&signature);
        match self.policy {
            UnhandledPolicy::Stop => Err(JniError::Unhandled { signature }),
            UnhandledPolicy::Zero => Ok(zero),
            UnhandledPolicy::Throw => {
                let e = self.new_throwable("java/lang/UnsupportedOperationException", Some(&signature));
                self.throw(e);
                Ok(zero)
            }
        }
    }

    /// Runs a Java method call through the handlers and the policy. A thrown exception becomes
    /// pending and the call returns zero.
    pub fn invoke(&mut self, kind: CallKind, method: &Method, this: Option<ObjectId>, args: &[Value]) -> Result<Value, JniError> {
        if args.len() != method.sig.params.len() {
            return Err(JniError::Misuse {
                function: "invoke",
                problem: format!("{}{} takes {} arguments, {} given", method.name, method.desc, method.sig.params.len(), args.len()),
            });
        }
        let class_name = self.class_name(method.class).to_string();
        let call = Call { kind, class: &class_name, name: &method.name, desc: &method.desc, sig: &method.sig, this, args };
        let zero = method.sig.ret.zero();
        match self.ask(|h, vm| h.call(vm, &call)) {
            Ok(v) => Ok(if kind == CallKind::Constructor { Value::Void } else { v.coerce(&method.sig.ret) }),
            Err(Fail::Throw(e)) => {
                self.throw(e);
                Ok(zero)
            }
            Err(Fail::Unhandled) => {
                // Object.<init> and similar no-op constructors need no handler.
                if kind == CallKind::Constructor && method.desc == "()V" && class_name == "java/lang/Object" {
                    return Ok(Value::Void);
                }
                let signature = call.signature();
                self.unhandled_result(signature, zero)
            }
        }
    }

    pub fn read_field(&mut self, field: &Field, this: Option<ObjectId>) -> Result<Value, JniError> {
        let class_name = self.class_name(field.class).to_string();
        let access = FieldAccess { class: &class_name, name: &field.name, desc: &field.desc, ty: &field.ty, this };
        match self.ask(|h, vm| h.get_field(vm, &access)) {
            Ok(v) => Ok(v.coerce(&field.ty)),
            Err(Fail::Throw(e)) => {
                self.throw(e);
                Ok(field.ty.zero())
            }
            Err(Fail::Unhandled) => {
                let stored = match this {
                    Some(obj) => self.heap.get(obj).and_then(|o| o.fields.get(&field.name).copied()),
                    None => self.classes[field.class.0 as usize].statics.get(&field.name).copied(),
                };
                match stored {
                    Some(v) => Ok(v),
                    None => {
                        let signature = access.signature();
                        self.unhandled_result(signature, field.ty.zero())
                    }
                }
            }
        }
    }

    pub fn write_field(&mut self, field: &Field, this: Option<ObjectId>, value: Value) -> Result<(), JniError> {
        let value = value.coerce(&field.ty);
        let class_name = self.class_name(field.class).to_string();
        let access = FieldAccess { class: &class_name, name: &field.name, desc: &field.desc, ty: &field.ty, this };
        match self.ask(|h, vm| h.set_field(vm, &access, value)) {
            Ok(()) => Ok(()),
            Err(Fail::Throw(e)) => {
                self.throw(e);
                Ok(())
            }
            Err(Fail::Unhandled) => {
                match this {
                    Some(obj) => {
                        if let Some(o) = self.heap.get_mut(obj) {
                            o.fields.insert(field.name.clone(), value);
                        }
                    }
                    None => {
                        self.classes[field.class.0 as usize].statics.insert(field.name.clone(), value);
                    }
                }
                Ok(())
            }
        }
    }

    /// Whether FindClass finds `name`: the handler's and models' answer, else yes.
    pub fn class_exists(&mut self, name: &str) -> bool {
        let answer = self.ask(|h, vm| h.class_exists(vm, name).ok_or(Fail::Unhandled));
        answer.unwrap_or(true)
    }
}
