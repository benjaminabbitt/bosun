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
pub mod manifest_edit;
pub mod paths;
pub mod state;

pub use config::UserConfig;
pub use error::IoError;
pub use manifest::{check_ext, load_manifest, manifest_hash, BackendExtKeys, ManifestError};
pub use state::FileStateStore;

use bosun_core::vers::scheme_for_type;
use bosun_core::Scheme;

/// How a lock/state leaf picks its version's §6 comparison scheme: from the
/// `backend` it records, mapped to that backend's declared vers type (§6) —
/// never the purl type (github's purl type is `github` but its versions compare
/// as semver). The CLI builds this from **every compiled-in backend**, not just
/// the ones present this run, so a leaf for an absent-but-known backend still
/// reads correctly. A `backend` not in the map is unknown to this build and is
/// a load error — never silently generic.
pub type BackendSchemes = std::collections::BTreeMap<String, Scheme>;

/// The §6 scheme for a leaf recorded under `backend`, by that backend's vers
/// type. A backend this build does not know is a load error naming it and
/// `source` — never [`Scheme::Generic`], which would compare its versions
/// wrongly.
pub(crate) fn scheme_of(
    schemes: &BackendSchemes,
    backend: &str,
    source: &std::path::Path,
) -> Result<Scheme, IoError> {
    schemes
        .get(backend)
        .copied()
        .ok_or_else(|| IoError::UnknownBackend {
            path: source.to_path_buf(),
            backend: backend.to_string(),
        })
}

/// Build a [`BackendSchemes`] from `(backend id, vers type)` pairs (the vers
/// type decides the scheme, §6), for callers assembling it from the compiled-in
/// backends.
pub fn schemes_from<'a>(backends: impl IntoIterator<Item = (&'a str, &'a str)>) -> BackendSchemes {
    backends
        .into_iter()
        .map(|(id, vers_type)| (id.to_string(), scheme_for_type(vers_type)))
        .collect()
}
