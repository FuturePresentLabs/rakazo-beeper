pub mod config;
pub mod fleet;
pub mod login;
pub mod markdown;
pub mod matrix;
pub mod media;
pub mod orch;
pub mod rakazo;
pub mod rich;
pub mod service;
pub mod store;
pub mod transport;

use sha2::{Digest, Sha256};

/// Length-delimited hashing keeps tenant and remote identifiers unambiguous.
pub fn stable_id(parts: &[&str]) -> String {
    let mut hash = Sha256::new();
    for part in parts {
        hash.update((part.len() as u64).to_be_bytes());
        hash.update(part.as_bytes());
    }
    hex::encode(hash.finalize())
}
pub mod avatar;
