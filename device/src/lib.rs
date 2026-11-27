//! Device profile: system properties, packages, certificates, rootfs per API level, APK metadata,
//! and the clock every time source of the emulated process reads.

pub mod clock;
pub mod profile;

pub use clock::{Clock, ClockMode};
pub use profile::{Inconsistency, Profile};
