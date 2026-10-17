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
//! - [`Scheme::Rpm`] → `rpmvercmp` over `[epoch:]version-release` (`rpm`);
//! - [`Scheme::Alpine`] → apk-tools' own ordering (`alpine`);
//! - [`Scheme::Pep440`] → `pep440_rs` (epoch, pre/post/dev releases, local
//!   labels: `1.0.dev1 < 1.0a1 < 1.0 < 1.0+local < 1.0.post1`);
//! - [`Scheme::Generic`] → bosun's own typed-[`Segment`] engine (`segment`).
//!
//! Comparing two versions of different schemes is not meaningful — it never
//! happens in practice, since every version for one package shares a scheme.
//! [`Ord`] stays total by ordering on the scheme first; [`Version::try_cmp`]
//! returns `None` on a scheme mismatch for callers that want to assert.

mod alpine;
mod rpm;
mod segment;

use std::cmp::Ordering;
use std::fmt;

use crate::errmsg;

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
    /// RPM `[epoch:]version-release`, ordered as rpm's `rpmvercmp` does.
    Rpm,
    /// Alpine `1.2.3_rc1-r0`, ordered as apk-tools orders it.
    Alpine,
    /// Python's PEP 440 (`1!2.0rc1.post2.dev3+local`), ordered by `pep440_rs`.
    Pep440,
}

impl Scheme {
    fn rank(self) -> u8 {
        match self {
            Scheme::Debian => 0,
            Scheme::Semver => 1,
            Scheme::Generic => 2,
            Scheme::Rpm => 3,
            Scheme::Alpine => 4,
            Scheme::Pep440 => 5,
        }
    }
}

/// Why a version string could not be parsed under its scheme.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum VersionError {
    /// The string was empty.
    #[error("{}", errmsg::VERSION_EMPTY)]
    Empty,
    /// A SemVer string `semver` rejected, with its message.
    #[error("{kind}: {0}", kind = errmsg::VERSION_INVALID_SEMVER)]
    Semver(String),
    /// A string outside apk's version grammar.
    #[error("{kind}: {0:?}", kind = errmsg::VERSION_INVALID_APK)]
    Alpine(String),
    /// A string outside PEP 440's grammar, with the parser's message.
    #[error("{kind}: {0}", kind = errmsg::VERSION_INVALID_PEP440)]
    Pep440(String),
}

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
    Rpm(rpm::Evr),
    Alpine,
    Pep440(pep440_rs::Version),
}

/// What follows a `~` in a Debian version that names a pre-release.
const DEBIAN_PRERELEASE_TAGS: [&str; 5] = ["alpha", "beta", "rc", "pre", "dev"];

impl Version {
    /// Parse `raw` under `scheme`. Debian and Generic accept any non-empty
    /// string (their grammars are permissive); SemVer, Alpine and PEP 440
    /// are validated.
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
            Scheme::Rpm => Parsed::Rpm(rpm::Evr::parse(&raw)),
            Scheme::Alpine if alpine::is_valid(&raw) => Parsed::Alpine,
            Scheme::Alpine => return Err(VersionError::Alpine(raw)),
            Scheme::Pep440 => raw
                .parse::<pep440_rs::Version>()
                .map(Parsed::Pep440)
                .map_err(|e| VersionError::Pep440(e.to_string()))?,
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

