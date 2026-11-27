//! Guest memory (pages, permissions, dirty tracking), registers and shared error types.

/// Errors raised by guest memory operations.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("address {0:#x} is not mapped")]
    Unmapped(u64),
}
