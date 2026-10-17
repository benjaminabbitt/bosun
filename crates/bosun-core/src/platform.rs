//! Platform identity: the operating systems and architectures bosun targets,
//! the [`Target`] key used throughout the lock and machine state (§4.5), and
//! the manifest [`Selector`] (§5.4).
//!
//! [`Target`] uses Docker's platform syntax (`linux/amd64`) so a container
//! build can look an entry up with BuildKit's `TARGETPLATFORM`, plus the
//! literal `any` for platform-independent backends (cargo, go, npm, …). It is
//! serialised as that string, so it can key a JSON object directly.
//!
//! [`Selector`] is the manifest `install` key: `any`, `<os>`, `<os>:<distro>`,
//! `<os>:<distro>@<release>`, or the libc selector `linux:<libc>[@<range>]`
//! (§5.4). Ranking selectors against a concrete machine (release vs ID vs
//! ID_LIKE vs libc precedence) belongs to resolution, not here; this type is
//! only the parsed key, and [`Libc`] and [`Distro`] the machine facts.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::errmsg;
use crate::spec::Spec;
use crate::version::{Scheme, Version};

/// An operating system bosun can target. Rendered with Docker's spelling, so
/// macOS is `darwin`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Os {
    /// Linux.
    Linux,
    /// macOS (Docker spelling: `darwin`).
    Macos,
    /// Windows.
    Windows,
}

/// A CPU architecture, in Docker's spelling.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Arch {
    /// x86-64 (`amd64`).
    Amd64,
    /// 64-bit ARM (`arm64`).
    Arm64,
}

/// A resolution target: either a concrete OS/arch platform, or `any` for
/// platform-independent backends (§4.5, §4.7).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Target {
    /// Platform-independent (cargo, go, npm, …): resolved once under `any`.
    Any,
    /// A concrete OS/arch platform.
    Platform {
        /// The operating system.
        os: Os,
        /// The architecture.
        arch: Arch,
    },
}

/// A manifest `install` selector key (§5.4): every OS, an OS, an OS narrowed
/// to a distro family, or one release of a distro.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Selector {
    /// Any machine (`any`), for backends no OS ties down (npm, cargo, …); the
    /// least specific selector.
    Any,
    /// Any machine of this OS.
    Os(Os),
    /// This OS narrowed to a distro (its `ID` or an `ID_LIKE` family).
    OsDistro(Os, String),
    /// One release of a distro (`linux:debian@bookworm`): the distro's own
    /// `ID`, and the release spelled as its packages' purl `distro` qualifier
    /// ([`Distro::release`]).
    OsDistroRelease(Os, String, String),
    /// A C library family, optionally with a version range (`linux:musl`,
    /// `linux:gnu@>=2.31`). Linux only. The family names `gnu` and `musl` are
    /// reserved: `linux:gnu` is always this, never a distro.
    OsLibc(Os, LibcFamily, Option<LibcRange>),
}

/// A C library family (§5.4), spelled as the environment field of a Rust
/// target triple: `gnu` (glibc) or `musl`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum LibcFamily {
    /// glibc.
    Gnu,
    /// musl.
    Musl,
}

/// A libc version range from a selector (`>=2.31` in `linux:gnu@>=2.31`): a
/// vers `generic` range, kept as written. Built only by [`LibcRange::parse`],
/// so it always parses.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct LibcRange(String);

/// The machine's C library (§5.4): its family, and its version when the
/// loader reported one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Libc {
    /// The family.
    pub family: LibcFamily,
    /// The version (`2.36`, `1.2.5`), when known.
    pub version: Option<String>,
}