    /// Whether this version is a pre-release of its final form: a SemVer
    /// pre-release segment, a Debian `~` before a pre-release tag (`~rc1`,
    /// `~beta2`), an RPM/generic `~` chunk (which sorts below the release),
    /// an Alpine `_alpha`/`_beta`/`_pre`/`_rc` suffix, or a PEP 440 alpha,
    /// beta, release candidate or dev release (a post-release is not one).
    /// Debian's other uses of `~` (`~deb12u1` stable updates, `~bpo12+1`
    /// backports, Ubuntu's `~24.04.1`) build the release itself. Selection
    /// never picks a pre-release unless a spec names it (§5.6).
    pub fn is_prerelease(&self) -> bool {
        match &self.parsed {
            Parsed::Semver(v) => !v.pre.is_empty(),
            Parsed::Debian => self.raw.split('~').skip(1).any(|after| {
                let after = after.to_ascii_lowercase();
                DEBIAN_PRERELEASE_TAGS
                    .iter()
                    .any(|tag| after.starts_with(tag))
            }),
            Parsed::Rpm(_) => self.raw.contains('~'),
            Parsed::Generic(segs) => segs.iter().any(|s| matches!(s, Segment::Tilde)),
            Parsed::Alpine => alpine::is_prerelease(&self.raw),
            Parsed::Pep440(v) => v.any_prerelease(),
        }
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
            (Parsed::Rpm(a), Parsed::Rpm(b)) => a.cmp(b),
            (Parsed::Alpine, Parsed::Alpine) => alpine::compare(&self.raw, &other.raw),
            (Parsed::Pep440(a), Parsed::Pep440(b)) => a.cmp(b),
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
    fn rpm(s: &str) -> Version {
        Version::parse(s, Scheme::Rpm).unwrap()
    }

    #[test]
    fn rpm_orders_epoch_version_release() {
        assert!(rpm("1:1.6-1.fc41") > rpm("2.0-1.fc41"));
        assert!(rpm("1.7.1-10.fc41") > rpm("1.7.1-8.fc41"));
        assert!(rpm("1.0~rc1-1") < rpm("1.0-1"));
        assert!(rpm("1.0^git1-1") > rpm("1.0-1"));
        assert_eq!(rpm("0:1.0-1"), rpm("1.0-1"));
        assert_ne!(rpm("1.0-1"), rpm("1.0-2"));
        assert_eq!(rpm("2:4.15.1-12.fc41").as_str(), "2:4.15.1-12.fc41");
        assert_eq!(Version::parse("", Scheme::Rpm), Err(VersionError::Empty));
    }

    #[test]
    fn a_debian_prerelease_is_a_tilde_before_a_prerelease_tag() {
        for pre in [
            "1.0~rc1-1",
            "2:1.0~beta2",
            "1.0~RC1",
            "3.12.0~alpha1-1",
            "1.0~pre3-2",
            "1.0~dev5",
        ] {
            assert!(deb(pre).is_prerelease(), "{pre}");
        }
        // A tilde that marks a stable update, backport or PPA build of the
        // release is not a pre-release of it: Debian 12's openssl and
        // ca-certificates (a native package), a backport, Ubuntu updates.
        for release in [
            "3.0.22-1~deb12u1",
            "20250419~deb12u1",
            "1:2.39.2-1~bpo12+1",
            "2.0.12-1~ubuntu24.04.1",
            "5.15.0-1~22.04",
            "1.0-1~ppa1",
            "1.6-2.1+deb12u3",
            "1.0~",
        ] {
            assert!(!deb(release).is_prerelease(), "{release}");
        }
    }

    #[test]
    fn rpm_prerelease_is_a_tilde_not_a_caret() {
        assert!(rpm("1.0~rc1-1.fc41").is_prerelease());
        assert!(!rpm("1.0^git1-1.fc41").is_prerelease());
        assert!(!rpm("1.0-1.fc41").is_prerelease());
    }

    #[test]
    fn rpm_ranks_after_the_other_schemes() {
        assert!(gen("9") < rpm("0"));
        assert_eq!(rpm("1.0").try_cmp(&gen("1.0")), None);
    }

    fn apk(s: &str) -> Version {
        Version::parse(s, Scheme::Alpine).unwrap()
    }

    #[test]
    fn alpine_orders_as_apk_does() {
        assert!(apk("1.7.1-r1") > apk("1.7.1-r0"));
        assert!(apk("1.8.0_rc1-r0") < apk("1.8.0-r0"));
        assert!(apk("1.10-r0") > apk("1.9-r0"));
        assert_eq!(apk("01.2"), apk("1.2"));
        assert_eq!(apk("1.0").try_cmp(&apk("1.0-r0")), Some(Ordering::Less));
    }

    #[test]
    fn alpine_rejects_what_apk_rejects() {
        assert_eq!(
            Version::parse("1.0-1", Scheme::Alpine),
            Err(VersionError::Alpine("1.0-1".into()))
        );
        assert_eq!(
            VersionError::Alpine("1.0-1".into()).to_string(),
            "invalid apk version: \"1.0-1\""
        );
        assert_eq!(Version::parse("", Scheme::Alpine), Err(VersionError::Empty));
    }

    #[test]
    fn alpine_pre_release_suffixes_are_pre_releases() {
        assert!(apk("1.8.0_rc1-r0").is_prerelease());
        assert!(!apk("1.8.0_p1-r0").is_prerelease());
        assert!(!apk("1.8.0-r0").is_prerelease());
    }

    fn pep(s: &str) -> Version {
        Version::parse(s, Scheme::Pep440).unwrap()
    }

    #[test]
    fn pep440_orders_as_the_pep_lists() {
        // PEP 440's own summary of ordering within a release, then epochs.
        let ordered = [
            "1.dev0",
            "1.0.dev456",
            "1.0a1",
            "1.0a2.dev456",
            "1.0a12.dev456",
            "1.0a12",
            "1.0b1.dev456",
            "1.0b2",
            "1.0b2.post345.dev456",
            "1.0b2.post345",
            "1.0rc1.dev456",
            "1.0rc1",
            "1.0",
            "1.0+abc.5",
            "1.0+abc.7",
            "1.0+5",
            "1.0.post456.dev34",
            "1.0.post456",
            "1.0.15",
            "1.1.dev1",
            "1!0.1",
        ];
        for pair in ordered.windows(2) {
            assert!(pep(pair[0]) < pep(pair[1]), "{} < {}", pair[0], pair[1]);
        }
    }

    #[test]
    fn pep440_pads_releases_and_normalizes_spellings() {
        assert_eq!(pep("1.0"), pep("1.0.0"));
        assert_eq!(pep("1.0a1"), pep("1.0-alpha.1"));
        assert_eq!(pep("1.0rc1"), pep("1.0c1"));
        assert_eq!(pep("v1.0.post1"), pep("1.0-1"));
        assert!(pep("1.10") > pep("1.9"));
        // The text stays what the backend reported.
        assert_eq!(pep("1.0-alpha.1").as_str(), "1.0-alpha.1");
    }

    #[test]
    fn pep440_pre_and_dev_releases_are_pre_releases_post_releases_are_not() {
        for v in ["1.0a1", "1.0b2", "1.0rc1", "1.0.dev3", "1.0.post1.dev1"] {
            assert!(pep(v).is_prerelease(), "{v}");
        }
        for v in ["1.0", "1.0.post1", "1!2.0", "1.0+local"] {
            assert!(!pep(v).is_prerelease(), "{v}");
        }
    }

    #[test]
    fn pep440_rejects_what_the_pep_rejects() {
        assert!(matches!(
            Version::parse("1.0-beta-gamma", Scheme::Pep440),
            Err(VersionError::Pep440(_))
        ));
        assert!(VersionError::Pep440("x".into())
            .to_string()
            .starts_with("invalid PEP 440 version: "));
        assert_eq!(Version::parse("", Scheme::Pep440), Err(VersionError::Empty));
        assert_eq!(pep("1.0").try_cmp(&sem("1.0.0")), None);
        assert!(apk("9") < pep("0"));
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
        assert!(gen("9.9") < apk("0"));
        assert_eq!(apk("1.0").try_cmp(&gen("1.0")), None);
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
