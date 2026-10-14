//! The lock and machine-state shape (§4.5, §4.6).
//!
//! The lockfile and the machine state are deliberately the *same* package
//! shape: a package has a logical name and a map from [`Target`] to a
//! per-target entry. One generic container, [`Package`], serves both; only
//!
//! - the **document header** ([`LockDocument`] vs [`StateDocument`]),
//! - the **package-level meta** (the lock carries the manifest's [`Spec`]; the
//!   state carries `()` — each claim carries its own spec instead), and
//! - the **per-target leaf** ([`LockLeaf`] vs [`StateLeaf`])
//!
//! differ. Everything a target entry has in common — which backend, which
//! package id, which source — lives in [`Placement`], embedded in both
//! leaves, so a lock entry and a state entry can be compared field by field.
//!
//! Targets are held in a [`BTreeMap`] so iteration is sorted: deterministic
//! lock diffs and stable Docker layer caching (§4.5). The JSON wire format is
//! bosun-io's concern (slice 5); this module only fixes the Rust shape.

use std::collections::BTreeMap;

use crate::model::{BackendId, Checksum, Claim, Origin, PackageId, SourceId};
use crate::platform::Target;
use crate::spec::Spec;
use crate::version::Version;

/// A package keyed by target, generic over its package-level meta `M` and its
/// per-target leaf `L`. Instantiated as [`LockPackage`] and [`StatePackage`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Package<M, L> {
    /// bosun's logical name for the tool (§5.4).
    pub name: String,
    /// Package-level metadata: the [`Spec`] for a lock, `()` for state.
    pub meta: M,
    /// One entry per target, sorted by target for deterministic output.
    pub targets: BTreeMap<Target, L>,
}

/// What every per-target entry shares: which backend installed/would install
/// the package, under which package id, from which source (§4.5, §4.6).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Placement {
    /// The backend for this target.
    pub backend: BackendId,
    /// The package id as the backend names it (§5.4).
    pub package: PackageId,
    /// The source it comes from, when one is needed (§9).
    pub source: Option<SourceId>,
}

/// A lockfile per-target leaf (§4.5): the resolved version and its provenance.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LockLeaf {
    /// Backend / package / source for this target.
    pub placement: Placement,
    /// A per-target spec override, where this platform's version string
    /// differs from the package-level spec (§4.5).
    pub spec: Option<Spec>,
    /// The exact version selected for this target.
    pub resolved: Version,
    /// For repo-bound backends: when the index it resolved against was
    /// fetched (§4.5). Opaque timestamp text for M1.
    pub index_snapshot: Option<String>,
    /// The checksum the backend reported, when any (§4.5).
    pub checksum: Option<Checksum>,
}

/// A machine-state per-target leaf (§4.6): what is installed and who claims it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StateLeaf {
    /// Backend / package / source as actually installed.
    pub placement: Placement,
    /// The version actually present.
    pub installed: Version,
    /// How it came to be present (§4.6).
    pub origin: Origin,
    /// Every manifest that claims this package, with the spec it asked for.
    pub claimed_by: Vec<Claim>,
}

/// A lockfile package: package-level [`Spec`] plus a [`LockLeaf`] per target.
pub type LockPackage = Package<Spec, LockLeaf>;

/// A machine-state package: no package-level meta plus a [`StateLeaf`] per
/// target.
pub type StatePackage = Package<(), StateLeaf>;

/// Per-target lock header entry (§4.5): the hash of the parsed manifest this
/// target was resolved against, so editing a comment does not invalidate it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TargetLock {
    /// Hash of the parsed (comment-stripped) manifest for this target.
    pub manifest_hash: String,
}

/// The whole lockfile (§4.5): one file covering every declared target.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LockDocument {
    /// Format version; bosun refuses a newer major it cannot read.
    pub lockfile_version: u32,
    /// Per-target manifest hashes, sorted by target.
    pub targets: BTreeMap<Target, TargetLock>,
    /// The locked packages.
    pub packages: Vec<LockPackage>,
}

