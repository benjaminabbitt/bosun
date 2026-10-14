//! Versions and their ordering (§6).
//!
//! Version strings do not normalize across ecosystems: Debian
//! `epoch:upstream-revision`, SemVer, and the long tail all have different
//! grammars *and* ordering rules. So a [`Version`] carries its [`Scheme`] and
//! orders only within it, delegating to a reference implementation where one
//! exists:
//!
//! - [`Scheme::Debian`] → `deb-version` (epoch, `~`, revision);
//! - [`Scheme::Semver`] → `semver` (pre-release precedence: `0.7.0-pre1 <
//!   0.7.0`);
//! - [`Scheme::Generic`] → bosun's own typed-[`Segment`] engine (`segment`).
//!
//! Comparing two versions of different schemes is not meaningful — it never
//! happens in practice, since every version for one package shares a scheme.
//! [`Ord`] stays total by ordering on the scheme first; [`Version::try_cmp`]
//! returns `None` on a scheme mismatch for callers that want to assert.

mod segment;

use std::cmp::Ordering;
use std::fmt;

pub use segment::Segment;

/// The version grammar a string is written in.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Scheme {
    /// Debian `epoch:upstream-revision`, ordered by `deb-version`.
    Debian,
    /// Semantic Versioning, ordered by `semver`.
    Semver,
    /// Anything else: bosun's typed-chunk engine.
    Generic,
}

impl Scheme {
    fn rank(self) -> u8 {
        match self {
            Scheme::Debian => 0,
            Scheme::Semver => 1,
            Scheme::Generic => 2,
        }
    }
}

/// Why a version string could not be parsed under its scheme.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum VersionError {
    /// The string was empty.
    Empty,
    /// A SemVer string `semver` rejected, with its message.
    Semver(String),
}

impl fmt::Display for VersionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            VersionError::Empty => write!(f, "empty version string"),
            VersionError::Semver(m) => write!(f, "invalid semver version: {m}"),
        }
    }
}

impl std::error::Error for VersionError {}

/// A version string tagged with the grammar it is read under. Keeps the
/// original text (what the backend reported) and a parsed form used only for
/// ordering.
#[derive(Clone, Debug)]
pub struct Version {
    raw: String,
    scheme: Scheme,
    parsed: Parsed,
}

#[derive(Clone, Debug)]
enum Parsed {
    Debian,
    Semver(semver::Version),
    Generic(Vec<Segment>),
}

impl Version {
    /// Parse `raw` under `scheme`. Debian and Generic accept any non-empty
    /// string (their grammars are permissive); SemVer is validated.
    pub fn parse(raw: impl Into<String>, scheme: Scheme) -> Result<Version, VersionError> {
        let raw = raw.into();
        if raw.is_empty() {
            return Err(VersionError::Empty);
        }
        let parsed = match scheme {
            Scheme::Debian => Parsed::Debian,
            Scheme::Semver => semver::Version::parse(&raw)
                .map(Parsed::Semver)
                .map_err(|e| VersionError::Semver(e.to_string()))?,
            Scheme::Generic => Parsed::Generic(segment::tokenize(&raw)),
        };
        Ok(Version {
            raw,
            scheme,
            parsed,
        })
    }

    /// The version string exactly as supplied.
    pub fn as_str(&self) -> &str {
        &self.raw
    }

    /// The scheme this version is read under.
    pub fn scheme(&self) -> Scheme {
        self.scheme
    }

    /// Compare with another version of the same scheme, or `None` if the
    /// schemes differ.
    pub fn try_cmp(&self, other: &Version) -> Option<Ordering> {
        if self.scheme != other.scheme {
            return None;
        }
        Some(match (&self.parsed, &other.parsed) {
            (Parsed::Debian, Parsed::Debian) => {
                deb_version::compare_versions(&self.raw, &other.raw)
            }
            (Parsed::Semver(a), Parsed::Semver(b)) => a.cmp(b),
            (Parsed::Generic(a), Parsed::Generic(b)) => segment::compare(a, b),
            // Unreachable: equal scheme implies equal Parsed variant.
            _ => Ordering::Equal,
        })
    }
}

