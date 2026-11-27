//! JNI on the emulator: the function tables as host stubs, registered natives called back from
//! JNI, and the host-side API for calling native methods.

use std::time::Instant;

use javavm::args::{Returned, Slot};
use javavm::jni::{self, Action};
use javavm::{CallKind, JType, ObjectId, Value, mangle, table};

use crate::host::HostFunction;
use crate::{Emulator, Error};

/// One step of a module's JNI life, for the timeline (PLAN.md F8).
#[derive(Debug, Clone)]
pub struct OnLoadEvent {
    pub module: String,
    pub version: u32,
    pub millis: f64,
}

impl Emulator {
    pub(crate) fn install_jni(&mut self) -> Result<(), Error> {
        let mut native = Vec::with_capacity(table::NATIVE.len());
        for (slot, name) in table::NATIVE.iter().enumerate() {
            native.push(if name.starts_with("reserved") { 0 } else { self.host.stub(self.cpu.as_mut(), HostFunction::Jni(slot as u16)) });
        }
        let mut invoke = Vec::with_capacity(table::INVOKE.len());
        for (slot, name) in table::INVOKE.iter().enumerate() {
            invoke.push(if name.starts_with("reserved") { 0 } else { self.host.stub(self.cpu.as_mut(), HostFunction::JavaVm(slot as u16)) });
        }
        jni::install(&mut self.vm, self.cpu.as_mut(), &native, &invoke).map_err(|e| self.jni_error(e))?;
        // Locals made outside any call (by init code, for example) live in this base frame.
        self.vm.refs.push_frame();
        Ok(())
    }

    fn jni_error(&self, error: javavm::JniError) -> Error {
        Error::Jni { error, caller: self.locate(self.vm.caller) }
    }

    pub(crate) fn locate(&self, address: u64) -> String {
        match self.linker.module_at(address) {
            Some(m) => {
                let symbol = m.closest_symbol(address).map(|(n, at)| format!(" ({n}+{:#x})", address - at)).unwrap_or_default();
                format!("{}+{:#x}{symbol}", m.name, address - m.base)
            }
            None => format!("{address:#x}"),
        }
    }

    pub(crate) fn jni_slot(&mut self, slot: usize) -> Result<(), Error> {
        match jni::dispatch(&mut self.vm, self.cpu.as_mut(), slot) {
            Ok(Action::Done) => Ok(()),
            Ok(Action::Native { function, slots, ret }) => {
                let returned = self.call_slots(function, &slots);
                match returned {
                    Ok(r) => {
                        jni::complete_native(&mut self.vm, self.cpu.as_mut(), &ret, r);
                        Ok(())
                    }
                    Err(e) => {
                        self.vm.refs.pop_frame();
                        Err(e)
                    }
                }
            }
            Err(e) => Err(self.jni_error(e)),
        }
    }

    pub(crate) fn java_vm_slot(&mut self, slot: usize) -> Result<(), Error> {
        jni::dispatch_invoke(&mut self.vm, self.cpu.as_mut(), slot).map_err(|e| self.jni_error(e))
    }

    /// Calls `JNI_OnLoad(JavaVM*, NULL)` of a loaded module, as `System.loadLibrary` does, and
    /// returns the JNI version it reports. A module without `JNI_OnLoad` returns `None`.
    pub fn call_jni_onload(&mut self, module: usize) -> Result<Option<u32>, Error> {
        let Some(function) = self.symbol(module, "JNI_OnLoad") else { return Ok(None) };
        let started = Instant::now();
        self.vm.refs.push_frame();
        let result = self.call(function, &[self.vm.java_vm, 0]);
        self.vm.refs.pop_frame();
        let version = result? as u32;
        self.onload_events.push(OnLoadEvent {
            module: self.linker.modules[module].name.clone(),
            version,
            millis: started.elapsed().as_secs_f64() * 1000.0,
        });
        self.take_exception()?;
        Ok(Some(version))
    }

