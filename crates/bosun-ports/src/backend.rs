//! The backend contract (§5.1, §5.3). Every package manager implements the
//! same two traits; adding a backend is one more adapter in bosun-backends,
//! never a change here.
//!
//! Lookup is split from mutation so the lockfile can be resolved for targets
//! other than the current machine (§4.7): [`Index`] (query side) may run
//! anywhere; [`Installer`] (mutation side) runs only on a matching host.

use bosun_core::{
    BackendId, Capabilities, Checksum, Distro, Installed, Ref, Scope, Source, Target, Version, Want,
};

use crate::error::PortError;
use crate::exec::CommandRunner;
use crate::step::Steps;

/// Query side: "which versions exist for this target" (§5.1). Reads whatever
/// index is current; refresh is explicit (§9).
pub trait Index {
    /// Refresh this backend's index (`apt-get update`, `brew update`, …).
    fn refresh(&self) -> Result<(), PortError>;
    /// The versions available for `pkg` on `target`, newest-unspecified
    /// (callers order with bosun-core's `Version`).
    fn query_available(&self, pkg: &Ref, target: &Target) -> Result<Vec<Version>, PortError>;

    /// The version to install for `pkg` on `target` among those `want`
    /// admits, chosen as this backend's package manager would choose it
    /// (§5.6). `prefer` lists versions a lock already chose, best first; the
    /// first the manager still offers and `want` admits wins (lockfiles
    /// advise). The default takes [`Want::choose`] over
    /// [`query_available`](Index::query_available): the highest admissible
    /// version.
    fn select(
        &self,
        pkg: &Ref,
        target: &Target,
        want: &Want,
        prefer: &[Version],
    ) -> Result<Selection, PortError> {
        let available = self.query_available(pkg, target)?;
        Ok(Selection {
            chosen: want.choose(&available, prefer),
            available,
        })
    }

    /// The checksum to record for `pkg` at `version` on `target`, when the
    /// backend enforces one (§12). github-release reads it from the release's
    /// `checksums.txt`; backends with nothing to record (apt, cargo) keep the
    /// default `None`, so the lockfile carries a checksum only where §12 asks.
    fn checksum(
        &self,
        _pkg: &Ref,
        _version: &Version,
        _target: &Target,
    ) -> Result<Option<Checksum>, PortError> {
        Ok(None)
    }
}

/// What [`Index::select`] found: the version to install, or `None` when
/// nothing offered is wanted, and the versions it considered, for the
/// diagnostic that names them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Selection {
    /// The version to install.
    pub chosen: Option<Version>,
    /// The versions considered, in no particular order.
    pub available: Vec<Version>,
}

/// Mutation side: install, verify, hold, downgrade, uninstall (§5.1). Runs on
/// the local machine only.
///
/// The mutating methods **describe** their work as a [`Steps`] list; a host
/// executor (or the container renderer) carries it out (§5.1). They do not act
/// directly, so the plan can show the exact commands and one description drives
/// every driver. Queries (`capabilities`, `verify_installed`) run directly.
pub trait Installer {
    /// What this backend can honour (§5.2), derived from its probed version.
    fn capabilities(&self) -> Capabilities;
    /// The install scope this backend lands in (§4.6), which drives the
    /// privilege check and the choice of state file. Fixed per backend in M1
    /// (apt → system, cargo/github-release → user).
    fn scope(&self) -> Scope;
    /// Steps to ensure a source (PPA/tap/bucket/registry) is present (§9).
    fn ensure_source(&self, source: &Source) -> Result<Steps, PortError>;
    /// Steps to install exactly `version` of `pkg`, leaving it unheld. The
    /// version that actually landed is read back with `verify_installed` after
    /// the steps run (Appendix B #229/#130).
    fn install_exact(&self, pkg: &Ref, version: &Version) -> Result<Steps, PortError>;
    /// Whether `pkg` is installed and, when readable, its version (§5.1). A
    /// direct read, by package id through the manager, never by binary name
    /// (§5.4). No sentinel version — see [`Installed`].
    fn verify_installed(&self, pkg: &Ref) -> Result<Installed, PortError>;
    /// Steps to durably hold `pkg` at `version` (`apt-mark hold`, …). bosun
    /// holds only a package whose spec is exact (§5.2).
    fn hold(&self, pkg: &Ref, version: &Version) -> Result<Steps, PortError>;
    /// Steps to release the hold [`hold`](Installer::hold) put on `pkg`, so
    /// its package manager may move it again (`apt-mark unhold`, …).
    fn unhold(&self, pkg: &Ref) -> Result<Steps, PortError>;
    /// Steps to move `pkg` down to `version` (explicit; some backends refuse).
    fn downgrade(&self, pkg: &Ref, version: &Version) -> Result<Steps, PortError>;
    /// Steps to uninstall `pkg`. Only ever called for the managed set (§9).
    fn uninstall(&self, pkg: &Ref) -> Result<Steps, PortError>;