/// Why a platform string could not be parsed.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum PlatformError {
    /// Not one of the known operating systems.
    #[error("{kind}: {0}", kind = errmsg::PLATFORM_UNKNOWN_OS)]
    UnknownOs(String),
    /// Not one of the known architectures.
    #[error("{kind}: {0}", kind = errmsg::PLATFORM_UNKNOWN_ARCH)]
    UnknownArch(String),
    /// Not of the form `os/arch` (or `any`).
    #[error("{kind}: {0}", kind = errmsg::PLATFORM_INVALID_TARGET)]
    InvalidTarget(String),
    /// Not of the form `any`, `os`, `os:distro`, `os:distro@release` or
    /// `linux:<libc>[@range]`.
    #[error("{kind}: {0}", kind = errmsg::PLATFORM_INVALID_SELECTOR)]
    InvalidSelector(String),
    /// A libc selector for an OS other than Linux.
    #[error("{kind}: {0}", kind = errmsg::PLATFORM_LIBC_NOT_LINUX)]
    LibcNotLinux(String),
    /// A libc selector's range is not a vers `generic` range.
    #[error("{kind}: {range:?}: {detail}", kind = errmsg::PLATFORM_INVALID_LIBC_RANGE)]
    InvalidLibcRange {
        /// The range as written.
        range: String,
        /// Why it was refused.
        detail: String,
    },
    /// Not one of the known C library families.
    #[error("{kind}: {0}", kind = errmsg::PLATFORM_UNKNOWN_LIBC)]
    UnknownLibc(String),
}

impl fmt::Display for Os {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Os::Linux => "linux",
            Os::Macos => "darwin",
            Os::Windows => "windows",
        })
    }
}

impl FromStr for Os {
    type Err = PlatformError;
    fn from_str(s: &str) -> Result<Os, PlatformError> {
        match s {
            "linux" => Ok(Os::Linux),
            "darwin" => Ok(Os::Macos),
            "windows" => Ok(Os::Windows),
            _ => Err(PlatformError::UnknownOs(s.to_string())),
        }
    }
}

impl fmt::Display for Arch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Arch::Amd64 => "amd64",
            Arch::Arm64 => "arm64",
        })
    }
}

impl FromStr for Arch {
    type Err = PlatformError;
    fn from_str(s: &str) -> Result<Arch, PlatformError> {
        match s {
            "amd64" => Ok(Arch::Amd64),
            "arm64" => Ok(Arch::Arm64),
            _ => Err(PlatformError::UnknownArch(s.to_string())),
        }
    }
}

impl Target {
    /// The accessor used by selectors and privilege checks.
    pub fn os(&self) -> Option<Os> {
        match self {
            Target::Any => None,
            Target::Platform { os, .. } => Some(*os),
        }
    }
}

impl fmt::Display for Target {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Target::Any => f.write_str("any"),
            Target::Platform { os, arch } => write!(f, "{os}/{arch}"),
        }
    }
}

impl FromStr for Target {
    type Err = PlatformError;
    fn from_str(s: &str) -> Result<Target, PlatformError> {
        if s == "any" {
            return Ok(Target::Any);
        }
        let (os, arch) = s
            .split_once('/')
            .ok_or_else(|| PlatformError::InvalidTarget(s.to_string()))?;
        // `os/arch/extra` has a trailing component: reject rather than
        // silently treating `arch/extra` as an architecture.
        if arch.contains('/') {
            return Err(PlatformError::InvalidTarget(s.to_string()));
        }
        Ok(Target::Platform {
            os: os.parse()?,
            arch: arch.parse()?,
        })
    }
}

impl Serialize for Target {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for Target {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Target, D::Error> {
        let s = String::deserialize(d)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

impl Selector {
    /// The OS this selector applies to; `None` for [`Selector::Any`].
    pub fn os(&self) -> Option<Os> {
        match self {
            Selector::Any => None,
            Selector::Os(os)
            | Selector::OsDistro(os, _)
            | Selector::OsDistroRelease(os, ..)
            | Selector::OsLibc(os, ..) => Some(*os),
        }
    }

    /// The distro, when the selector names one.
    pub fn distro(&self) -> Option<&str> {
        match self {
            Selector::Any | Selector::Os(_) | Selector::OsLibc(..) => None,
            Selector::OsDistro(_, d) | Selector::OsDistroRelease(_, d, _) => Some(d),
        }
    }

    /// The libc family and range, when the selector names a libc.
    pub fn libc(&self) -> Option<(LibcFamily, Option<&LibcRange>)> {
        match self {
            Selector::OsLibc(_, family, range) => Some((*family, range.as_ref())),
            _ => None,
        }
    }

