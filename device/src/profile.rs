//! A device profile: the system properties, build fields and `/proc` files that one emulated
//! device answers from, so every source agrees and nothing reads as an emulator (PLAN.md F15).
//!
//! A security SDK reads a wide fingerprint and rejects a signature when the parts disagree (an
//! `arm64-v8a` ABI on an x86 `ro.product.cpu.abi`, a fingerprint that does not match its fields,
//! a `generic`/`sdk`/`emulator` model, a nonzero `TracerPid`). `Profile::check` catches those
//! before a run, against the real device the preset is modelled on.

use std::fmt::Write as _;

/// A coherent set of device facts. Build it from a preset and adjust, or from scratch.
#[derive(Debug, Clone, Default)]
pub struct Profile {
    /// System properties (`ro.*`, `persist.*`, ...), as `__system_property_get` returns them and
    /// `getprop` prints them. Order is the insertion order.
    properties: Vec<(String, String)>,
}

/// A problem found by `check`: two facts that contradict each other or betray an emulator.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Inconsistency {
    pub property: String,
    pub reason: String,
}

impl std::fmt::Display for Inconsistency {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.property, self.reason)
    }
}

/// The CPU architecture a profile is for; its ABI list must match.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Arch {
    Arm64,
    Arm32,
}

impl Profile {
    pub fn new() -> Profile {
        Profile::default()
    }

    /// Google Nexus 5X (bullhead), Android 6.0 (MDA89E), the device unidbg's bundled `sdk23`
    /// property set describes. `arch` picks the primary ABI (a 32-bit process on this device
    /// reports a 32-bit primary ABI with the 64-bit list still present).
    pub fn nexus5x(arch: Arch) -> Profile {
        let (abi, abilist) = match arch {
            Arch::Arm64 => ("arm64-v8a", "arm64-v8a,armeabi-v7a,armeabi"),
            Arch::Arm32 => ("armeabi-v7a", "armeabi-v7a,armeabi"),
        };
        let mut p = Profile::new();
        p.set_all([
            ("ro.build.id", "MDA89E"),
            ("ro.build.display.id", "MDA89E"),
            ("ro.build.version.incremental", "2296692"),
            ("ro.build.version.sdk", "23"),
            ("ro.build.version.release", "6.0"),
            ("ro.build.version.codename", "REL"),
            ("ro.build.version.all_codenames", "REL"),
            ("ro.build.version.security_patch", "2015-10-01"),
            ("ro.build.version.preview_sdk", "0"),
            ("ro.build.version.base_os", ""),
            ("ro.build.type", "user"),
            ("ro.build.tags", "release-keys"),
            ("ro.build.user", "android-build"),
            ("ro.build.host", "wphl1.hot.corp.google.com"),
            ("ro.build.flavor", "bullhead-user"),
            ("ro.build.product", "bullhead"),
            ("ro.build.description", "bullhead-user 6.0 MDA89E 2296692 release-keys"),
            ("ro.build.date.utc", "1443600000"),
            ("ro.build.characteristics", "nosdcard"),
            ("ro.product.model", "Nexus 5X"),
            ("ro.product.brand", "google"),
            ("ro.product.name", "bullhead"),
            ("ro.product.device", "bullhead"),
            ("ro.product.board", "bullhead"),
            ("ro.product.manufacturer", "LGE"),
            ("ro.product.locale", "en-US"),
            ("ro.product.cpu.abi", abi),
            ("ro.product.cpu.abilist", abilist),
            ("ro.product.cpu.abilist32", "armeabi-v7a,armeabi"),
            ("ro.product.cpu.abilist64", if arch == Arch::Arm64 { "arm64-v8a" } else { "" }),
            ("ro.board.platform", "msm8992"),
            ("ro.hardware", "bullhead"),
            ("ro.boot.hardware", "bullhead"),
            ("ro.revision", "0"),
            ("ro.serialno", "00c7d1a9f8e2b3c4"),
            ("ro.boot.serialno", "00c7d1a9f8e2b3c4"),
            ("ro.bootloader", "BHZ10r"),
            ("ro.baseband", "M8994F-2.6.22.0.56"),
            ("ro.boot.baseband", "msm"),
            ("ro.secure", "1"),
            ("ro.debuggable", "0"),
            ("ro.allow.mock.location", "0"),
            ("ro.adb.secure", "1"),
            ("ro.build.fingerprint", "google/bullhead/bullhead:6.0/MDA89E/2296692:user/release-keys"),
            ("ro.bootmode", "unknown"),
            ("ro.treble.enabled", "false"),
            ("gsm.version.baseband", "M8994F-2.6.22.0.56"),
            ("gsm.operator.alpha", ""),
            ("gsm.operator.numeric", ""),
            ("gsm.sim.operator.numeric", ""),
            ("net.hostname", "android-00c7d1a9f8e2b3c4"),
            ("persist.sys.timezone", "America/Los_Angeles"),
            ("persist.sys.locale", "en-US"),
            ("dalvik.vm.isa.arm64.variant", "cortex-a53"),
            ("dalvik.vm.isa.arm.variant", "cortex-a53"),
            ("dalvik.vm.heapsize", "512m"),
        ]);
        p
    }