    /// What the user installed through this backend themselves, by the
    /// package manager's own record (§9 state export): `apt-mark showmanual`,
    /// `cargo install --list`. Never the full installed set, and never a
    /// guess at which packages are dependencies. A backend with no such
    /// record keeps the default, [`UserInstalls::Unsupported`].
    fn user_installs(&self) -> Result<UserInstalls, PortError> {
        Ok(UserInstalls::Unsupported)
    }
}

/// One package a user installed themselves, as the backend records it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UserInstall {
    /// The backend's own id for the package (`fd-find`, `ripgrep`).
    pub package: String,
    /// The version installed now.
    pub version: Version,
}

/// Where a backend's record of what the user installed lives (§9).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UserInstalls {
    /// The backend keeps no record of what the user asked for: no export.
    Unsupported,
    /// The packages the package manager records as installed by the user.
    Listed(Vec<UserInstall>),
    /// bosun is this backend's installer (the GitHub release backends), so its
    /// record is bosun's own machine state.
    BosunState,
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
/// Static metadata the resolver needs about a backend (§5.6): the purl type for
/// identity, the vers type for version comparison (§6), and whether it resolves
/// under `any` (§4.7). Fixed per backend, independent of any probe, so the
/// resolver can place a backend without a lookup table elsewhere (§5.1: adding
/// a backend is one more impl, never a change in the CLI).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BackendMeta {
    /// The purl type for identity (`deb`, `cargo`, `github`).
    pub purl_type: &'static str,
    /// The vers type versions compare under (`deb`, `cargo`, `semver`).
    pub vers_type: &'static str,
    /// Whether it resolves once under `any` (cargo, go, npm, …).
    pub platform_independent: bool,
    /// The options this backend reads from its entry in an install block's
    /// `backends` map (§5.4), besides bosun's own `package`; any other key there
    /// is a manifest error. The values reach
    /// the backend as purl qualifiers, and it validates them when it reads them.
    pub option_keys: &'static [&'static str],
}

/// in; only those whose probe succeeds are built for the run. `Send + Sync`
/// so all factories can be probed in parallel (§5.3).
pub trait BackendFactory: Send + Sync {
    /// The backend's stable id.
    fn id(&self) -> BackendId;
    /// The resolver metadata for this backend (§5.6), fixed and probe-independent.
    fn meta(&self) -> BackendMeta;
    /// Cheap presence check against the host; `Ok(None)` means not present.
    fn probe(&self, host: &HostInfo) -> Result<Option<Probe>, PortError>;
    /// Build the backend from a successful probe. Called only when present.
    fn build(&self, probe: Probe) -> Backend;

    /// The query side for a machine bosun is not running on (§4.7): an index
    /// that reads the backend's registry or mirror over HTTP, for the platform
    /// this factory was made for, with no probe and no local program. `None`
    /// when the backend can only ask a local package manager.
    fn remote_index(&self) -> Option<Box<dyn Index>> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bosun_core::{Scheme, VersRange};

    struct Listing(Vec<&'static str>);

    impl Index for Listing {
        fn refresh(&self) -> Result<(), PortError> {
            Ok(())
        }
        fn query_available(&self, _: &Ref, _: &Target) -> Result<Vec<Version>, PortError> {
            Ok(self.0.iter().map(|v| sem(v)).collect())
        }
    }

    fn sem(v: &str) -> Version {
        Version::parse(v, Scheme::Semver).unwrap()
    }

    fn select(listing: &[&'static str], range: &str, prefer: &[&str]) -> Selection {
        let pkg = Ref::new("pkg:cargo/ripgrep".parse().unwrap());
        let want = Want::of(VersRange::parse(range).unwrap());
        let prefer: Vec<Version> = prefer.iter().map(|v| sem(v)).collect();
        Listing(listing.to_vec())
            .select(&pkg, &Target::Any, &want, &prefer)
            .unwrap()
    }

    #[test]
    fn the_default_selection_is_the_highest_wanted_version_listed() {
        let got = select(&["14.0.0", "14.1.1", "15.0.0"], "vers:cargo/<15.0.0", &[]);
        assert_eq!(got.chosen, Some(sem("14.1.1")));
        assert_eq!(
            got.available,
            vec![sem("14.0.0"), sem("14.1.1"), sem("15.0.0")]
        );
    }

    #[test]
    fn the_default_selection_keeps_a_listed_locked_version() {
        let got = select(&["14.0.0", "14.1.1"], "vers:cargo/<15.0.0", &["14.0.0"]);
        assert_eq!(got.chosen, Some(sem("14.0.0")));
        let none = select(&["14.0.0"], "vers:cargo/>=15.0.0", &[]);
        assert_eq!(none.chosen, None);
    }
}