    /// The release, when the selector names one.
    pub fn release(&self) -> Option<&str> {
        match self {
            Selector::OsDistroRelease(_, _, release) => Some(release),
            _ => None,
        }
    }
}

impl fmt::Display for Selector {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Selector::Any => f.write_str("any"),
            Selector::Os(os) => write!(f, "{os}"),
            Selector::OsDistro(os, distro) => write!(f, "{os}:{distro}"),
            Selector::OsDistroRelease(os, distro, release) => {
                write!(f, "{os}:{distro}@{release}")
            }
            Selector::OsLibc(os, family, None) => write!(f, "{os}:{family}"),
            Selector::OsLibc(os, family, Some(range)) => write!(f, "{os}:{family}@{range}"),
        }
    }
}

impl FromStr for Selector {
    type Err = PlatformError;
    fn from_str(s: &str) -> Result<Selector, PlatformError> {
        let invalid = || PlatformError::InvalidSelector(s.to_string());
        match s.split_once(':') {
            None if s == "any" => Ok(Selector::Any),
            None => Ok(Selector::Os(s.parse()?)),
            Some(("any", _) | (_, "")) => Err(invalid()),
            Some((os, rest)) => match rest.split_once('@') {
                Some((name, range)) if name.parse::<LibcFamily>().is_ok() => {
                    libc_selector(s, os, name, Some(range))
                }
                None if rest.parse::<LibcFamily>().is_ok() => libc_selector(s, os, rest, None),
                None => Ok(Selector::OsDistro(os.parse()?, rest.to_string())),
                Some(("", _) | (_, "")) => Err(invalid()),
                Some((_, release)) if release.contains('@') => Err(invalid()),
                Some((distro, release)) => Ok(Selector::OsDistroRelease(
                    os.parse()?,
                    distro.to_string(),
                    release.to_string(),
                )),
            },
        }
    }
}

impl fmt::Display for LibcFamily {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            LibcFamily::Gnu => "gnu",
            LibcFamily::Musl => "musl",
        })
    }
}

impl FromStr for LibcFamily {
    type Err = PlatformError;
    fn from_str(s: &str) -> Result<LibcFamily, PlatformError> {
        match s {
            "gnu" => Ok(LibcFamily::Gnu),
            "musl" => Ok(LibcFamily::Musl),
            _ => Err(PlatformError::UnknownLibc(s.to_string())),
        }
    }
}

impl LibcRange {
    /// Parse a selector's range: vers `generic` constraints, with or without
    /// the `vers:generic/` prefix (`>=2.31`, `>=2.31|<3`).
    pub fn parse(s: &str) -> Result<LibcRange, PlatformError> {
        let invalid = |detail: String| PlatformError::InvalidLibcRange {
            range: s.to_string(),
            detail,
        };
        let spec = Spec::new(s, "generic").map_err(|e| invalid(e.to_string()))?;
        match spec.range().ty() {
            "generic" => Ok(LibcRange(s.to_string())),
            other => Err(invalid(format!("vers:{other}"))),
        }
    }

    /// The range as written.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Whether `version` lies in this range, by bosun's generic ordering. A
    /// version that does not parse is in no range.
    pub fn contains(&self, version: &str) -> bool {
        let Ok(spec) = Spec::new(self.0.as_str(), "generic") else {
            return false;
        };
        Version::parse(version, Scheme::Generic).is_ok_and(|v| spec.range().contains(&v))
    }
}

impl fmt::Display for LibcRange {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl Libc {
    /// Whether this libc is `family`, with a version in `range` when one is
    /// given. An unknown version satisfies no range.
    pub fn satisfies(&self, family: LibcFamily, range: Option<&LibcRange>) -> bool {
        self.family == family
            && range.is_none_or(|r| self.version.as_deref().is_some_and(|v| r.contains(v)))
    }
}

impl fmt::Display for Libc {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.version {
            Some(v) => write!(f, "{} {v}", self.family),
            None => write!(f, "{}", self.family),
        }
    }
}

/// A libc selector `os:name[@range]`, `name` a known family: Linux only, and
/// the range a vers `generic` range.
fn libc_selector(
    s: &str,
    os: &str,
    name: &str,
    range: Option<&str>,
) -> Result<Selector, PlatformError> {
    let os: Os = os.parse()?;
    if os != Os::Linux {
        return Err(PlatformError::LibcNotLinux(s.to_string()));
    }
    Ok(Selector::OsLibc(
        os,
        name.parse()?,
        range.map(LibcRange::parse).transpose()?,
    ))
}