impl PartialEq for Version {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for Version {}

impl Ord for Version {
    fn cmp(&self, other: &Self) -> Ordering {
        match self.try_cmp(other) {
            Some(o) => o,
            // Different schemes: order by scheme so `Ord` stays total.
            None => self.scheme.rank().cmp(&other.scheme.rank()),
        }
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.raw)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn deb(s: &str) -> Version {
        Version::parse(s, Scheme::Debian).unwrap()
    }
    fn sem(s: &str) -> Version {
        Version::parse(s, Scheme::Semver).unwrap()
    }
    fn gen(s: &str) -> Version {
        Version::parse(s, Scheme::Generic).unwrap()
    }

    #[test]
    fn empty_is_rejected_every_scheme() {
        assert_eq!(Version::parse("", Scheme::Debian), Err(VersionError::Empty));
        assert_eq!(Version::parse("", Scheme::Semver), Err(VersionError::Empty));
        assert_eq!(
            Version::parse("", Scheme::Generic),
            Err(VersionError::Empty)
        );
    }

    #[test]
    fn invalid_semver_is_rejected() {
        assert!(matches!(
            Version::parse("not.a.version", Scheme::Semver),
            Err(VersionError::Semver(_))
        ));
    }

    #[test]
    fn debian_handles_epoch_tilde_revision() {
        // epoch dominates upstream
        assert!(deb("1:1.0") > deb("2.0"));
        // tilde is a pre-release, below the release
        assert!(deb("1.0~rc1") < deb("1.0"));
        // revision breaks ties
        assert!(deb("1.0-2") > deb("1.0-1"));
        // numeric upstream compares numerically
        assert!(deb("1.10") > deb("1.9"));
    }

    #[test]
    fn debian_apt_style_revisions_order() {
        assert!(deb("1.7.1-3ubuntu0.1") > deb("1.7.1-3"));
    }

    #[test]
    fn semver_prerelease_is_below_release() {
        // The dogfood case: the ctxloom family is at 0.7.0-pre1.
        assert!(sem("0.7.0-pre1") < sem("0.7.0"));
        assert!(sem("0.7.0-pre1") < sem("0.7.0-pre2"));
        assert!(sem("1.2.3") > sem("1.2.2"));
    }

    #[test]
    fn generic_orders_by_segments() {
        assert!(gen("1.10") > gen("1.9"));
        assert!(gen("1.0~rc1") < gen("1.0"));
        assert!(gen("1.0.1") > gen("1.0"));
    }

    #[test]
    fn equal_versions_compare_equal_and_eq() {
        assert_eq!(deb("1.0-1"), deb("1.0-1"));
        assert_eq!(sem("1.0.0"), sem("1.0.0"));
        assert_eq!(gen("1.0"), gen("1.0"));
    }

    #[test]
    fn unequal_versions_are_not_eq() {
        // `<`/`>` go through `cmp`; this pins `PartialEq::eq` itself.
        assert_ne!(deb("1.0"), deb("2.0"));
        assert_ne!(sem("1.0.0"), sem("1.0.1"));
        assert_ne!(gen("1.0"), gen("1.0.1"));
    }

    #[test]
    fn version_error_messages_are_rendered() {
        assert_eq!(VersionError::Empty.to_string(), "empty version string");
        assert_eq!(
            VersionError::Semver("unexpected end of input".into()).to_string(),
            "invalid semver version: unexpected end of input"
        );
    }

    #[test]
    fn try_cmp_rejects_scheme_mismatch() {
        assert_eq!(deb("1.0").try_cmp(&sem("1.0.0")), None);
    }

    #[test]
    fn ord_is_total_across_schemes_by_scheme_rank() {
        // Not semantically meaningful, but total and deterministic.
        assert!(deb("9.9") < sem("0.0.1"));
        assert!(sem("9.9.9") < gen("0"));
    }

    #[test]
    fn roundtrips_display_and_as_str() {
        assert_eq!(deb("1:2.3-4").as_str(), "1:2.3-4");
        assert_eq!(sem("1.2.3-rc.1").to_string(), "1.2.3-rc.1");
    }

    #[test]
    fn oracle_generic_agrees_with_deb_version_on_shared_cases() {
        // Where both engines define the ordering identically, they agree.
        for (a, b) in [
            ("1.10", "1.9"),
            ("1.0", "1.0.1"),
            ("1.0~rc1", "1.0"),
            ("2.0", "1.0"),
            ("1.2.3", "1.2.3"),
        ] {
            assert_eq!(
                gen(a).cmp(&gen(b)),
                deb_version::compare_versions(a, b),
                "disagreement on {a} vs {b}"
            );
        }
    }
}
