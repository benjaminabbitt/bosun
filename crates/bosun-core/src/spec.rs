//! A manifest entry's version spec and the one selection rule over it (§5.6,
//! §7).
//!
//! A spec is a vers set (§5.6): a bare version is a one-version set, so there
//! is no separate "exact" path. Selection is a single rule — **the highest
//! available version in the set** — with one guard: a pre-release is never
//! selected unless the spec itself names a pre-release (a constraint version
//! is a pre-release, or the spec is a bare pre-release version). Only the
//! language schemes have pre-releases; Debian, RPM and Alpine versions have
//! none (see [`Version::is_prerelease`]).
//!
//! There is no policy and no deviation: vers ranges subsume the old
//! exact/range/tolerant distinction.

use crate::errmsg;
use crate::vers::{VersError, VersOp, VersRange};
use crate::version::{Scheme, Version};
use crate::want::Want;

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
    /// A constraint version does not parse in the spec's scheme (§6): a
    /// typo like `>=0.6` under SemVer that would otherwise match nothing.
    #[error("{kind}: {version:?} is not a valid {scheme} version", kind = errmsg::SPEC_BAD_VERSION)]
    BadVersion {
        /// The offending version text.
        version: String,
        /// The scheme it failed to parse under.
        scheme: &'static str,
    },
}

/// A human name for a scheme, for diagnostics.
fn scheme_name(scheme: Scheme) -> &'static str {
    match scheme {
        Scheme::Debian => "Debian",
        Scheme::Semver => "SemVer",
        Scheme::Generic => "generic",
        Scheme::Rpm => "RPM",
        Scheme::Alpine => "apk",
        Scheme::Pep440 => "PEP 440",
    }
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
        let range = VersRange::parse(&canonical)?;
        // Every constraint version must parse in the scheme (§6): a version
        // that does not would silently match nothing at selection time.
        let scheme = range.scheme();
        for c in range.constraints() {
            if c.op != VersOp::Star && Version::parse(&c.version, scheme).is_err() {
                return Err(SpecError::BadVersion {
                    version: c.version.clone(),
                    scheme: scheme_name(scheme),
                });
            }
        }
        Ok(Spec { range, raw })
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
        Want::of(self.range.clone())
            .choose(available, &[])
            .ok_or_else(|| {
                // Newest first, so the diagnostic reads naturally.
                let mut sorted: Vec<&Version> = available.iter().collect();
                sorted.sort_by(|a, b| b.cmp(a));
                SelectError::Unsatisfiable {
                    spec: self.canonical(),
                    available: sorted
                        .iter()
                        .map(|v| v.as_str())
                        .collect::<Vec<_>>()
                        .join(", "),
                }
            })
    }
}

/// The spec `bosun add` writes when none is given, from the newest version
/// (§4.2): any later version that shouldn't break it.
///
/// - SemVer: the caret range, `>=X.Y.Z|<(X+1).0.0`; below 1.0 the next minor
///   (`0.9.1` → `<0.10.0`) and below 0.1 the next patch is the bound.
///   Build metadata is dropped; a pre-release stays as the lower bound.
/// - Debian and RPM: any revision (release) of the same upstream version,
///   `>=U|<U'~` where `U'` bumps the last number of `U`'s leading numeric
///   part (`1.7.1-3` → `>=1.7.1|<1.7.2~`); an epoch is kept on both bounds.
/// - PEP 440: `>=V`, the lower bound `uv add` writes; the epoch is kept and a
///   local label dropped.
/// - Otherwise, or when the version has no numeric part to bump: the exact
///   version.
pub fn default_spec(latest: &Version) -> String {
    let exact = latest.as_str().to_string();
    match latest.scheme() {
        Scheme::Semver => match semver::Version::parse(latest.as_str()) {
            Ok(v) => {
                let floor = if v.pre.is_empty() {
                    format!("{}.{}.{}", v.major, v.minor, v.patch)
                } else {
                    format!("{}.{}.{}-{}", v.major, v.minor, v.patch, v.pre)
                };
                let ceiling = match (v.major, v.minor) {
                    (0, 0) => format!("0.0.{}", v.patch + 1),
                    (0, minor) => format!("0.{}.0", minor + 1),
                    (major, _) => format!("{}.0.0", major + 1),
                };
                format!(">={floor}|<{ceiling}")
            }
            Err(_) => exact,
        },
        Scheme::Debian | Scheme::Rpm => epoch_release_default(latest.as_str()).unwrap_or(exact),
        Scheme::Alpine => alpine_default(latest.as_str()).unwrap_or(exact),
        Scheme::Pep440 => pep440_default(latest.as_str()).unwrap_or(exact),
        Scheme::Generic => exact,
    }
}