/// The whole machine-state file (§4.6): what bosun installed for one scope.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StateDocument {
    /// Format version.
    pub state_version: u32,
    /// The recorded packages.
    pub packages: Vec<StatePackage>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Scope;
    use crate::platform::{Arch, Os};
    use crate::version::Scheme;
    use std::path::PathBuf;

    fn target(os: Os, arch: Arch) -> Target {
        Target::Platform { os, arch }
    }

    fn ver(raw: &str, scheme: Scheme) -> Version {
        Version::parse(raw, scheme).unwrap()
    }

    fn placement() -> Placement {
        Placement {
            backend: BackendId::new("apt"),
            package: PackageId::new("jq"),
            source: None,
        }
    }

    #[test]
    fn lock_and_state_share_the_package_container() {
        // The same generic container serves both, keyed by the same target.
        let t = target(Os::Linux, Arch::Amd64);

        let lock: LockPackage = Package {
            name: "jq".into(),
            meta: Spec::new(">=1.7.1|<1.7.2~", "deb").unwrap(),
            targets: BTreeMap::from([(
                t,
                LockLeaf {
                    placement: placement(),
                    spec: None,
                    resolved: ver("1.7.1-3ubuntu0.1", Scheme::Debian),
                    index_snapshot: Some("2026-10-13T19:00:00Z".into()),
                    checksum: None,
                },
            )]),
        };

        let state: StatePackage = Package {
            name: "jq".into(),
            meta: (),
            targets: BTreeMap::from([(
                t,
                StateLeaf {
                    placement: placement(),
                    installed: ver("1.7.1-3ubuntu0.1", Scheme::Debian),
                    origin: Origin::Installed,
                    claimed_by: vec![Claim {
                        manifest: PathBuf::from("/w/verprop/bosun.yaml"),
                        spec: ">=1.7.1|<1.7.2~".into(),
                        user: None,
                    }],
                },
            )]),
        };

        // Same name, same target key, same placement — comparable field by
        // field, differing only in the leaf and package-level meta.
        assert_eq!(lock.name, state.name);
        assert_eq!(lock.targets[&t].placement, state.targets[&t].placement);
        assert_eq!(lock.targets[&t].resolved, state.targets[&t].installed);
        assert_eq!(lock.meta.canonical(), "vers:deb/>=1.7.1|<1.7.2~");
    }

    #[test]
    fn targets_iterate_sorted_for_deterministic_output() {
        let mut m: BTreeMap<Target, u8> = BTreeMap::new();
        m.insert(target(Os::Linux, Arch::Arm64), 1);
        m.insert(Target::Any, 2);
        m.insert(target(Os::Linux, Arch::Amd64), 3);
        let order: Vec<Target> = m.keys().copied().collect();
        // `any` sorts first, then platforms by (os, arch).
        assert_eq!(
            order,
            vec![
                Target::Any,
                target(Os::Linux, Arch::Amd64),
                target(Os::Linux, Arch::Arm64),
            ]
        );
    }

    #[test]
    fn documents_carry_their_version_headers() {
        let lock = LockDocument {
            lockfile_version: 1,
            targets: BTreeMap::from([(
                target(Os::Linux, Arch::Amd64),
                TargetLock {
                    manifest_hash: "sha256:9f2c".into(),
                },
            )]),
            packages: vec![],
        };
        let state = StateDocument {
            state_version: 1,
            packages: vec![],
        };
        assert_eq!(lock.lockfile_version, 1);
        assert_eq!(state.state_version, 1);
    }

    #[test]
    fn installed_records_scope() {
        let installed = crate::model::Installed {
            package: PackageId::new("ripgrep"),
            version: ver("14.1.1", Scheme::Semver),
            scope: Scope::User,
        };
        assert_eq!(installed.scope, Scope::User);
    }
}
