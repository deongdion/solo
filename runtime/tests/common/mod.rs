//! Shared by the integration tests: `TEST_BACKEND=unicorn` (with `--features unicorn`) runs them
//! on unicorn instead of dynarmic.

#![allow(dead_code)]

use runtime::BackendKind;

pub fn backend() -> BackendKind {
    match std::env::var("TEST_BACKEND").as_deref() {
        #[cfg(feature = "unicorn")]
        Ok("unicorn") => BackendKind::Unicorn,
        Ok("dynarmic") | Err(_) => BackendKind::Dynarmic,
        Ok(other) => panic!("TEST_BACKEND={other} is not available in this build"),
    }
}
