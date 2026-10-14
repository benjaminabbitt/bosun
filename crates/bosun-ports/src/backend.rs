//! The backend contract (§5.1, §5.3). Every package manager implements the
//! same two traits; adding a backend is one more adapter in bosun-backends,
//! never a change here.
//!
//! Lookup is split from mutation so the lockfile can be resolved for targets
//! other than the current machine (§4.7): [`Index`] (query side) may run
//! anywhere; [`Installer`] (mutation side) runs only on a matching host.

use bosun_core::{BackendId, Capabilities, Distro, Installed, Ref, Source, Target, Version};

use crate::error::PortError;
use crate::exec::CommandRunner;

/// Query side: "which versions exist for this target" (§5.1). Reads whatever
/// index is current; refresh is explicit (§9).
pub trait Index {
    /// Refresh this backend's index (`apt-get update`, `brew update`, …).
    fn refresh(&self) -> Result<(), PortError>;
    /// The versions available for `pkg` on `target`, newest-unspecified
    /// (callers order with bosun-core's `Version`).
    fn query_available(&self, pkg: &Ref, target: &Target) -> Result<Vec<Version>, PortError>;
}

/// Mutation side: install, verify, hold, downgrade, uninstall (§5.1). Runs on
/// the local machine only. [`Installed`] carries the scope the install landed
/// in (§4.6), which drives the privilege check and the choice of state file.
pub trait Installer {
    /// What this backend can honour (§5.2), derived from its probed version.
    fn capabilities(&self) -> Capabilities;
    /// Ensure a source (PPA/tap/bucket/registry) is present before install
    /// (§9).
    fn ensure_source(&self, source: &Source) -> Result<(), PortError>;
    /// Install exactly `version` of `pkg`, returning what landed and where.
    fn install_exact(&self, pkg: &Ref, version: &Version) -> Result<Installed, PortError>;
    /// The installed version of `pkg`, or `None` if it is not installed.
    /// Verification is by package id through the manager, never by binary
    /// name (§5.4).
    fn verify_installed(&self, pkg: &Ref) -> Result<Option<Version>, PortError>;
    /// Durably hold `pkg` at `version` (`apt-mark hold`, …).
    fn hold(&self, pkg: &Ref, version: &Version) -> Result<(), PortError>;
    /// Move `pkg` down to `version` (explicit; some backends refuse).
    fn downgrade(&self, pkg: &Ref, version: &Version) -> Result<Installed, PortError>;
    /// Uninstall `pkg`. Only ever called for the managed set (§9).
    fn uninstall(&self, pkg: &Ref) -> Result<(), PortError>;
}

/// What a backend learned about itself when it probed (§5.3): its own version,
/// recorded so flag behaviour can be gated per package-manager version later
/// (Appendix B #224). `Capabilities` are derived from this.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Probe {
    /// Which backend this probe is for.
    pub backend: BackendId,
    /// The package manager's own version, as it reported it.
    pub version: Version,
}

/// The machine facts and runner a probe sees (§5.3): enough to decide whether
/// the backend is present and applicable, plus the runner to ask it.
pub struct HostInfo<'a> {
    /// The operating system.
    pub os: bosun_core::Os,
    /// The CPU architecture.
    pub arch: bosun_core::Arch,
    /// Linux distro facts, when available (§5.4).
    pub distro: Option<&'a Distro>,
    /// The runner a probe uses to ask a tool its version.
    pub runner: &'a dyn CommandRunner,
}

/// A backend built from a successful probe: both sides of the contract for one
/// present backend. The query and mutation sides stay distinct trait objects
/// (§5.1); this only bundles the two the local registry hands out together.
pub struct Backend {
    /// Query side.
    pub index: Box<dyn Index>,
    /// Mutation side.
    pub installer: Box<dyn Installer>,
}

/// Registers a backend when it is present (§5.3). Every backend is compiled
/// in; only those whose probe succeeds are built for the run. `Send + Sync`
/// so all factories can be probed in parallel (§5.3).
pub trait BackendFactory: Send + Sync {
    /// The backend's stable id.
    fn id(&self) -> BackendId;
    /// Cheap presence check against the host; `Ok(None)` means not present.
    fn probe(&self, host: &HostInfo) -> Result<Option<Probe>, PortError>;
    /// Build the backend from a successful probe. Called only when present.
    fn build(&self, probe: Probe) -> Backend;
}
