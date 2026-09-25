//! remuda: multi-account and session manager for coding-agent CLIs.
//!
//! Library code never reads the process environment; `main.rs` captures it once and passes
//! an [`Env`] snapshot down.

pub mod attribution;
pub mod checks;
pub mod cli;
pub mod identity;
pub mod index;
pub mod launch;
pub mod live;
pub mod paths;
pub mod pricing;
pub mod probe;
pub mod provider;
pub mod registry;
pub mod relay;
pub mod setup;
pub mod share;
pub mod stats;
pub mod text;
pub mod transcript;
pub mod tui;
pub mod usage;

use std::collections::BTreeMap;

/// Snapshot of the process environment (UTF-8 entries only).
pub type Env = BTreeMap<String, String>;
