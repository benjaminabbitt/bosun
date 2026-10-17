//! The parsed manifest model (§4.4, §5.4). Pure data: the loader in bosun-io
//! walks annals-yaml into this; the resolver (bosun-resolve) reads it.
//!
//! Specs stay as the **written strings** (bare version, prefix-less vers, or a
//! full `vers:` range); they are parsed into [`crate::Spec`] at resolve time,
//! when the backend — and so the vers type (§5.6) — is known. There is no
//! `policy` field: a spec is a vers set.

use std::collections::BTreeMap;

use crate::platform::Selector;

/// A whole manifest.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Manifest {
    /// Format version (`2`).
    pub version: u32,
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
    /// The stem of the variables `render` gives this entry (`FD` for
    /// `FD_VERSION`, §8.3), when the logical name's own does not suit.
    pub var: Option<String>,
}

/// How to install a package for one selector (§5.4). Fields default or inherit
/// from `defaults.install`; the package id defaults to the logical name.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct InstallBlock {
    /// The acceptable backends, each with its options (§5.3, §5.4). A set, not
    /// an order: which one is used here is the machine's preference, never the
    /// manifest's. Empty means "inherit from `defaults.install`".
    pub backends: Backends,
    /// The package id for backends this block inherits rather than names
    /// (`windows: {package: sharkdp.fd}` over a `defaults` backend). A named
    /// backend's own [`PACKAGE_OPTION`] wins over this.
    pub package: Option<String>,
    /// A per-selector spec override.
    pub spec: Option<String>,
}

/// The acceptable backends: backend id → its options.
pub type Backends = BTreeMap<String, Options>;

/// One backend's options: [`PACKAGE_OPTION`] plus the keys that backend
/// declares (github-release's `asset`, …). Carried verbatim; each backend
/// interprets its own values, so this crate stays backend-agnostic.
pub type Options = BTreeMap<String, String>;

/// The option naming a backend's own package id (`winget: {package:
/// sharkdp.fd}`). bosun's option, not the backend's: every backend takes it,
/// and it becomes the identity's name, never a qualifier.
pub const PACKAGE_OPTION: &str = "package";

impl InstallBlock {
    /// The package id to use with `backend`: its own [`PACKAGE_OPTION`], else
    /// the block's [`InstallBlock::package`]. `None` means the logical name.
    pub fn package_for(&self, backend: &str) -> Option<&str> {
        self.backends
            .get(backend)
            .and_then(|options| options.get(PACKAGE_OPTION))
            .or(self.package.as_ref())
            .map(String::as_str)
    }
}

/// Whether `s` names a shell or Dockerfile variable (§8.3): ASCII letters,
/// digits and `_`, not starting with a digit.
pub fn is_variable_name(s: &str) -> bool {
    let mut chars = s.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn block(package: Option<&str>, options: &[(&str, &str, &str)]) -> InstallBlock {
        let mut b = InstallBlock {
            package: package.map(str::to_string),
            ..Default::default()
        };
        for (backend, key, value) in options {
            b.backends
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
    fn a_variable_name_is_letters_digits_and_underscores_not_led_by_a_digit() {
        for good in ["FD", "_", "_FD", "fd_2", "A1"] {
            assert!(is_variable_name(good), "{good}");
        }
        for bad in ["", "7ZIP", "FD-X", "FD X", "FD.X", "\u{c9}"] {
            assert!(!is_variable_name(bad), "{bad}");
        }
    }

    #[test]
    fn package_for_ignores_a_backends_other_settings() {
        let b = block(None, &[("github-release", "asset", "fd.tar.gz")]);
        assert_eq!(b.package_for("github-release"), None);
    }
}
