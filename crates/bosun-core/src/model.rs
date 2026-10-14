//! Backend and package value types shared by the ports, resolver, and
//! applier: how a package is named to a backend, what a backend can do, and
//! what an install or a claim records.
//!
//! These are plain data. Their JSON/YAML wire format is bosun-io's concern
//! (slice 5); this module only fixes the Rust shape.

use std::path::PathBuf;

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

/// A package's identifier *as a backend names it* (§5.4): `fd-find` on apt,
/// `sharkdp.fd` on winget, `<owner>/bosun` for github-release.
///
/// Some backends distinguish a *kind* under one manager — brew formula vs
/// cask, scoop `bucket/app` (Appendix B). M1 backends don't need it, so
/// `kind` is an opaque optional string rather than a closed enum; it is
/// carried so verify and prune target exactly what install did.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PackageId {
    /// The backend-native package identifier.
    pub id: String,
    /// An optional backend-specific kind (e.g. `cask`), opaque in M1.
    pub kind: Option<String>,
}

impl PackageId {
    /// A plain package id with no kind.
    pub fn new(id: impl Into<String>) -> PackageId {
        PackageId {
            id: id.into(),
            kind: None,
        }
    }

    /// A package id carrying a backend-specific kind.
    pub fn with_kind(id: impl Into<String>, kind: impl Into<String>) -> PackageId {
        PackageId {
            id: id.into(),
            kind: Some(kind.into()),
        }
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

/// A backend-facing reference to one package: its id and, where relevant, the
/// source it comes from. Passed to [`Index`](crate) queries and installer
/// operations; the backend itself is implied by which installer is called.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ref {
    /// The package id, as the backend names it.
    pub package: PackageId,
    /// The source it is installed from, when one is needed.
    pub source: Option<SourceId>,
}

impl Ref {
    /// A reference to a package with no explicit source.
    pub fn new(package: PackageId) -> Ref {
        Ref {
            package,
            source: None,
        }
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

/// What a backend reports after installing (§5.1): the package id it actually
/// installed, the version, and the scope it landed in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Installed {
    /// The package id as actually installed (§5.4).
    pub package: PackageId,
    /// The version now present.
    pub version: Version,
    /// The scope it landed in.
    pub scope: Scope,
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

    #[test]
    fn package_id_defaults_to_no_kind() {
        let p = PackageId::new("fd-find");
        assert_eq!(p.id, "fd-find");
        assert_eq!(p.kind, None);
    }

    #[test]
    fn package_id_can_carry_a_kind() {
        let p = PackageId::with_kind("font-fira-code", "cask");
        assert_eq!(p.id, "font-fira-code");
        assert_eq!(p.kind.as_deref(), Some("cask"));
        // Same id, different kind, are distinct packages.
        assert_ne!(p, PackageId::new("font-fira-code"));
    }

    #[test]
    fn ref_defaults_to_no_source() {
        let r = Ref::new(PackageId::new("jq"));
        assert_eq!(r.source, None);
        assert_eq!(r.package.id, "jq");
    }

    #[test]
    fn backend_and_source_ids_expose_str() {
        assert_eq!(BackendId::new("apt").as_str(), "apt");
        assert_eq!(SourceId::new("ctxloom/tap").as_str(), "ctxloom/tap");
    }
}
