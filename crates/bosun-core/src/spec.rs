//! A manifest entry's version spec and the one selection rule over it (§5.6,
//! §7).
//!
//! A spec is a vers set (§5.6): a bare version is a one-version set, so there
//! is no separate "exact" path. Selection is a single rule — **the highest
//! available version in the set** — with one guard: a pre-release is never
//! selected unless the spec itself names a pre-release (a constraint version
//! is a pre-release, or the spec is a bare pre-release version).
//!
//! There is no policy and no deviation: vers ranges subsume the old
//! exact/range/tolerant distinction.

use crate::errmsg;
use crate::vers::{VersError, VersRange};
use crate::version::{Scheme, Version};

/// A version spec: the written string plus its parsed vers set. The vers type
/// (and thus the §6 comparison scheme) comes from the written `vers:<type>/`
/// prefix, or from the backend's type supplied at construction.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Spec {
    raw: String,
    range: VersRange,
}

/// Why a spec string could not be understood.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum SpecError {
    /// The spec was not a valid vers range (or bare version).
    #[error(transparent)]
    Vers(#[from] VersError),
}

/// Why selection could not choose a version.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum SelectError {
    /// No available version is in the spec's set.
    #[error("{kind}: {spec} (available: {available})", kind = errmsg::SELECT_UNSATISFIABLE)]
    Unsatisfiable {
        /// The spec, in canonical vers form.
        spec: String,
        /// The available versions that were considered.
        available: String,
    },
}

impl Spec {
    /// Build from a written spec and the backend's vers type. A full
    /// `vers:<type>/…` carries its own type; a bare version (`1.7.1`) or
    /// prefix-less constraints (`>=2.40|<3`) are read under `ty`.
    pub fn new(raw: impl Into<String>, ty: &str) -> Result<Spec, SpecError> {
        let raw = raw.into();
        let canonical = if raw.starts_with("vers:") {
            raw.clone()
        } else {
            format!("vers:{ty}/{raw}")
        };
        Ok(Spec {
            range: VersRange::parse(&canonical)?,
            raw,
        })
    }

    /// The spec string exactly as written.
    pub fn as_str(&self) -> &str {
        &self.raw
    }

    /// The canonical vers form (what the lockfile stores).
    pub fn canonical(&self) -> String {
        self.range.to_string()
    }

    /// The §6 scheme versions in this spec compare under.
    pub fn scheme(&self) -> Scheme {
        self.range.scheme()
    }

    /// The parsed vers set.
    pub fn range(&self) -> &VersRange {
        &self.range
    }

    /// Select the highest available version in the set (§5.6), skipping
    /// pre-releases unless the spec names one. Errors when nothing is in the
    /// set, naming the spec and what was available.
    pub fn select(&self, available: &[Version]) -> Result<Version, SelectError> {
        let allow_pre = self.range.mentions_prerelease();
        available
            .iter()
            .filter(|v| self.range.contains(v))
            .filter(|v| allow_pre || !v.is_prerelease())
            .max()
            .cloned()
            .ok_or_else(|| SelectError::Unsatisfiable {
                spec: self.canonical(),
                available: available
                    .iter()
                    .map(Version::as_str)
                    .collect::<Vec<_>>()
                    .join(", "),
            })
    }
}

/// A manifest entry's resolvable intent: a logical name (§5.4) and its spec.
/// Per-target backend/package selection is layered on in later slices.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    /// bosun's own logical name for the tool.
    pub name: String,
    /// The version spec.
    pub spec: Spec,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vers(scheme: Scheme, raw: &[&str]) -> Vec<Version> {
        raw.iter()
            .map(|s| Version::parse(*s, scheme).unwrap())
            .collect()
    }

    #[test]
    fn bare_version_selects_that_version() {
        let spec = Spec::new("1.7.1-3", "deb").unwrap();
        let available = vers(Scheme::Debian, &["1.7.0", "1.7.1-3", "1.8.0"]);
        assert_eq!(spec.select(&available).unwrap().as_str(), "1.7.1-3");
    }

    #[test]
    fn bare_version_absent_is_unsatisfiable() {
        let spec = Spec::new("1.7.1-3", "deb").unwrap();
        let available = vers(Scheme::Debian, &["1.7.0", "1.8.0"]);
        assert!(matches!(
            spec.select(&available),
            Err(SelectError::Unsatisfiable { .. })
        ));
    }

    #[test]
    fn range_selects_highest_in_set() {
        let spec = Spec::new(">=1.2.0|<2.0.0", "cargo").unwrap();
        let available = vers(Scheme::Semver, &["1.2.0", "1.5.0", "1.9.3", "2.0.0"]);
        assert_eq!(spec.select(&available).unwrap().as_str(), "1.9.3");
    }

    #[test]
    fn deb_tolerant_style_range_rounds_up_within_bound() {
        // The old "tolerant" dogfood case, now just a vers range: 1.7.1 or the
        // next Debian revision of 1.7.1. Selection is highest-in-set.
        let spec = Spec::new(">=1.7.1|<1.7.2~", "deb").unwrap();
        let available = vers(
            Scheme::Debian,
            &["1.7.0-9", "1.7.1-1", "1.7.1-3", "1.7.2-1"],
        );
        assert_eq!(spec.select(&available).unwrap().as_str(), "1.7.1-3");
    }

    #[test]
    fn unsatisfiable_when_nothing_in_set() {
        let spec = Spec::new(">=3.0.0", "cargo").unwrap();
        let available = vers(Scheme::Semver, &["1.0.0", "2.5.0"]);
        assert!(matches!(
            spec.select(&available),
            Err(SelectError::Unsatisfiable { .. })
        ));
    }

    #[test]
    fn prerelease_not_selected_unless_spec_names_one() {
        let available = vers(Scheme::Semver, &["1.0.0", "1.1.0-rc1"]);
        // Plain range excludes the pre-release even though it is higher.
        let plain = Spec::new(">=1.0.0", "cargo").unwrap();
        assert_eq!(plain.select(&available).unwrap().as_str(), "1.0.0");
        // A spec naming a pre-release admits pre-releases.
        let named = Spec::new(">=1.1.0-rc1", "cargo").unwrap();
        assert_eq!(named.select(&available).unwrap().as_str(), "1.1.0-rc1");
    }

    #[test]
    fn full_vers_spec_carries_its_own_type() {
        let spec = Spec::new("vers:deb/>=1.7.1|<1.7.2~", "cargo").unwrap();
        assert_eq!(spec.scheme(), Scheme::Debian); // the string's type wins
        assert_eq!(spec.canonical(), "vers:deb/>=1.7.1|<1.7.2~");
    }

    #[test]
    fn canonical_is_the_vers_form() {
        assert_eq!(
            Spec::new("1.0.0", "npm").unwrap().canonical(),
            "vers:npm/1.0.0"
        );
    }

    #[test]
    fn as_str_is_the_written_spec() {
        assert_eq!(Spec::new(">=2.40|<3", "deb").unwrap().as_str(), ">=2.40|<3");
    }

    #[test]
    fn invalid_spec_is_rejected() {
        assert!(matches!(
            Spec::new("=1.0.0", "npm"),
            Err(SpecError::Vers(_))
        ));
    }
}
