//! The parsed manifest model (§4.4, §5.4). Pure data: the loader in bosun-io
//! walks annals-yaml into this; the resolver (bosun-resolve) reads it.
//!
//! Specs stay as the **written strings** (bare version, prefix-less vers, or a
//! full `vers:` range); they are parsed into [`crate::Spec`] at resolve time,
//! when the backend — and so the vers type (§5.6) — is known. There is no
//! `policy` field: a spec is a vers set.

use std::collections::BTreeMap;

use crate::platform::{Selector, Target};

/// A whole manifest.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Manifest {
    /// Format version (`1`).
    pub version: u32,
    /// The targets this manifest declares (§4.5).
    pub targets: Vec<Target>,
    /// `defaults.install`: per-selector install defaults entries inherit from.
    pub defaults: BTreeMap<Selector, InstallBlock>,
    /// The packages, in file order.
    pub packages: Vec<ManifestEntry>,
}

/// One package entry (§5.4).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ManifestEntry {
    /// bosun's logical name for the tool.
    pub name: String,
    /// The entry-level spec (a per-selector `install` block may override it).
    pub spec: Option<String>,
    /// Previous names to migrate from (`formerly`, §5.4).
    pub formerly: Vec<String>,
    /// Per-selector install blocks (`<os>` / `<os>:<distro>`).
    pub install: BTreeMap<Selector, InstallBlock>,
}

/// How to install a package for one selector (§5.4). Fields default or inherit
/// from `defaults.install`; the package id defaults to the logical name.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct InstallBlock {
    /// The backend to use (e.g. `apt`, `cargo`).
    pub backend: Option<String>,
    /// The backend-native package id, where it differs from the name.
    pub package: Option<String>,
    /// A per-selector spec override.
    pub spec: Option<String>,
}
