//! Platform identity: the operating systems and architectures bosun targets,
//! the [`Target`] key used throughout the lock and machine state (§4.5), and
//! the manifest [`Selector`] (§5.4).
//!
//! [`Target`] uses Docker's platform syntax (`linux/amd64`) so a container
//! build can look an entry up with BuildKit's `TARGETPLATFORM`, plus the
//! literal `any` for platform-independent backends (cargo, go, npm, …). It is
//! serialised as that string, so it can key a JSON object directly.
//!
//! [`Selector`] is the manifest `install` key: `any`, `<os>` or
//! `<os>:<distro>` (§5.4). Matching a selector against a concrete machine (ID vs ID_LIKE
//! precedence, distro facts) belongs to resolution, not here; this type is
//! only the parsed key.

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

/// A manifest `install` selector key (§5.4): every OS, an OS, or an OS
/// narrowed to a distro family.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Selector {
    /// Any machine (`any`), for backends no OS ties down (npm, cargo, …); the
    /// least specific selector.
    Any,
    /// Any machine of this OS.
    Os(Os),
    /// This OS narrowed to a distro (its `ID` or an `ID_LIKE` family).
    OsDistro(Os, String),
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
    /// Not of the form `os` or `os:distro`.
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
            Selector::Os(os) | Selector::OsDistro(os, _) => Some(*os),
        }
    }

    /// The distro family, when the selector names one.
    pub fn distro(&self) -> Option<&str> {
        match self {
            Selector::Any | Selector::Os(_) => None,
            Selector::OsDistro(_, d) => Some(d),
        }
    }
}

impl fmt::Display for Selector {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Selector::Any => f.write_str("any"),
            Selector::Os(os) => write!(f, "{os}"),
            Selector::OsDistro(os, distro) => write!(f, "{os}:{distro}"),
        }
    }
}

impl FromStr for Selector {
    type Err = PlatformError;
    fn from_str(s: &str) -> Result<Selector, PlatformError> {
        match s.split_once(':') {
            None if s == "any" => Ok(Selector::Any),
            None => Ok(Selector::Os(s.parse()?)),
            Some(("any", _) | (_, "")) => Err(PlatformError::InvalidSelector(s.to_string())),
            Some((os, distro)) => Ok(Selector::OsDistro(os.parse()?, distro.to_string())),
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
