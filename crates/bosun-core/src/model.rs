//! Backend and package value types shared by the ports, resolver, and
//! applier: how a package is named to a backend, what a backend can do, and
//! what a verify or a claim records.
//!
//! These are plain data. Their JSON/YAML wire format is bosun-io's concern;
//! this module only fixes the Rust shape.

use std::path::PathBuf;

use crate::purl::Purl;
use crate::version::Version;

/// A backend's stable identifier: `apt`, `cargo`, `github-release`, … (§5.3).
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct BackendId(pub String);

impl BackendId {
    /// Construct from anything string-like.
    pub fn new(id: impl Into<String>) -> BackendId {
        BackendId(id.into())
    }

    /// The identifier as a string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A source's identifier: an apt PPA, a brew tap, a scoop bucket, … (§9).
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SourceId(pub String);

impl SourceId {
    /// Construct from anything string-like.
    pub fn new(id: impl Into<String>) -> SourceId {
        SourceId(id.into())
    }

    /// The identifier as a string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A source to ensure present before install (§9): which backend owns it and
/// its identifier (tap/PPA/bucket). Backend-specific detail stays opaque for
/// M1.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Source {
    /// The backend that owns this source.
    pub backend: BackendId,
    /// The source identifier.
    pub id: SourceId,
}

/// A backend-facing reference to one package: its canonical purl (§5.6). A
/// backend derives its native id (`purl.native_id()`), its qualifiers
/// (`arch`/`distro`/`repository_url`/`binary`) and its source from the purl.
/// Passed to [`Index`](crate) queries and installer operations; the backend
/// itself is implied by which installer is called.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ref {
    /// The package's canonical purl.
    pub purl: Purl,
}

impl Ref {
    /// A reference to `purl`.
    pub fn new(purl: Purl) -> Ref {
        Ref { purl }
    }
}

/// The install scope a backend lands in (§4.6), which drives the privilege
/// check and selects the state file.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Scope {
    /// Machine-wide (apt, dnf, winget `--scope machine`, …). Needs elevation.
    System,
    /// Per-user (cargo, go install, Homebrew, …). Must not be elevated.
    User,
}

/// How a package came to be present (§4.6), recorded per claim in machine
/// state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Origin {
    /// bosun installed it; prune may remove it once no manifest claims it.
    Installed,
    /// Already present, satisfying a claim; bosun never changes or removes it
    /// until explicitly adopted.
    Accepted,
}

/// What `verify_installed` found (§5.1): whether the package is present and,
/// when the backend can read it, its version.
///
/// A backend that reads the version from the manager (apt, cargo) reports
/// `Present { version: Some(..) }`. One that cannot without running a binary
/// it did not build (github-release) reports `Present { version: None }`, and
/// bosun state is the version authority (§4.6). There is **no sentinel
/// version**, so a version-less presence can never be mistaken for a version
/// in a comparison (which would reinstall every run — Appendix B).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Installed {
    /// The package is not installed.
    Absent,
    /// The package is installed, with its version if the backend can read it.
    Present {
        /// The installed version, or `None` when the backend cannot read it.
        version: Option<Version>,
    },
}

/// What a backend can honour (§5.1, §5.2). bosun degrades with a warning when
/// a backend cannot meet a pin.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Capabilities {
    /// Can install a specific version verbatim.
    pub supports_exact_version: bool,
    /// Can durably hold/pin an installed version.
    pub supports_hold: bool,
    /// Can move to a lower version on request.
    pub supports_downgrade: bool,
}

/// An upstream-published checksum (§4.5), e.g. `sha256:…`. Informational
/// except for github-release, where the backend enforces it.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Checksum(pub String);

/// One manifest's claim on a package (§4.6): where the manifest lives, the
/// spec it asked for, and — in system-scope state — the OS user who claimed
/// it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Claim {
    /// The manifest file that holds this claim.
    pub manifest: PathBuf,
    /// The spec string the manifest asked for (§5.6: a bare version or vers).
    pub spec: String,
    /// The OS user who claimed it (system scope only; §4.6).
    pub user: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::version::Scheme;

    #[test]
    fn ref_carries_the_purl() {
        let r = Ref::new("pkg:deb/debian/jq".parse().unwrap());
        assert_eq!(r.purl.native_id(), "jq");
    }

    #[test]
    fn backend_and_source_ids_expose_str() {
        assert_eq!(BackendId::new("apt").as_str(), "apt");
        assert_eq!(SourceId::new("ctxloom/tap").as_str(), "ctxloom/tap");
    }

    #[test]
    fn installed_present_without_a_version_carries_no_version() {
        // A version-less presence has no Version to compare against a spec.
        let absent = Installed::Absent;
        let present_unknown = Installed::Present { version: None };
        let present_known = Installed::Present {
            version: Some(Version::parse("1.2.3", Scheme::Semver).unwrap()),
        };
        assert_ne!(absent, present_unknown);
        assert_ne!(present_unknown, present_known);
        match present_unknown {
            Installed::Present { version } => assert!(version.is_none()),
            Installed::Absent => unreachable!(),
        }
    }
}