/// `>=V` for a PEP 440 version, as `uv add` writes it (see [`default_spec`]).
fn pep440_default(raw: &str) -> Option<String> {
    let v: pep440_rs::Version = raw.parse().ok()?;
    Some(format!(">={}", v.without_local()))
}

/// `>=U|<U'~` for a Debian or RPM version (see [`default_spec`]), or `None`
/// when its upstream part does not start with a number.
fn epoch_release_default(raw: &str) -> Option<String> {
    let (epoch, rest) = match raw.split_once(':') {
        Some((e, r)) => (format!("{e}:"), r),
        None => (String::new(), raw),
    };
    let upstream = rest.rsplit_once('-').map_or(rest, |(u, _)| u);
    let next = bump_leading_number(upstream)?;
    Some(format!(">={epoch}{upstream}|<{epoch}{next}~"))
}

/// `>=U|<U'_alpha` for an Alpine version (see [`default_spec`]), or `None`
/// when it does not start with a number.
fn alpine_default(raw: &str) -> Option<String> {
    let upstream = raw.rsplit_once("-r").map_or(raw, |(u, _)| u);
    let next = bump_leading_number(upstream)?;
    Some(format!(">={upstream}|<{next}_alpha"))
}

/// The leading dotted number of `upstream` with its last component bumped
/// (`1.7.1+x` → `1.7.2`), or `None` when it does not start with a number.
fn bump_leading_number(upstream: &str) -> Option<String> {
    let numeric_len = upstream
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(upstream.len());
    let numeric = upstream[..numeric_len].trim_end_matches('.');
    let (head, last) = match numeric.rsplit_once('.') {
        Some((h, l)) => (format!("{h}."), l),
        None => (String::new(), numeric),
    };
    let bumped = last.parse::<u64>().ok()? + 1;
    Some(format!("{head}{bumped}"))
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
    fn an_os_spec_selects_the_newest_build_the_repositories_carry() {
        // No pre-release guard for OS schemes: apt, dnf and apk install a
        // `~rc1` or `_rc1` build when it is the newest on offer.
        let deb = vers(Scheme::Debian, &["2.10-3", "2.11~rc1-1"]);
        assert_eq!(
            Spec::new("*", "deb")
                .unwrap()
                .select(&deb)
                .unwrap()
                .as_str(),
            "2.11~rc1-1"
        );
        let rpm = vers(Scheme::Rpm, &["2.0-1.fc41", "1:1.7~rc1-1.fc41"]);
        assert_eq!(
            Spec::new(">=1.6", "rpm")
                .unwrap()
                .select(&rpm)
                .unwrap()
                .as_str(),
            "1:1.7~rc1-1.fc41"
        );
        let apk = vers(Scheme::Alpine, &["1.7.1-r10", "1.8.0_rc1-r0"]);
        assert_eq!(
            Spec::new(">=1.7", "apk")
                .unwrap()
                .select(&apk)
                .unwrap()
                .as_str(),
            "1.8.0_rc1-r0"
        );
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

    #[test]
    fn a_constraint_version_invalid_in_its_scheme_is_a_spec_error() {
        // `0.6` is not SemVer; it must error at parse, not match nothing later.
        match Spec::new(">=0.6", "semver").unwrap_err() {
            SpecError::BadVersion { version, scheme } => {
                assert_eq!(version, "0.6");
                assert_eq!(scheme, "SemVer");
            }
            other => panic!("expected bad version, got {other:?}"),
        }
        // A bare invalid version is caught too.
        assert!(matches!(
            Spec::new("0.6", "cargo"),
            Err(SpecError::BadVersion { .. })
        ));
        // The full form stays valid.
        assert!(Spec::new(">=0.6.0", "semver").is_ok());
    }

    fn default_of(raw: &str, scheme: Scheme) -> String {
        default_spec(&Version::parse(raw, scheme).unwrap())
    }

    #[test]
    fn default_spec_for_semver_is_the_caret_range() {
        assert_eq!(default_of("2.0.2", Scheme::Semver), ">=2.0.2|<3.0.0");
        assert_eq!(default_of("10.5.0", Scheme::Semver), ">=10.5.0|<11.0.0");
    }

    #[test]
    fn default_spec_below_one_keeps_the_minor() {
        assert_eq!(default_of("0.9.1", Scheme::Semver), ">=0.9.1|<0.10.0");
        assert_eq!(default_of("0.0.3", Scheme::Semver), ">=0.0.3|<0.0.4");
    }

    #[test]
    fn default_spec_drops_build_metadata_but_keeps_a_prerelease() {
        assert_eq!(
            default_of("1.2.3+build.5", Scheme::Semver),
            ">=1.2.3|<2.0.0"
        );
        assert_eq!(
            default_of("1.2.3-rc.1", Scheme::Semver),
            ">=1.2.3-rc.1|<2.0.0"
        );
    }

    #[test]
    fn default_spec_for_debian_is_any_revision_of_the_upstream_version() {
        assert_eq!(default_of("1.7.1-3", Scheme::Debian), ">=1.7.1|<1.7.2~");
        assert_eq!(
            default_of("1.7.1-3+deb13u1", Scheme::Debian),
            ">=1.7.1|<1.7.2~"
        );
        assert_eq!(default_of("1.40.0-1", Scheme::Debian), ">=1.40.0|<1.40.1~");
        assert_eq!(default_of("1:2.3-4", Scheme::Debian), ">=1:2.3|<1:2.4~");
        assert_eq!(
            default_of("9.0.0~rc1-1", Scheme::Debian),
            ">=9.0.0~rc1|<9.0.1~"
        );
        assert_eq!(default_of("2.3", Scheme::Debian), ">=2.3|<2.4~");
    }

    #[test]
    fn default_spec_for_rpm_is_any_release_of_the_version() {
        assert_eq!(default_of("1.7.1-8.fc41", Scheme::Rpm), ">=1.7.1|<1.7.2~");
        assert_eq!(
            default_of("2:4.15.1-12.fc41", Scheme::Rpm),
            ">=2:4.15.1|<2:4.15.2~"
        );
        assert_eq!(default_of("2026e-1.el9_8", Scheme::Rpm), ">=2026e|<2027~");
        assert_eq!(default_of("beta-1", Scheme::Rpm), "beta-1");
    }

    #[test]
    fn scheme_names_read_as_their_ecosystem() {
        assert_eq!(scheme_name(Scheme::Debian), "Debian");
        assert_eq!(scheme_name(Scheme::Semver), "SemVer");
        assert_eq!(scheme_name(Scheme::Generic), "generic");
        assert_eq!(scheme_name(Scheme::Rpm), "RPM");
        assert_eq!(scheme_name(Scheme::Alpine), "apk");
        assert_eq!(scheme_name(Scheme::Pep440), "PEP 440");
    }

    #[test]
    fn default_spec_for_pep440_is_the_lower_bound_uv_add_writes() {
        let p = |raw| default_of(raw, Scheme::Pep440);
        assert_eq!(p("3.2.3"), ">=3.2.3");
        assert_eq!(p("0.6.9"), ">=0.6.9");
        assert_eq!(p("2024.10"), ">=2024.10");
        assert_eq!(p("1!2.0"), ">=1!2.0");
        assert_eq!(p("24.1a1"), ">=24.1a1");
        assert_eq!(p("1.0.post2"), ">=1.0.post2");
        assert_eq!(p("2.1.0+cpu"), ">=2.1.0");
    }

    #[test]
    fn default_spec_for_alpine_is_any_revision_of_the_upstream_version() {
        assert_eq!(
            default_of("1.7.1-r3", Scheme::Alpine),
            ">=1.7.1|<1.7.2_alpha"
        );
        assert_eq!(
            default_of("1.40.0", Scheme::Alpine),
            ">=1.40.0|<1.40.1_alpha"
        );
        assert_eq!(
            default_of("2.4_p1-r1", Scheme::Alpine),
            ">=2.4_p1|<2.5_alpha"
        );
        assert_eq!(
            default_of("20260909-r0", Scheme::Alpine),
            ">=20260909|<20260910_alpha"
        );
        assert_eq!(
            default_of("1.8.0_rc1-r0", Scheme::Alpine),
            ">=1.8.0_rc1|<1.8.1_alpha"
        );
    }

    #[test]
    fn a_bad_apk_constraint_is_named_as_apk() {
        match Spec::new(">=1.0-1", "apk").unwrap_err() {
            SpecError::BadVersion { scheme, .. } => assert_eq!(scheme, "apk"),
            other => panic!("expected bad version, got {other:?}"),
        }
    }

    #[test]
    fn default_spec_is_the_exact_version_when_no_range_fits() {
        assert_eq!(default_of("beta-1", Scheme::Debian), "beta-1");
        assert_eq!(default_of("2024.10.1", Scheme::Generic), "2024.10.1");
    }

    #[test]
    fn default_spec_parses_back_and_holds_the_version() {
        for (raw, scheme, ty) in [
            ("2.0.2", Scheme::Semver, "semver"),
            ("0.9.1", Scheme::Semver, "semver"),
            ("1.2.3-rc.1", Scheme::Semver, "semver"),
            ("1.7.1-3", Scheme::Debian, "deb"),
            ("1:2.3-4", Scheme::Debian, "deb"),
            ("beta-1", Scheme::Debian, "deb"),
            ("1.7.1-8.fc41", Scheme::Rpm, "rpm"),
            ("2:4.15.1-12.fc41", Scheme::Rpm, "rpm"),
            ("1.7.1-r3", Scheme::Alpine, "apk"),
            ("1.8.0_rc1-r0", Scheme::Alpine, "apk"),
            ("0.6.9", Scheme::Pep440, "pypi"),
            ("24.1a1", Scheme::Pep440, "pypi"),
            ("1!2.0.dev3", Scheme::Pep440, "pypi"),
            ("2.1.0+cpu", Scheme::Pep440, "pypi"),
        ] {
            let v = Version::parse(raw, scheme).unwrap();
            let spec = Spec::new(default_spec(&v), ty).unwrap();
            assert_eq!(spec.select(std::slice::from_ref(&v)).unwrap(), v, "{raw}");
        }
    }

    #[test]
    fn unsatisfiable_lists_available_newest_first() {
        let spec = Spec::new(">=99.0.0", "cargo").unwrap();
        let available = vers(Scheme::Semver, &["0.3.11", "0.4.0", "0.3.12"]);
        match spec.select(&available).unwrap_err() {
            SelectError::Unsatisfiable { available, .. } => {
                assert_eq!(available, "0.4.0, 0.3.12, 0.3.11");
            }
        }
    }
}
