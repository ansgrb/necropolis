//! Shared domain types and configuration for Necropolis.
//!
//! Everything that more than one crate needs to agree on lives here: the L0 card shape,
//! enums mirrored by the `CHECK` constraints in `migrations/0001_init.up.sql`, and `necro.toml`.

pub mod card;
pub mod config;
pub mod types;

pub use card::{CARD_SCHEMA_VERSION, Card, CardIdea, PROMPT_VERSION};
pub use config::Config;
pub use types::*;

/// Generates a new ULID-based identifier with a readable prefix, e.g. `proj_01J...`.
pub fn new_id(prefix: &str) -> String {
    format!("{prefix}_{}", ulid::Ulid::generate())
}
