//! bosun I/O: read and write the lockfile (§4.5) and machine state (§4.6) as
//! byte-stable JSON, resolve the per-platform config/state paths (§4.4, §4.6),
//! and read user config through `annals-yaml` (strict YAML subset).
//!
//! The manifest loader and its parsed-model hash (§4.4, §4.5) land alongside
//! this; the domain model in bosun-core stays wire-free — the serde shapes
//! live here.

pub mod config;
pub mod errmsg;
pub mod error;
pub mod lock;
pub mod manifest;
pub mod paths;
pub mod state;

pub use config::UserConfig;
pub use error::IoError;
pub use manifest::{load_manifest, manifest_hash, ManifestError};
pub use state::FileStateStore;
