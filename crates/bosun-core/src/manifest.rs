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
    /// Candidate backends in preference order (§5.3). The resolver picks the
    /// first that is registered; an explicit flag/env/config choice still wins
    /// over this list, and a single named backend is never overridden. Written
    /// as a scalar or a sequence in the manifest; empty means "inherit/any".
    pub backends: Vec<String>,
    /// The backend-native package id, where it differs from the name. Shared
    /// by every candidate backend; one backend's own id goes in its `ext`
    /// section under [`EXT_PACKAGE`], which wins over this.
    pub package: Option<String>,
    /// A per-selector spec override.
    pub spec: Option<String>,
    /// Backend-specific settings, by backend id then key (`ext:
    /// {github-release: {asset: …}}`). Carried verbatim: each backend declares
    /// the keys it reads and interprets their values, so this crate stays
    /// backend-agnostic. The one exception is [`EXT_PACKAGE`], which every
    /// backend accepts and bosun reads itself.
    pub ext: Ext,
}

/// The `ext` key naming one backend's own package id (`ext: {winget:
/// {package: sharkdp.fd}}`), so a fallback list can name each candidate's
/// package. bosun's key, not the backend's: it becomes the identity's name,
/// never a qualifier.
pub const EXT_PACKAGE: &str = "package";

impl InstallBlock {
    /// The package id to use with `backend`: its own `ext` package, else the
    /// shared [`InstallBlock::package`]. `None` means the logical name.
    pub fn package_for(&self, backend: &str) -> Option<&str> {
        self.ext
            .get(backend)
            .and_then(|settings| settings.get(EXT_PACKAGE))
            .or(self.package.as_ref())
            .map(String::as_str)
    }
}

/// Backend-specific install settings: backend id → key → value.
pub type Ext = BTreeMap<String, BTreeMap<String, String>>;

#[cfg(test)]
mod tests {
    use super::*;

    fn block(package: Option<&str>, ext: &[(&str, &str, &str)]) -> InstallBlock {
        let mut b = InstallBlock {
            package: package.map(str::to_string),
            ..Default::default()
        };
        for (backend, key, value) in ext {
            b.ext
                .entry(backend.to_string())
                .or_default()
                .insert(key.to_string(), value.to_string());
        }
        b
    }

    #[test]
    fn package_for_a_backend_with_its_own_package_is_that_package() {
        let b = block(Some("sharkdp/fd"), &[("winget", "package", "sharkdp.fd")]);
        assert_eq!(b.package_for("winget"), Some("sharkdp.fd"));
    }

    #[test]
    fn package_for_any_other_backend_is_the_shared_package() {
        let b = block(Some("sharkdp/fd"), &[("winget", "package", "sharkdp.fd")]);
        assert_eq!(b.package_for("github-release"), Some("sharkdp/fd"));
    }

    #[test]
    fn package_for_ignores_a_backends_other_settings() {
        let b = block(None, &[("github-release", "asset", "fd.tar.gz")]);
        assert_eq!(b.package_for("github-release"), None);
    }
}
