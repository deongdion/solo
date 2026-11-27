//! Host functions reached through `svc #imm` stubs: the dynamic linker interface (libdl) and
//! reporting stubs for symbols nothing defines.

use std::collections::HashMap;
use std::time::Instant;

use backend::{Arch, Cpu, Reg};

use crate::{Emulator, Error, HOST_BASE, layout};

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum HostFunction {
    Dlopen,
    AndroidDlopenExt,
    Dlsym,
    Dlvsym,
    Dlerror,
    Dlclose,
    Dladdr,
    DlIteratePhdr,
    DlUnwindFindExidx,
    TargetSdkVersion,
    SetTargetSdkVersion,
    UpdateLibraryPath,
    GetLibraryPath,
    /// A strong reference the linker could not bind; calling it is reported and returns 0.
    Unresolved { module: String, symbol: String },
}

impl HostFunction {
    fn for_libdl(symbol: &str) -> Option<HostFunction> {
        Some(match symbol {
            "dlopen" => HostFunction::Dlopen,
            "android_dlopen_ext" => HostFunction::AndroidDlopenExt,
            "dlsym" => HostFunction::Dlsym,
            "dlvsym" => HostFunction::Dlvsym,
            "dlerror" => HostFunction::Dlerror,
            "dlclose" => HostFunction::Dlclose,
            "dladdr" => HostFunction::Dladdr,
            "dl_iterate_phdr" => HostFunction::DlIteratePhdr,
            "dl_unwind_find_exidx" => HostFunction::DlUnwindFindExidx,
            "android_get_application_target_sdk_version" => HostFunction::TargetSdkVersion,
            "android_set_application_target_sdk_version" => HostFunction::SetTargetSdkVersion,
            "android_update_LD_LIBRARY_PATH" => HostFunction::UpdateLibraryPath,
            "android_get_LD_LIBRARY_PATH" => HostFunction::GetLibraryPath,
            _ => return None,
        })
    }
}

/// One `dlopen` call, for the load timeline (PLAN.md F8).
#[derive(Debug, Clone)]
pub struct DlopenEvent {
    pub name: String,
    pub caller: String,
    pub handle: u64,
    pub millis: f64,
}

#[derive(Default)]
pub struct Host {
    pub functions: Vec<HostFunction>,
    stubs: HashMap<HostFunction, u64>,
    pub data_next: u64,
    strings: HashMap<String, u64>,
    pub environ: u64,
    pub argv: u64,
    error: Option<String>,
    error_buffer: u64,
    pub dlopen_events: Vec<DlopenEvent>,
    pub unresolved_calls: HashMap<(String, String), u64>,
    sdk_version: u64,
}

impl Host {
    /// Returns the stub address for `function`, writing the stub on first use.
    fn stub(&mut self, cpu: &mut dyn Cpu, function: HostFunction) -> u64 {
        if let Some(&at) = self.stubs.get(&function) {
            return at;
        }
        let index = self.functions.len() as u32;
        let at = layout::HOST_STUBS + index as u64 * 8;
        let imm = HOST_BASE + index;
        let (first, second) = match cpu.arch() {
            Arch::Arm64 => (0xd400_0001 | (imm << 5), 0xd65f_03c0),
            Arch::Arm32 => (0xef00_0000 | imm, 0xe12f_ff1e),
        };
        let memory = cpu.memory_mut();
        // The host page is mapped read-execute; the host writes it directly.
        memory.write_u32(at, first).expect("host stub page is mapped");
        memory.write_u32(at + 4, second).expect("host stub page is mapped");
        cpu.invalidate(at, 8);
        self.functions.push(function.clone());
        self.stubs.insert(function, at);
        at
    }

    fn alloc(&mut self, cpu: &mut dyn Cpu, bytes: &[u8]) -> Result<u64, Error> {
        let at = (self.data_next + 15) & !15;
        if at + bytes.len() as u64 > layout::HOST_DATA + layout::HOST_DATA_SIZE {
            return Err(Error::Memory(machine::Error::NoSpace(bytes.len() as u64)));
        }
        cpu.memory_mut().write(at, bytes)?;
        self.data_next = at + bytes.len() as u64;
        Ok(at)
    }

