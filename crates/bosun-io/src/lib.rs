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

use bosun_core::vers::scheme_for_type;
use bosun_core::Scheme;

/// How a lock/state leaf picks its version's §6 comparison scheme: from the
/// `backend` it records, mapped to that backend's declared vers type (§6) —
/// never the purl type (github's purl type is `github` but its versions compare
/// as semver). The CLI builds this from the registered backends. A backend not
/// listed falls back to [`Scheme::Generic`].
pub type BackendSchemes = std::collections::BTreeMap<String, Scheme>;

/// The §6 scheme for a leaf recorded under `backend`, by that backend's vers
/// type; [`Scheme::Generic`] when the backend is not registered this run.
pub(crate) fn scheme_of(schemes: &BackendSchemes, backend: &str) -> Scheme {
    schemes.get(backend).copied().unwrap_or(Scheme::Generic)
}

/// Build a [`BackendSchemes`] from `(backend id, vers type)` pairs (the vers
/// type decides the scheme, §6), for callers assembling it from a registry.
pub fn schemes_from<'a>(backends: impl IntoIterator<Item = (&'a str, &'a str)>) -> BackendSchemes {
    backends
        .into_iter()
        .map(|(id, vers_type)| (id.to_string(), scheme_for_type(vers_type)))
        .collect()
}
