//! Platform identity: the operating systems and architectures bosun targets,
//! the [`Target`] key used throughout the lock and machine state (§4.5), and
//! the manifest [`Selector`] (§5.4).
//!
//! [`Target`] uses Docker's platform syntax (`linux/amd64`) so a container
//! build can look an entry up with BuildKit's `TARGETPLATFORM`, plus the
//! literal `any` for platform-independent backends (cargo, go, npm, …). It is
//! serialised as that string, so it can key a JSON object directly.
//!
//! [`Selector`] is the manifest `install` key: `any`, `<os>`, `<os>:<distro>`
//! or `<os>:<distro>@<release>` (§5.4). Matching a selector against a concrete
//! machine (release vs ID vs ID_LIKE precedence, distro facts) belongs to
//! resolution, not here; this type is only the parsed key.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::errmsg;

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
    /// Not of the form `any`, `os`, `os:distro` or `os:distro@release`.
    #[error("{kind}: {0}", kind = errmsg::PLATFORM_INVALID_SELECTOR)]
    InvalidSelector(String),
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
            Selector::Os(os) | Selector::OsDistro(os, _) | Selector::OsDistroRelease(os, ..) => {
                Some(*os)
            }
        }
    }

    /// The distro, when the selector names one.
    pub fn distro(&self) -> Option<&str> {
        match self {
            Selector::Any | Selector::Os(_) => None,
            Selector::OsDistro(_, d) | Selector::OsDistroRelease(_, d, _) => Some(d),
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
    /// their purl `distro` qualifier (§5.6). deb: `VERSION_CODENAME`
    /// (`bookworm`); rpm: `<ID>-<VERSION_ID>` (`fedora-41`). `None` for a type
    /// with no such rule, or when a field it needs is missing or blank (Arch,
    /// Debian sid), so a release selector never matches there.
    pub fn release(&self, purl_type: &str) -> Option<String> {
        let present = |field: &Option<String>| field.clone().filter(|v| !v.is_empty());
        match purl_type {
            "deb" => present(&self.version_codename),
            "rpm" => present(&self.version_id).map(|v| format!("{}-{v}", self.id)),
            _ => None,
        }
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
        }
    }

    #[test]
    fn deb_release_is_the_codename() {
        let bookworm = os_release("debian", Some("12"), Some("bookworm"));
        assert_eq!(bookworm.release("deb").as_deref(), Some("bookworm"));
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
