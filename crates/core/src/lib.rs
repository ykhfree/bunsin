//! Pure harness logic. This crate does no IO: callers inject time, IDs and data.

pub mod approval;
pub mod context;
pub mod event;
pub mod llm;
pub mod marker;
pub mod policy;
pub mod ports;
pub mod proposal;
pub mod tool;

use sha2::{Digest, Sha256};

/// Returns the lowercase hex SHA-256 of `bytes`.
pub fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}