    pub fn set(&mut self, name: &str, value: &str) -> &mut Profile {
        if let Some(entry) = self.properties.iter_mut().find(|(k, _)| k == name) {
            entry.1 = value.to_string();
        } else {
            self.properties.push((name.to_string(), value.to_string()));
        }
        self
    }

    pub fn set_all<'a>(&mut self, pairs: impl IntoIterator<Item = (&'a str, &'a str)>) -> &mut Profile {
        for (k, v) in pairs {
            self.set(k, v);
        }
        self
    }

    pub fn get(&self, name: &str) -> Option<&str> {
        self.properties.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
    }

    pub fn properties(&self) -> &[(String, String)] {
        &self.properties
    }

    /// `getprop` output: one `[name]: [value]` line per property, sorted by name.
    pub fn getprop(&self) -> String {
        let mut sorted = self.properties.clone();
        sorted.sort();
        let mut out = String::new();
        for (k, v) in sorted {
            let _ = writeln!(out, "[{k}]: [{v}]");
        }
        out
    }

    /// `/proc/cpuinfo` for this device: one block per core plus the hardware summary, matching the
    /// board and ABI. A security SDK compares the Hardware line and core count to the properties.
    pub fn cpuinfo(&self, arch: Arch) -> String {
        let cores = 6; // Snapdragon 808: 4x Cortex-A53 + 2x Cortex-A57.
        let revision = self.get("ro.revision").unwrap_or("0");
        let serial = self.get("ro.serialno").unwrap_or("0000000000000000");
        let mut out = String::new();
        for core in 0..cores {
            match arch {
                Arch::Arm64 => {
                    let _ = write!(
                        out,
                        "processor\t: {core}\nBogoMIPS\t: 38.40\nFeatures\t: fp asimd evtstrm aes pmull sha1 sha2 crc32\n\
                         CPU implementer\t: 0x41\nCPU architecture: 8\nCPU variant\t: 0x0\nCPU part\t: {part}\nCPU revision\t: 2\n\n",
                        part = if core < 4 { "0xd03" } else { "0xd07" },
                    );
                }
                Arch::Arm32 => {
                    let _ = write!(
                        out,
                        "processor\t: {core}\nmodel name\t: ARMv7 Processor rev 2 (v7l)\nBogoMIPS\t: 38.40\n\
                         Features\t: half thumb fastmult vfp edsp neon vfpv3 tls vfpv4 idiva idivt \nCPU implementer\t: 0x41\n\
                         CPU architecture: 7\nCPU variant\t: 0x0\nCPU part\t: {part}\nCPU revision\t: 2\n\n",
                        part = if core < 4 { "0xd03" } else { "0xd07" },
                    );
                }
            }
        }
        let _ = write!(out, "Hardware\t: Qualcomm Technologies, Inc MSM8992\nRevision\t: {revision:0>4}\nSerial\t\t: {serial}\n");
        out
    }

    /// Contradictions and emulator tells. Empty means the profile looks like a real device.
    pub fn check(&self, arch: Arch) -> Vec<Inconsistency> {
        let mut out = Vec::new();
        let mut flag = |property: &str, reason: String| out.push(Inconsistency { property: property.to_string(), reason });

        // The ABI must match the architecture being emulated.
        let want = if arch == Arch::Arm64 { "arm64-v8a" } else { "armeabi-v7a" };
        match self.get("ro.product.cpu.abi") {
            Some(abi) if abi == want => {}
            Some(abi) => flag("ro.product.cpu.abi", format!("is {abi:?} but the emulated architecture needs {want:?}")),
            None => flag("ro.product.cpu.abi", "missing".into()),
        }
        if let Some(list) = self.get("ro.product.cpu.abilist")
            && !list.split(',').any(|a| a == want)
        {
            flag("ro.product.cpu.abilist", format!("{list:?} does not include {want:?}"));
        }
        if arch == Arch::Arm64
            && let Some(list) = self.get("ro.product.cpu.abilist64")
            && !list.split(',').any(|a| a == "arm64-v8a")
        {
            flag("ro.product.cpu.abilist64", format!("{list:?} has no 64-bit ABI on a 64-bit device"));
        }

        // sdk is a number, and sdk 23 is Android 6.0.x.
        match self.get("ro.build.version.sdk").map(|s| s.parse::<u32>()) {
            Some(Ok(sdk)) => {
                if let Some(release) = self.get("ro.build.version.release") {
                    let want_major = match sdk {
                        21 | 22 => Some("5"),
                        23 => Some("6"),
                        24 | 25 => Some("7"),
                        26 | 27 => Some("8"),
                        28 => Some("9"),
                        _ => None,
                    };
                    if let Some(major) = want_major
                        && !release.starts_with(major)
                    {
                        flag("ro.build.version.release", format!("{release:?} does not match API level {sdk}"));
                    }
                }
            }
            Some(Err(_)) => flag("ro.build.version.sdk", "is not a number".into()),
            None => flag("ro.build.version.sdk", "missing".into()),
        }

        // The fingerprint is built from the other fields; a mismatch is the classic tell.
        if let Some(fp) = self.get("ro.build.fingerprint") {
            let built = self.built_fingerprint();
            if let Some(built) = &built
                && fp != built
            {
                flag("ro.build.fingerprint", format!("{fp:?} does not match its fields {built:?}"));
            }
            for tell in ["generic", "sdk_gphone", "emulator", "vbox", "test-keys"] {
                if fp.contains(tell) {
                    flag("ro.build.fingerprint", format!("contains {tell:?}, a tell"));
                }
            }
        }

        // Release build, not a debug or emulator image.
        for (name, want, tell) in [
            ("ro.build.tags", "release-keys", "test-keys"),
            ("ro.build.type", "user", "eng"),
            ("ro.secure", "1", "0"),
            ("ro.debuggable", "0", "1"),
        ] {
            if let Some(v) = self.get(name)
                && v == tell
            {
                flag(name, format!("is {v:?}; a real retail device has {want:?}"));
            }
        }

        // Model, device and hardware must not name an emulator.
        for name in ["ro.product.model", "ro.product.device", "ro.product.name", "ro.hardware", "ro.board.platform"] {
            match self.get(name) {
                Some("") => flag(name, "is empty".into()),
                Some(v) => {
                    let low = v.to_ascii_lowercase();
                    for tell in ["generic", "emulator", "goldfish", "ranchu", "vbox", "unknown", "sdk"] {
                        if low.contains(tell) {
                            flag(name, format!("is {v:?}, which contains the emulator tell {tell:?}"));
                        }
                    }
                }
                None => flag(name, "missing".into()),
            }
        }

        // ro.hardware and ro.product.board should agree with the platform family.
        if let (Some(hw), Some(board)) = (self.get("ro.hardware"), self.get("ro.product.board"))
            && hw != board
            && !matches!((hw, board), ("qcom", _) | (_, "qcom"))
        {
            flag("ro.hardware", format!("{hw:?} and ro.product.board {board:?} name different boards"));
        }

        out
    }

    /// The fingerprint these fields build to, or None if a field is missing.
    fn built_fingerprint(&self) -> Option<String> {
        Some(format!(
            "{}/{}/{}:{}/{}/{}:{}/{}",
            self.get("ro.product.brand")?,
            self.get("ro.product.name")?,
            self.get("ro.product.device")?,
            self.get("ro.build.version.release")?,
            self.get("ro.build.id")?,
            self.get("ro.build.version.incremental")?,
            self.get("ro.build.type")?,
            self.get("ro.build.tags")?,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nexus5x_is_internally_consistent() {
        for arch in [Arch::Arm64, Arch::Arm32] {
            let p = Profile::nexus5x(arch);
            assert_eq!(p.check(arch), vec![], "{arch:?}");
            assert_eq!(p.get("ro.product.cpu.abi"), Some(if arch == Arch::Arm64 { "arm64-v8a" } else { "armeabi-v7a" }));
            assert!(p.getprop().contains("[ro.build.id]: [MDA89E]"));
            assert!(p.cpuinfo(arch).contains("Hardware\t: Qualcomm"));
            assert_eq!(p.cpuinfo(arch).matches("processor\t:").count(), 6);
        }
    }

    #[test]
    fn check_catches_tells_and_contradictions() {
        let mut p = Profile::nexus5x(Arch::Arm64);
        // Wrong ABI for the architecture.
        assert!(p.check(Arch::Arm32).iter().any(|i| i.property == "ro.product.cpu.abi"));

        p.set("ro.build.id", "QQ1A.200000");
        let found = p.check(Arch::Arm64);
        assert!(found.iter().any(|i| i.property == "ro.build.fingerprint"), "{found:?}");

        let mut emu = Profile::nexus5x(Arch::Arm64);
        emu.set("ro.product.model", "Android SDK built for arm64");
        emu.set("ro.build.tags", "test-keys");
        emu.set("ro.debuggable", "1");
        let found = emu.check(Arch::Arm64);
        for property in ["ro.product.model", "ro.build.tags", "ro.debuggable"] {
            assert!(found.iter().any(|i| i.property == property), "missing {property} in {found:?}");
        }
    }
}