/// Distro facts read from `/etc/os-release` (§5.4). The `Machine` interface
/// surfaces these; os-release parsing itself lives in bosun-host.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Distro {
    /// The `ID` field, e.g. `ubuntu`, `debian`, `fedora`.
    pub id: String,
    /// The `ID_LIKE` families, e.g. `[debian]` for Ubuntu.
    pub id_like: Vec<String>,
    /// The `VERSION_ID` field, when present (absent on rolling distros).
    pub version_id: Option<String>,
    /// The `VERSION_CODENAME` field (e.g. `bookworm`, `noble`), used as the
    /// deb purl `distro` qualifier (§5.6). Absent on distros without one.
    pub version_codename: Option<String>,
    /// The codename of the distro whose archive this one's apt reads:
    /// `UBUNTU_CODENAME`, else `DEBIAN_CODENAME` (Linux Mint 22 `noble`
    /// under `VERSION_CODENAME=wilma`; LMDE 6 `bookworm` under `faye`).
    pub base_codename: Option<String>,
}

impl Distro {
    /// Whether this distro belongs to `family` by the §5.4 rule: its `ID`
    /// equals the family, or its `ID_LIKE` contains it. So Ubuntu, Mint and
    /// Pop!_OS all satisfy `debian`. Combining this with the OS and ranking
    /// selector specificity is resolution's job, not this type's.
    pub fn satisfies_family(&self, family: &str) -> bool {
        self.id == family || self.id_like.iter().any(|f| f == family)
    }

    /// This distro's release as packages of `purl_type` spell it: the value of
    /// their purl `distro` qualifier (§5.6). deb: the codename apt uses, the
    /// base distro's where os-release names one, else `VERSION_CODENAME`
    /// (`bookworm`; `noble` on Linux Mint 22); rpm: `<ID>-<VERSION_ID>`
    /// (`fedora-41`). `None` for a type
    /// with no such rule, or when a field it needs is missing or blank (Arch,
    /// Debian sid), so a release selector never matches there.
    pub fn release(&self, purl_type: &str) -> Option<String> {
        let present = |field: &Option<String>| field.clone().filter(|v| !v.is_empty());
        match purl_type {
            "deb" => present(&self.base_codename).or_else(|| present(&self.version_codename)),
            "rpm" => present(&self.version_id).map(|v| format!("{}-{v}", self.id)),
            _ => None,
        }
    }