    fn string(&mut self, cpu: &mut dyn Cpu, text: &str) -> Result<u64, Error> {
        if let Some(&at) = self.strings.get(text) {
            return Ok(at);
        }
        let mut bytes = text.as_bytes().to_vec();
        bytes.push(0);
        let at = self.alloc(cpu, &bytes)?;
        self.strings.insert(text.to_string(), at);
        Ok(at)
    }
}

pub struct LinkHooks<'a> {
    pub host: &'a mut Host,
}

impl linker::Hooks for LinkHooks<'_> {
    fn intercept(&mut self, cpu: &mut dyn Cpu, library: &str, symbol: &str) -> Option<u64> {
        if library != "libdl.so" {
            return None;
        }
        HostFunction::for_libdl(symbol).map(|f| self.host.stub(cpu, f))
    }

    fn unresolved(&mut self, cpu: &mut dyn Cpu, module: &str, symbol: &str) -> u64 {
        let function = HostFunction::Unresolved { module: module.to_string(), symbol: symbol.to_string() };
        self.host.stub(cpu, function)
    }
}

impl Emulator {
    fn arg(&self, index: u8) -> u64 {
        let v = self.cpu.reg(Reg::General(index));
        if self.arch() == Arch::Arm64 { v } else { v & 0xffff_ffff }
    }

    fn guest_string(&self, address: u64) -> Result<String, Error> {
        Ok(String::from_utf8_lossy(&self.cpu.memory().read_cstr(address, 4096)?).into_owned())
    }

    fn caller(&self) -> String {
        let lr = self.cpu.reg(Reg::Lr);
        match self.linker.module_at(lr) {
            Some(m) => format!("{}+{:#x}", m.name, lr - m.base),
            None => format!("{lr:#x}"),
        }
    }

    fn set_dl_error(&mut self, message: Option<String>) {
        self.host.error = message;
    }

