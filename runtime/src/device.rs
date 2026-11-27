//! Installing a device profile (PLAN.md F15): the guest reads its system properties through
//! `__system_property_get`, intercepted to answer from the profile, and `/proc/cpuinfo` from the
//! same profile, so every source describes one coherent device.

use std::sync::Arc;

use backend::Arch;
use ::device::Profile;

use crate::hooks::{Flow, Hook};
use crate::{Emulator, Error};

/// The longest value `__system_property_get` may return (bionic's `PROP_VALUE_MAX`), minus the
/// terminator.
const PROP_VALUE_MAX: usize = 91;

impl Emulator {
    /// The architecture as the device profile names it.
    pub(crate) fn profile_arch(&self) -> ::device::profile::Arch {
        match self.arch() {
            Arch::Arm64 => ::device::profile::Arch::Arm64,
            Arch::Arm32 => ::device::profile::Arch::Arm32,
        }
    }

    /// Installs a device profile: writes `/proc/cpuinfo` for the profile and intercepts
    /// `__system_property_get` on `libc` so the guest's property reads answer from it. Call after
    /// loading libc and before the code that reads properties (init functions, `JNI_OnLoad`).
    /// The profile is carried by `fork` and `restore`.
    ///
    /// `profile.check(..)` reports contradictions; this does not reject a profile, so an
    /// intentionally odd one can be installed.
    pub fn install_device_profile(&mut self, libc: usize, profile: Profile) -> Result<(), Error> {
        let arch = self.profile_arch();
        self.kernel.vfs.add_file("/proc/cpuinfo", profile.cpuinfo(arch).into_bytes());
        let profile = Arc::new(profile);
        self.profile = Some(profile.clone());

        // int __system_property_get(const char* name, char* value): writes the value (NUL
        // terminated) into `value` and returns its length, or 0 when the property is unset.
        let hook = Hook::on_enter(move |emu, call| {
            let name = emu.cpu.memory().read_cstr(call.arg(emu, 0)?, 256)?;
            let name = String::from_utf8_lossy(&name).into_owned();
            let value = match profile.get(&name) {
                Some(value) => value,
                None => {
                    *emu.kernel.missing_properties.entry(name).or_insert(0) += 1;
                    ""
                }
            };
            let out = call.arg(emu, 1)?;
            let mut bytes = value.as_bytes();
            if bytes.len() > PROP_VALUE_MAX {
                bytes = &bytes[..PROP_VALUE_MAX];
            }
            let mut buf = bytes.to_vec();
            buf.push(0);
            emu.cpu.memory_mut().write(out, &buf)?;
            Ok(Flow::Return(bytes.len() as u64))
        });
        self.hook_symbol(libc, "__system_property_get", hook)?;
        Ok(())
    }

    /// The installed profile, if any.
    pub fn device_profile(&self) -> Option<&Profile> {
        self.profile.as_deref()
    }
}