    /// The deb release to name in place of `release` when `release` is this
    /// distro's own codename but its packages spell the release by the base
    /// distro's (Linux Mint 22: `wilma`, whose apt reads Ubuntu `noble`), so
    /// a selector naming it matches nothing. `None` otherwise.
    pub fn deb_release_for_own_codename(&self, release: &str) -> Option<String> {
        let deb = self.release("deb")?;
        (self.version_codename.as_deref() == Some(release) && deb != release).then_some(deb)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn os_roundtrips_docker_spelling() {
        for (os, s) in [
            (Os::Linux, "linux"),
            (Os::Macos, "darwin"),
            (Os::Windows, "windows"),
        ] {
            assert_eq!(os.to_string(), s);
            assert_eq!(s.parse::<Os>().unwrap(), os);
        }
    }

    #[test]
    fn os_rejects_unknown() {
        assert!(matches!(
            "plan9".parse::<Os>(),
            Err(PlatformError::UnknownOs(_))
        ));
    }

    #[test]
    fn arch_roundtrips() {
        for (arch, s) in [(Arch::Amd64, "amd64"), (Arch::Arm64, "arm64")] {
            assert_eq!(arch.to_string(), s);
            assert_eq!(s.parse::<Arch>().unwrap(), arch);
        }
    }

    #[test]
    fn arch_rejects_unknown() {
        assert!(matches!(
            "sparc".parse::<Arch>(),
            Err(PlatformError::UnknownArch(_))
        ));
    }

    #[test]
    fn target_roundtrips_platform_and_any() {
        let p = Target::Platform {
            os: Os::Linux,
            arch: Arch::Amd64,
        };
        assert_eq!(p.to_string(), "linux/amd64");
        assert_eq!("linux/amd64".parse::<Target>().unwrap(), p);
        assert_eq!(Target::Any.to_string(), "any");
        assert_eq!("any".parse::<Target>().unwrap(), Target::Any);
    }

    #[test]
    fn target_os_is_some_for_platform_none_for_any() {
        assert_eq!(
            Target::Platform {
                os: Os::Linux,
                arch: Arch::Amd64,
            }
            .os(),
            Some(Os::Linux)
        );
        assert_eq!(Target::Any.os(), None);
    }

    #[test]
    fn target_rejects_malformed() {
        assert!(matches!(
            "linux".parse::<Target>(),
            Err(PlatformError::InvalidTarget(_))
        ));
        assert!(matches!(
            "linux/amd64/extra".parse::<Target>(),
            Err(PlatformError::InvalidTarget(_))
        ));
        assert!(matches!(
            "plan9/amd64".parse::<Target>(),
            Err(PlatformError::UnknownOs(_))
        ));
    }

    #[test]
    fn target_serializes_as_string_key() {
        let p = Target::Platform {
            os: Os::Linux,
            arch: Arch::Arm64,
        };
        assert_eq!(serde_json::to_string(&p).unwrap(), "\"linux/arm64\"");
        let back: Target = serde_json::from_str("\"linux/arm64\"").unwrap();
        assert_eq!(back, p);
    }

    #[test]
    fn selector_roundtrips_os_and_os_distro() {
        let os = Selector::Os(Os::Linux);
        assert_eq!(os.to_string(), "linux");
        assert_eq!("linux".parse::<Selector>().unwrap(), os);

        let od = Selector::OsDistro(Os::Linux, "debian".to_string());
        assert_eq!(od.to_string(), "linux:debian");
        assert_eq!("linux:debian".parse::<Selector>().unwrap(), od);
        assert_eq!(od.os(), Some(Os::Linux));
        assert_eq!(od.distro(), Some("debian"));
        assert_eq!(os.distro(), None);
    }

    #[test]
    fn selector_any_applies_to_every_os() {
        assert_eq!("any".parse::<Selector>().unwrap(), Selector::Any);
        assert_eq!(Selector::Any.to_string(), "any");
        assert_eq!(Selector::Any.os(), None);
        assert_eq!(Selector::Any.distro(), None);
    }

    #[test]
    fn selector_any_takes_no_distro() {
        assert!(matches!(
            "any:debian".parse::<Selector>(),
            Err(PlatformError::InvalidSelector(_))
        ));
    }

    #[test]
    fn distro_satisfies_family_by_id_or_id_like() {
        let ubuntu = Distro {
            id: "ubuntu".into(),
            id_like: vec!["debian".into()],
            version_id: Some("24.04".into()),
            version_codename: Some("noble".into()),
            base_codename: None,
        };
        assert!(ubuntu.satisfies_family("ubuntu")); // by ID
        assert!(ubuntu.satisfies_family("debian")); // by ID_LIKE
        assert!(!ubuntu.satisfies_family("fedora"));
    }

    #[test]
    fn selector_roundtrips_a_release() {
        let rel = Selector::OsDistroRelease(Os::Linux, "debian".into(), "bookworm".into());
        assert_eq!(rel.to_string(), "linux:debian@bookworm");
        assert_eq!("linux:debian@bookworm".parse::<Selector>().unwrap(), rel);
        assert_eq!(rel.os(), Some(Os::Linux));
        assert_eq!(rel.distro(), Some("debian"));
        assert_eq!(rel.release(), Some("bookworm"));
        assert_eq!(
            "linux:fedora@fedora-41".parse::<Selector>().unwrap(),
            Selector::OsDistroRelease(Os::Linux, "fedora".into(), "fedora-41".into())
        );
    }

    #[test]
    fn only_a_release_selector_has_a_release() {
        assert_eq!(Selector::Any.release(), None);
        assert_eq!(Selector::Os(Os::Linux).release(), None);
        assert_eq!(
            Selector::OsDistro(Os::Linux, "debian".into()).release(),
            None
        );
    }

    #[test]
    fn selector_rejects_a_malformed_release() {
        for s in [
            "linux:debian@",
            "linux:@bookworm",
            "linux@bookworm",
            "any:debian@bookworm",
            "linux:debian@book@worm",
        ] {
            assert!(s.parse::<Selector>().is_err(), "{s} should not parse");
        }
        assert!(matches!(
            "linux:debian@".parse::<Selector>(),
            Err(PlatformError::InvalidSelector(_))
        ));
        assert!(matches!(
            "linux:@bookworm".parse::<Selector>(),
            Err(PlatformError::InvalidSelector(_))
        ));
    }

    fn os_release(id: &str, version_id: Option<&str>, codename: Option<&str>) -> Distro {
        Distro {
            id: id.into(),
            id_like: Vec::new(),
            version_id: version_id.map(str::to_string),
            version_codename: codename.map(str::to_string),
            base_codename: None,
        }
    }

    #[test]
    fn deb_release_is_the_codename() {
        let bookworm = os_release("debian", Some("12"), Some("bookworm"));
        assert_eq!(bookworm.release("deb").as_deref(), Some("bookworm"));
    }

    #[test]
    fn a_derivatives_deb_release_is_its_base_distros_codename() {
        let mint = Distro {
            base_codename: Some("noble".into()),
            ..os_release("linuxmint", Some("22"), Some("wilma"))
        };
        assert_eq!(mint.release("deb").as_deref(), Some("noble"));
        assert_eq!(mint.release("rpm").as_deref(), Some("linuxmint-22"));
        let blank = Distro {
            base_codename: Some(String::new()),
            ..mint
        };
        assert_eq!(blank.release("deb").as_deref(), Some("wilma"));
    }

    #[test]
    fn a_derivatives_own_codename_points_to_its_deb_release() {
        let mint = Distro {
            base_codename: Some("noble".into()),
            ..os_release("linuxmint", Some("22"), Some("wilma"))
        };
        assert_eq!(
            mint.deb_release_for_own_codename("wilma").as_deref(),
            Some("noble")
        );
        assert_eq!(mint.deb_release_for_own_codename("noble"), None);
        assert_eq!(mint.deb_release_for_own_codename("faye"), None);
        // Debian's own codename is its release.
        let bookworm = os_release("debian", Some("12"), Some("bookworm"));
        assert_eq!(bookworm.deb_release_for_own_codename("bookworm"), None);
        // No deb release at all: nothing to point to.
        let sid = os_release("debian", None, None);
        assert_eq!(sid.deb_release_for_own_codename("sid"), None);
    }

    #[test]
    fn rpm_release_is_id_dash_version_id() {
        let fedora = os_release("fedora", Some("41"), None);
        assert_eq!(fedora.release("rpm").as_deref(), Some("fedora-41"));
    }

    #[test]
    fn a_missing_or_blank_field_gives_no_release() {
        let sid = os_release("debian", None, None);
        assert_eq!(sid.release("deb"), None);
        assert_eq!(sid.release("rpm"), None);
        let blank = os_release("fedora", Some(""), Some(""));
        assert_eq!(blank.release("deb"), None);
        assert_eq!(blank.release("rpm"), None);
    }

    #[test]
    fn types_without_a_distro_rule_have_no_release() {
        let bookworm = os_release("debian", Some("12"), Some("bookworm"));
        for t in ["github", "cargo", "npm", "generic"] {
            assert_eq!(bookworm.release(t), None, "{t}");
        }
    }

    // --- libc ----------------------------------------------------------------

    fn range(s: &str) -> LibcRange {
        LibcRange::parse(s).unwrap()
    }

    #[test]
    fn libc_family_roundtrips_the_rust_triple_spelling() {
        for (family, s) in [(LibcFamily::Gnu, "gnu"), (LibcFamily::Musl, "musl")] {
            assert_eq!(family.to_string(), s);
            assert_eq!(s.parse::<LibcFamily>().unwrap(), family);
        }
        for s in ["glibc", "GNU", "", "uclibc"] {
            assert!(
                matches!(s.parse::<LibcFamily>(), Err(PlatformError::UnknownLibc(x)) if x == s),
                "{s}"
            );
        }
    }

    #[test]
    fn a_libc_name_is_a_libc_selector_not_a_distro() {
        let gnu = "linux:gnu".parse::<Selector>().unwrap();
        assert_eq!(gnu, Selector::OsLibc(Os::Linux, LibcFamily::Gnu, None));
        assert_eq!(gnu.libc(), Some((LibcFamily::Gnu, None)));
        assert_eq!(gnu.distro(), None);
        assert_eq!(gnu.release(), None);
        assert_eq!(gnu.os(), Some(Os::Linux));
        assert_eq!(gnu.to_string(), "linux:gnu");
        assert_eq!(
            "linux:musl".parse::<Selector>().unwrap(),
            Selector::OsLibc(Os::Linux, LibcFamily::Musl, None)
        );
    }

    #[test]
    fn a_libc_selector_takes_a_range_after_the_at() {
        let floor = "linux:gnu@>=2.31".parse::<Selector>().unwrap();
        assert_eq!(
            floor,
            Selector::OsLibc(Os::Linux, LibcFamily::Gnu, Some(range(">=2.31")))
        );
        assert_eq!(floor.to_string(), "linux:gnu@>=2.31");
        let (family, r) = floor.libc().unwrap();
        assert_eq!(family, LibcFamily::Gnu);
        assert_eq!(r.unwrap().as_str(), ">=2.31");
        assert_eq!(floor.release(), None);
        // A full vers string, and a bare version (one version), are ranges too.
        assert!("linux:gnu@vers:generic/>=2.31|<3"
            .parse::<Selector>()
            .is_ok());
        assert!("linux:musl@1.2.5".parse::<Selector>().is_ok());
        // Other selectors have no libc.
        for s in ["any", "linux", "linux:debian", "linux:debian@bookworm"] {
            assert_eq!(s.parse::<Selector>().unwrap().libc(), None, "{s}");
        }
    }

    #[test]
    fn a_distro_at_a_release_is_still_a_release() {
        assert_eq!(
            "linux:debian@bookworm".parse::<Selector>().unwrap(),
            Selector::OsDistroRelease(Os::Linux, "debian".into(), "bookworm".into())
        );
    }

    #[test]
    fn a_libc_range_must_be_a_generic_vers_range() {
        for s in [
            "linux:gnu@",
            "linux:gnu@>=two|<",
            "linux:gnu@vers:semver/>=2.31.0",
            "linux:gnu@>=2.31 ",
            "linux:musl@<2|>=1",
        ] {
            assert!(
                matches!(
                    s.parse::<Selector>(),
                    Err(PlatformError::InvalidLibcRange { .. })
                ),
                "{s} should be an invalid range: {:?}",
                s.parse::<Selector>()
            );
        }
    }

    #[test]
    fn libc_selectors_are_linux_only() {
        for s in ["windows:gnu", "darwin:musl", "windows:gnu@>=2.31"] {
            assert!(
                matches!(s.parse::<Selector>(), Err(PlatformError::LibcNotLinux(x)) if x == s),
                "{s}"
            );
        }
        assert!(matches!(
            "plan9:gnu".parse::<Selector>(),
            Err(PlatformError::UnknownOs(_))
        ));
    }

    #[test]
    fn a_libc_range_compares_by_generic_ordering() {
        let floor = range(">=2.31");
        assert!(floor.contains("2.31"));
        assert!(floor.contains("2.36"));
        assert!(floor.contains("2.100"));
        assert!(!floor.contains("2.30"));
        assert!(!floor.contains("2.4"));
        let band = range("vers:generic/>=1.2|<1.3");
        assert!(band.contains("1.2.5"));
        assert!(!band.contains("1.3"));
        assert!(!floor.contains(""));
    }

    fn glibc(v: Option<&str>) -> Libc {
        Libc {
            family: LibcFamily::Gnu,
            version: v.map(str::to_string),
        }
    }

    #[test]
    fn libc_satisfies_its_family_and_a_range_its_version_is_in() {
        let g236 = glibc(Some("2.36"));
        assert!(g236.satisfies(LibcFamily::Gnu, None));
        assert!(!g236.satisfies(LibcFamily::Musl, None));
        assert!(g236.satisfies(LibcFamily::Gnu, Some(&range(">=2.31"))));
        assert!(!g236.satisfies(LibcFamily::Gnu, Some(&range(">=2.39"))));
        assert!(!g236.satisfies(LibcFamily::Musl, Some(&range(">=1"))));
        // An unknown version satisfies the bare family, never a range.
        let unknown = glibc(None);
        assert!(unknown.satisfies(LibcFamily::Gnu, None));
        assert!(!unknown.satisfies(LibcFamily::Gnu, Some(&range(">=0"))));
    }

    #[test]
    fn libc_displays_family_then_version() {
        assert_eq!(glibc(Some("2.36")).to_string(), "gnu 2.36");
        assert_eq!(glibc(None).to_string(), "gnu");
    }

    #[test]
    fn selector_rejects_malformed() {
        assert!(matches!(
            "linux:".parse::<Selector>(),
            Err(PlatformError::InvalidSelector(_))
        ));
        assert!(matches!(
            "plan9".parse::<Selector>(),
            Err(PlatformError::UnknownOs(_))
        ));
    }
}