    pub(crate) fn host_call(&mut self, function: &HostFunction) -> Result<u64, Error> {
        match function {
            HostFunction::Dlopen | HostFunction::AndroidDlopenExt => {
                let address = self.arg(0);
                if address == 0 {
                    // dlopen(NULL) returns a handle for the global scope.
                    return Ok(self.linker.modules.first().map(|m| m.base).unwrap_or(0));
                }
                let name = self.guest_string(address)?;
                let caller = self.caller();
                let started = Instant::now();
                let result = match self.load(&name) {
                    Ok(index) => {
                        self.set_dl_error(None);
                        self.linker.modules[index].base
                    }
                    Err(Error::Linker(e)) => {
                        self.set_dl_error(Some(format!("dlopen failed: {e}")));
                        0
                    }
                    Err(other) => return Err(other),
                };
                self.host.dlopen_events.push(DlopenEvent {
                    name,
                    caller,
                    handle: result,
                    millis: started.elapsed().as_secs_f64() * 1000.0,
                });
                Ok(result)
            }
            HostFunction::Dlsym | HostFunction::Dlvsym => {
                let handle = self.arg(0);
                let name = self.guest_string(self.arg(1))?;
                let found = self.dlsym(handle, &name);
                if found == 0 {
                    self.set_dl_error(Some(format!("undefined symbol: {name}")));
                }
                Ok(found)
            }
            HostFunction::Dlerror => match self.host.error.take() {
                Some(message) => {
                    if self.host.error_buffer == 0 {
                        self.host.error_buffer = self.host.alloc(self.cpu.as_mut(), &[0u8; 512])?;
                    }
                    let mut bytes = message.into_bytes();
                    bytes.truncate(511);
                    bytes.push(0);
                    self.cpu.memory_mut().write(self.host.error_buffer, &bytes)?;
                    Ok(self.host.error_buffer)
                }
                None => Ok(0),
            },
            HostFunction::Dlclose => Ok(0),
            HostFunction::Dladdr => {
                let address = self.arg(0);
                let info = self.arg(1);
                let Some(module) = self.linker.module_at(address).cloned() else { return Ok(0) };
                let word = self.word();
                let path = self.host.string(self.cpu.as_mut(), &module.path)?;
                let (sname, saddr) = match module.closest_symbol(address) {
                    Some((name, at)) => (self.host.string(self.cpu.as_mut(), &name)?, at),
                    None => (0, 0),
                };
                for (i, v) in [path, module.base, sname, saddr].into_iter().enumerate() {
                    self.write_guest_word(info + i as u64 * word, v)?;
                }
                Ok(1)
            }
            HostFunction::DlIteratePhdr => {
                let callback = self.arg(0);
                let data = self.arg(1);
                let word = self.word();
                let size = 4 * word;
                let modules = self.linker.modules.clone();
                let info = self.host.alloc(self.cpu.as_mut(), &vec![0u8; size as usize])?;
                for module in modules {
                    let name = self.host.string(self.cpu.as_mut(), &module.path)?;
                    self.write_guest_word(info, module.base)?;
                    self.write_guest_word(info + word, name)?;
                    self.write_guest_word(info + 2 * word, module.phdr)?;
                    self.write_guest_word(info + 3 * word, module.phnum as u64)?;
                    let result = self.call(callback, &[info, size, data])?;
                    if result != 0 {
                        return Ok(result);
                    }
                }
                Ok(0)
            }
            HostFunction::DlUnwindFindExidx => {
                let pc = self.arg(0);
                let count = self.arg(1);
                match self.linker.module_at(pc).and_then(|m| m.exidx) {
                    Some((start, bytes)) => {
                        self.cpu.memory_mut().write_u32(count, (bytes / 8) as u32)?;
                        Ok(start)
                    }
                    None => Ok(0),
                }
            }
            HostFunction::TargetSdkVersion => Ok(if self.host.sdk_version == 0 { 23 } else { self.host.sdk_version }),
            HostFunction::SetTargetSdkVersion => {
                self.host.sdk_version = self.arg(0);
                Ok(0)
            }
            HostFunction::UpdateLibraryPath => Ok(0),
            HostFunction::GetLibraryPath => {
                let buffer = self.arg(0);
                let text = b"/system/lib64:/vendor/lib64\0";
                if buffer != 0 {
                    let size = (self.arg(1) as usize).min(text.len());
                    if size > 0 {
                        self.cpu.memory_mut().write(buffer, &text[..size])?;
                    }
                }
                Ok(0)
            }
            HostFunction::Unresolved { module, symbol } => {
                *self.host.unresolved_calls.entry((module.clone(), symbol.clone())).or_default() += 1;
                eprintln!("[unresolved] {module} called {symbol} from {}; returning 0", self.caller());
                Ok(0)
            }
        }
    }

    fn write_guest_word(&mut self, at: u64, value: u64) -> Result<(), Error> {
        if self.arch() == Arch::Arm64 {
            self.cpu.memory_mut().write_u64(at, value)?;
        } else {
            self.cpu.memory_mut().write_u32(at, value as u32)?;
        }
        Ok(())
    }

    /// `dlsym`: a module handle searches that module and its dependencies; RTLD_DEFAULT and
    /// RTLD_NEXT search every loaded module in load order.
    fn dlsym(&mut self, handle: u64, name: &str) -> u64 {
        let (default, next) = if self.arch() == Arch::Arm64 { (0, u64::MAX) } else { (0xffff_ffff, 0xffff_fffe) };
        let candidates: Vec<usize> = if handle == default || handle == next || handle == 0 {
            (0..self.linker.modules.len()).collect()
        } else {
            match self.linker.modules.iter().position(|m| m.base == handle) {
                Some(index) => {
                    let mut group = vec![index];
                    let mut i = 0;
                    while i < group.len() {
                        for needed in self.linker.modules[group[i]].needed.clone() {
                            if let Some(dep) = self.linker.find(&needed) {
                                if !group.contains(&dep) {
                                    group.push(dep);
                                }
                            }
                        }
                        i += 1;
                    }
                    group
                }
                None => return 0,
            }
        };
        for index in candidates {
            let module = &self.linker.modules[index];
            if let Some(address) = module.symbol(name) {
                let library = module.soname.clone().unwrap_or_else(|| module.name.clone());
                if library == "libdl.so" {
                    if let Some(f) = HostFunction::for_libdl(name) {
                        return self.host.stub(self.cpu.as_mut(), f);
                    }
                }
                return address;
            }
        }
        0
    }
}