    /// The guest function implementing a native method: a RegisterNatives binding, else an
    /// exported `Java_` symbol (short name, then the overload name).
    pub fn find_native(&mut self, class: &str, name: &str, desc: &str, is_static: bool, this: Option<ObjectId>) -> Option<u64> {
        let class_id = self.vm.class(class);
        let index = self.vm.method_id(class_id, name, desc, is_static)?;
        let method = self.vm.method(index)?.clone();
        if let Some(f) = self.vm.native_for(&method, this) {
            return Some(f);
        }
        let short = mangle::short_name(class, name);
        let long = mangle::long_name(class, name, desc);
        self.linker.modules.iter().find_map(|m| m.symbol(&short).or_else(|| m.symbol(&long)))
    }

    /// Calls a static native method, `Class.name(args)`.
    pub fn call_static(&mut self, class: &str, name: &str, desc: &str, args: &[Value]) -> Result<Value, Error> {
        self.call_native(CallKind::Static, class, name, desc, None, args)
    }

    /// Calls an instance native method on `this`.
    pub fn call_method(&mut self, this: ObjectId, class: &str, name: &str, desc: &str, args: &[Value]) -> Result<Value, Error> {
        self.call_native(CallKind::Virtual, class, name, desc, Some(this), args)
    }

    fn call_native(
        &mut self,
        kind: CallKind,
        class: &str,
        name: &str,
        desc: &str,
        this: Option<ObjectId>,
        args: &[Value],
    ) -> Result<Value, Error> {
        let class = class.replace('.', "/");
        let signature = format!("{class}->{name}{desc}");
        let sig = javavm::MethodSig::parse(desc).ok_or_else(|| Error::NoNative(format!("{signature} (bad descriptor)")))?;
        if sig.params.len() != args.len() {
            return Err(Error::NoNative(format!("{signature}: {} arguments given for {}", args.len(), sig.params.len())));
        }
        let function = self
            .find_native(&class, name, desc, kind == CallKind::Static, this)
            .ok_or_else(|| Error::NoNative(signature.clone()))?;
        self.vm.refs.push_frame();
        let target = match this {
            Some(t) => self.vm.refs.new_local(Some(t)),
            None => {
                let id = self.vm.class(&class);
                let object = self.vm.class_info(id).object;
                self.vm.refs.new_local(Some(object))
            }
        };
        let mut slots = vec![Slot::Word(self.vm.env), Slot::Word(target)];
        for (value, t) in args.iter().zip(&sig.params) {
            slots.push(match t {
                JType::Object(_) => Slot::Word(self.vm.refs.new_local(value.object())),
                _ => Slot::from_value(value.coerce(t)),
            });
        }
        let returned: Result<Returned, Error> = self.call_slots(function, &slots);
        let result = returned.map(|r| {
            let value = r.value(self.arch(), &sig.ret);
            match sig.ret {
                JType::Object(_) => Value::from_object(self.vm.refs.get(value.as_i64() as u64).ok().flatten()),
                _ => value.coerce(&sig.ret),
            }
        });
        self.vm.refs.pop_frame();
        let value = result?;
        self.take_exception()?;
        if self.depth == 0 {
            let keep: Vec<ObjectId> = value.object().into_iter().chain(this).chain(args.iter().filter_map(|a| a.object())).collect();
            self.maybe_collect(&keep);
        }
        Ok(value)
    }

    fn take_exception(&mut self) -> Result<(), Error> {
        match self.vm.pending.take() {
            Some(exception) => {
                let description = self.vm.describe_throwable(exception);
                Err(Error::JavaException { exception, description })
            }
            None => Ok(()),
        }
    }

    /// Collects the Java heap when it has doubled since the last collection; `keep` (the call's
    /// result, receiver and arguments) survives. Objects the embedder holds across calls must be pinned (`Vm::pin`).
    fn maybe_collect(&mut self, keep: &[ObjectId]) {
        if self.vm.heap.live() < self.collect_at {
            return;
        }
        self.vm.collect(keep);
        self.collect_at = (self.vm.heap.live() * 2).max(4096);
    }
}
