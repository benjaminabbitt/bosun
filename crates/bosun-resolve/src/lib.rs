//! bosun resolver (§4.7, §5.3, §5.6, §7): turn a manifest plus `Index` queries
//! into resolved versions and a lockfile for this run's targets.
//!
//! Resolution is pure and port-driven: it reads a [`Manifest`], the host facts,
//! and a set of registered backends (each an [`Index`] plus the metadata needed
//! to build identities and pick a target key), and never touches the disk or
//! the network itself. The wiring layer builds the backend list from the live
//! registry (bosun-host); tests supply fakes.
//!
//! For each package entry it: picks the most specific applicable `install`
//! block (exact distro ID > ID_LIKE > plain OS > `defaults.install`, §5.4);
//! chooses a backend by installer preference (flag > env > config > manifest
//! order, a single explicit backend never overridden, §5.3); builds the
//! canonical purl from the backend type and host facts (§5.6); queries the
//! backend index; and selects the highest version in the spec's vers set (§7).
//!
//! A platform-independent backend (cargo, go, npm, …) resolves under the `any`
//! target; every other backend under the current platform (§4.7). [`resolve`]
//! covers only the targets resolved this run; [`merge_lock`] folds that into an
//! existing multi-platform `bosun.lock` per target key, and [`check_targets`]
//! reports declared targets that are missing or stale (`lock --check`).
//!
//! Where one install scope holds a package claimed by several manifests (§4.6),
//! [`select_across_claims`] intersects every claim's vers set — refusing when
//! they disagree — and [`frozen_select`] holds a claim to its locked version,
//! refusing a clash (`install --frozen`, §7).

mod errmsg;

use std::collections::{BTreeMap, BTreeSet};

use bosun_core::manifest::{Ext, InstallBlock, Manifest, ManifestEntry};
use bosun_core::platform::{Arch, Distro, Os, Selector, Target};
use bosun_core::record::{LockDocument, LockLeaf, LockPackage, Package, Placement, TargetLock};
use bosun_core::spec::{SelectError, Spec, SpecError};
use bosun_core::{BackendId, Claim, Purl, PurlError, Ref, Version};
use bosun_ports::backend::Index;
use bosun_ports::error::PortError;

/// The lockfile format version this resolver writes (§4.5).
const LOCKFILE_VERSION: u32 = 1;

/// The host facts resolution matches selectors and builds identities against.
/// A narrow view of the `Machine` port: resolution needs only these, never the
/// privilege or state-path methods, so it stays pure and trivially faked.
pub struct Host<'a> {
    /// The operating system.
    pub os: Os,
    /// The CPU architecture.
    pub arch: Arch,
    /// Linux distro facts, when available (§5.4).
    pub distro: Option<&'a Distro>,
}

/// A registered backend the resolver can query, with the metadata it needs to
/// build an identity and choose a target key (§5.3, §5.6). The wiring layer
/// fills these from each backend adapter; the resolver itself holds no backend
/// knowledge.
pub struct ResolvableBackend<'a> {
    /// The backend's stable id (`apt`, `cargo`, `github-release`).
    pub id: BackendId,
    /// The purl type for identity (`deb`, `cargo`, `github`), §5.6.
    pub purl_type: String,
    /// The vers type versions compare under (§6). Usually the purl type, but
    /// `github` tags compare as `semver` though their purl type is `github`.
    pub vers_type: String,
    /// Platform-independent backends (cargo, go, npm, …) resolve once under the
    /// `any` target; the rest under the current platform (§4.7).
    pub platform_independent: bool,
    /// The query side used to list available versions.
    pub index: &'a dyn Index,
}

/// The result of resolving one manifest: the lockfile and a per-entry report.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Resolution {
    /// The lockfile for the targets resolved this run.
    pub lock: LockDocument,
    /// What happened to each manifest entry, in file order.
    pub outcomes: Vec<EntryOutcome>,
}

/// What resolution decided for one entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EntryOutcome {
    /// The entry's logical name.
    pub name: String,
    /// The outcome.
    pub outcome: Outcome,
}

/// The outcome of resolving one entry. Only [`Outcome::Resolved`] contributes a
/// lockfile entry; the others are reported, never a crash (§5.3, §7).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Resolved to a concrete version via `backend` under `target`.
    Resolved {
        /// The target key the entry resolved under.
        target: Target,
        /// The backend that resolved it.
        backend: BackendId,
        /// The selected version.
        version: Version,
    },
    /// No install block applies here, or none names a registered backend.
    NotApplicable(NotApplicable),
    /// A backend applied but nothing available was in the spec's set (§7).
    Unsatisfiable {
        /// The spec, in canonical vers form.
        spec: String,
        /// The versions that were considered.
        available: String,
    },
}

/// Why an entry is not applicable on this host (§5.3). Reported, not fatal.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NotApplicable {
    /// No `install` selector (and no `defaults`) matches this host.
    NoSelector,
    /// A block matched but named no backend to install with.
    NoBackend,
    /// The named backends are none of them registered here.
    NoRegisteredBackend {
        /// The backends the block named, in manifest order.
        candidates: Vec<BackendId>,
    },
}

/// A hard resolution failure: the manifest or a backend is broken in a way that
/// stops a lockfile being produced. Per-entry "not here" and "no version in the
/// set" are [`Outcome`]s, not errors.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ResolveError {
    /// An applicable entry has no spec to resolve.
    #[error("{kind}: {name}", kind = errmsg::MISSING_SPEC)]
    MissingSpec {
        /// The entry's logical name.
        name: String,
    },
    /// A spec string could not be parsed as a vers set.
    #[error("{kind}: {name}: {source}", kind = errmsg::BAD_SPEC)]
    BadSpec {
        /// The entry's logical name.
        name: String,
        /// The underlying spec error.
        #[source]
        source: SpecError,
    },
    /// The identity purl could not be built.
    #[error("{kind}: {name}: {source}", kind = errmsg::BAD_PURL)]
    BadPurl {
        /// The entry's logical name.
        name: String,
        /// The underlying purl error.
        #[source]
        source: PurlError,
    },
    /// A deb identity needs distro facts this host lacks.
    #[error("{kind}: {name}", kind = errmsg::NO_DISTRO)]
    NoDistro {
        /// The entry's logical name.
        name: String,
    },
    /// A github package id was not of the form `owner/repo`.
    #[error("{kind}: {name}: {value:?}", kind = errmsg::BAD_GITHUB_ID)]
    BadGithubId {
        /// The entry's logical name.
        name: String,
        /// The id as written.
        value: String,
    },
    /// An index query failed.
    #[error("{kind}: {name}: {source}", kind = errmsg::QUERY_FAILED)]
    Query {
        /// The entry's logical name.
        name: String,
        /// The underlying port error.
        #[source]
        source: PortError,
    },
}

/// Resolve `manifest` against `host` and the registered `backends`, producing a
/// lockfile and a per-entry report (§4.7). `manifest_hash` is the parsed-model
/// hash the targets record (computed by the caller via bosun-io, so the
/// resolver stays free of file concerns, §4.5). `preference` is the merged
/// installer preference (flag then env then config, §5.3); the manifest's own
/// order is appended per entry.
pub fn resolve(
    manifest: &Manifest,
    manifest_hash: &str,
    host: &Host<'_>,
    backends: &[ResolvableBackend<'_>],
    preference: &[BackendId],
) -> Result<Resolution, ResolveError> {
    let mut outcomes = Vec::with_capacity(manifest.packages.len());
    let mut packages = Vec::new();
    let mut targets: BTreeMap<Target, TargetLock> = BTreeMap::new();

    for entry in &manifest.packages {
        let (outcome, resolved) = resolve_entry(entry, manifest, host, backends, preference)?;
        if let Some((target, package)) = resolved {
            targets.entry(target).or_insert_with(|| TargetLock {
                manifest_hash: manifest_hash.to_string(),
            });
            packages.push(package);
        }
        outcomes.push(EntryOutcome {
            name: entry.name.clone(),
            outcome,
        });
    }

    Ok(Resolution {
        lock: LockDocument {
            lockfile_version: LOCKFILE_VERSION,
            targets,
            packages,
        },
        outcomes,
    })
}

/// Resolve a single entry. Returns its outcome and, when resolved, the target
/// key and lockfile package to record.
fn resolve_entry(
    entry: &ManifestEntry,
    manifest: &Manifest,
    host: &Host<'_>,
    backends: &[ResolvableBackend<'_>],
    preference: &[BackendId],
) -> Result<(Outcome, Option<(Target, LockPackage)>), ResolveError> {
    // §5.4: the most specific install block that applies here, defaults merged.
    let Some(block) = effective_block(entry, manifest, host) else {
        return Ok((Outcome::NotApplicable(NotApplicable::NoSelector), None));
    };
    if block.backends.is_empty() {
        return Ok((Outcome::NotApplicable(NotApplicable::NoBackend), None));
    }
    // §5.3: first candidate that is registered and highest in preference.
    let Some(chosen) = choose_backend(&block.backends, backends, preference) else {
        return Ok((
            Outcome::NotApplicable(NotApplicable::NoRegisteredBackend {
                candidates: block.backends.iter().map(BackendId::new).collect(),
            }),
            None,
        ));
    };

    // §5.6: parse the spec under the backend's vers type; build the identity.
    let raw_spec = block
        .spec
        .as_deref()
        .or(entry.spec.as_deref())
        .ok_or_else(|| ResolveError::MissingSpec {
            name: entry.name.clone(),
        })?;
    let spec = Spec::new(raw_spec, &chosen.vers_type).map_err(|source| ResolveError::BadSpec {
        name: entry.name.clone(),
        source,
    })?;
    let native = block.package.as_deref().unwrap_or(&entry.name);
    let purl = build_purl(
        &entry.name,
        &chosen.purl_type,
        native,
        block.ext.get(chosen.id.as_str()),
        host,
    )?;

    // §4.7: platform-independent backends resolve under `any`.
    let target = if chosen.platform_independent {
        Target::Any
    } else {
        Target::Platform {
            os: host.os,
            arch: host.arch,
        }
    };

    // §7: highest available version in the spec's set.
    let available = chosen
        .index
        .query_available(&Ref::new(purl.clone()), &target)
        .map_err(|source| ResolveError::Query {
            name: entry.name.clone(),
            source,
        })?;
    let version = match spec.select(&available) {
        Ok(version) => version,
        Err(SelectError::Unsatisfiable { spec, available }) => {
            return Ok((Outcome::Unsatisfiable { spec, available }, None));
        }
    };

    // §12: record the backend's enforced checksum when it has one (github-release
    // reads it from the release's checksums.txt; apt/cargo have none).
    let checksum = chosen
        .index
        .checksum(&Ref::new(purl.clone()), &version, &target)
        .map_err(|source| ResolveError::Query {
            name: entry.name.clone(),
            source,
        })?;

    let package: LockPackage = Package {
        name: entry.name.clone(),
        meta: spec,
        targets: BTreeMap::from([(
            target,
            LockLeaf {
                placement: Placement {
                    backend: chosen.id.clone(),
                    purl,
                },
                spec: None,
                resolved: version.clone(),
                index_snapshot: None,
                checksum,
            },
        )]),
    };
    Ok((
        Outcome::Resolved {
            target,
            backend: chosen.id.clone(),
            version,
        },
        Some((target, package)),
    ))
}

/// The effective install block for `entry` on `host`: the most specific block
/// in `entry.install` merged over the most specific in `defaults.install`
/// (entry fields win, missing fields inherit). `None` when neither declares a
/// block that applies to this host.
fn effective_block(
    entry: &ManifestEntry,
    manifest: &Manifest,
    host: &Host<'_>,
) -> Option<InstallBlock> {
    let from_defaults = most_specific(&manifest.defaults, host);
    let from_entry = most_specific(&entry.install, host);
    match (from_defaults, from_entry) {
        (None, None) => None,
        (Some(block), None) | (None, Some(block)) => Some(block),
        (Some(defaults), Some(entry)) => Some(InstallBlock {
            backends: if entry.backends.is_empty() {
                defaults.backends
            } else {
                entry.backends
            },
            package: entry.package.or(defaults.package),
            spec: entry.spec.or(defaults.spec),
            ext: merge_ext(defaults.ext, entry.ext),
        }),
    }
}

/// `entry`'s ext settings over `defaults`', per backend and per key: a key the
/// entry sets wins, the rest inherit.
fn merge_ext(mut defaults: Ext, entry: Ext) -> Ext {
    for (backend, settings) in entry {
        defaults.entry(backend).or_default().extend(settings);
    }
    defaults
}

/// The most specific selector in `map` that applies to `host`, by the §5.4
/// ladder (exact distro ID > ID_LIKE > plain OS), or `None` if none apply.
fn most_specific(map: &BTreeMap<Selector, InstallBlock>, host: &Host<'_>) -> Option<InstallBlock> {
    map.iter()
        .filter_map(|(sel, block)| specificity(sel, host).map(|rank| (rank, block)))
        .max_by_key(|(rank, _)| *rank)
        .map(|(_, block)| block.clone())
}

/// How specifically `sel` applies to `host`: higher is more specific. `None`
/// when it does not apply (wrong OS, or a distro family the host is not in).
fn specificity(sel: &Selector, host: &Host<'_>) -> Option<u8> {
    if sel.os() != host.os {
        return None;
    }
    match sel.distro() {
        None => Some(1), // plain OS
        Some(family) => {
            let distro = host.distro?;
            if distro.id == family {
                Some(3) // exact ID
            } else if distro.id_like.iter().any(|f| f == family) {
                Some(2) // ID_LIKE family
            } else {
                None
            }
        }
    }
}

/// Choose a backend among `candidates` (manifest order), restricted to the
/// `registered` set, by installer preference (§5.3). A single explicit
/// candidate is used as-is and never reordered; otherwise the registered
/// candidate highest in `preference` wins, falling back to manifest order when
/// none appear in the preference list. `None` when no candidate is registered.
fn choose_backend<'b>(
    candidates: &[String],
    registered: &'b [ResolvableBackend<'b>],
    preference: &[BackendId],
) -> Option<&'b ResolvableBackend<'b>> {
    let available: Vec<&ResolvableBackend> = candidates
        .iter()
        .filter_map(|c| registered.iter().find(|b| b.id.as_str() == c))
        .collect();
    if candidates.len() == 1 {
        // A single explicit backend is never overridden by preference (§5.3).
        return available.into_iter().next();
    }
    available
        .iter()
        .copied()
        .filter_map(|b| {
            preference
                .iter()
                .position(|p| *p == b.id)
                .map(|pos| (pos, b))
        })
        .min_by_key(|(pos, _)| *pos)
        .map(|(_, b)| b)
        .or_else(|| available.into_iter().next())
}

/// Build the unversioned canonical identity purl for a backend (§5.6):
///
/// - `deb`: `pkg:deb/<distro-id>/<name>?arch=<arch>&distro=<codename>` — the
///   namespace is the distro ID and the qualifiers carry arch and the distro
///   codename (from `VERSION_CODENAME`), omitted when the host has none.
/// - `github`: `pkg:github/<owner>/<repo>?binary=<logical>` — the package id
///   is `owner/repo`; the `binary` qualifier defaults to the entry's logical
///   name so `- name: ltk` with `package: owner/repo` selects the `ltk` binary.
/// - anything else: `pkg:<type>/<name>` (cargo, generic, …).
///
/// The chosen backend's `ext` settings (`ext`, already narrowed to that
/// backend) are added as qualifiers, verbatim, for the backend to read and
/// validate (github-release's `asset` template). Another backend's section
/// never reaches this identity. The qualifiers above are set after them, so an
/// ext key can't replace one.
fn build_purl(
    name: &str,
    purl_type: &str,
    native: &str,
    ext: Option<&BTreeMap<String, String>>,
    host: &Host<'_>,
) -> Result<Purl, ResolveError> {
    let mut qualifiers = ext.cloned().unwrap_or_default();
    let bad_purl = |source| ResolveError::BadPurl {
        name: name.to_string(),
        source,
    };
    match purl_type {
        "deb" => {
            let distro = host.distro.ok_or_else(|| ResolveError::NoDistro {
                name: name.to_string(),
            })?;
            qualifiers.insert("arch".to_string(), host.arch.to_string());
            if let Some(codename) = &distro.version_codename {
                qualifiers.insert("distro".to_string(), codename.clone());
            }
            Purl::from_components("deb", Some(&distro.id), native, None, qualifiers, None)
                .map_err(bad_purl)
        }
        "github" => {
            let (owner, repo) =
                native
                    .split_once('/')
                    .ok_or_else(|| ResolveError::BadGithubId {
                        name: name.to_string(),
                        value: native.to_string(),
                    })?;
            qualifiers.insert("binary".to_string(), name.to_string());
            Purl::from_components("github", Some(owner), repo, None, qualifiers, None)
                .map_err(bad_purl)
        }
        other => {
            Purl::from_components(other, None, native, None, qualifiers, None).map_err(bad_purl)
        }
    }
}

// --- multi-claim resolution (§4.6) and frozen install (§7) -----------------

/// Why resolving a package across several claims failed (§4.6, §7).
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ClaimError {
    /// A claim's spec string could not be parsed as a vers set.
    #[error("{kind}: {manifest}: {source}", kind = errmsg::BAD_CLAIM_SPEC)]
    BadSpec {
        /// The manifest the claim came from.
        manifest: String,
        /// The underlying spec error.
        #[source]
        source: SpecError,
    },
    /// No available version satisfies every claim: the specs disagree (§4.6).
    /// Refused rather than flip-flopped; the message names each manifest and
    /// spec and the versions that were available.
    #[error("{kind}: {claims} (available: {available})", kind = errmsg::EMPTY_INTERSECTION)]
    EmptyIntersection {
        /// Each claim as `manifest=spec`, comma-separated.
        claims: String,
        /// The versions that were considered.
        available: String,
    },
    /// A lockfile's pinned version is excluded by another claim's spec, so
    /// `--frozen` cannot honour the lock without breaking that claim (§7).
    #[error("{kind}: {locked} excluded by {clashes}", kind = errmsg::FROZEN_CLASH)]
    FrozenClash {
        /// The locked version held by `--frozen`.
        locked: String,
        /// The clashing claims as `manifest=spec`, comma-separated.
        clashes: String,
    },
}

/// Parse every claim's spec under `vers_type` (a claim carries its own
/// `vers:<type>/` prefix when it has one; §5.6), keeping claim and spec aligned.
fn parse_claims(claims: &[Claim], vers_type: &str) -> Result<Vec<Spec>, ClaimError> {
    claims
        .iter()
        .map(|c| {
            Spec::new(&c.spec, vers_type).map_err(|source| ClaimError::BadSpec {
                manifest: c.manifest.display().to_string(),
                source,
            })
        })
        .collect()
}

/// Render claims as `manifest=spec, …` for a diagnostic.
fn claims_summary<'a>(claims: impl IntoIterator<Item = &'a Claim>) -> String {
    claims
        .into_iter()
        .map(|c| format!("{}={}", c.manifest.display(), c.spec))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Render available versions as `a, b, …` for a diagnostic.
fn versions_summary(available: &[Version]) -> String {
    available
        .iter()
        .map(Version::as_str)
        .collect::<Vec<_>>()
        .join(", ")
}

/// Select one version for a package claimed by several manifests in one install
/// scope (§4.6). One scope holds one version, so every claim's vers set must
/// admit it; the highest available version in the intersection wins, except a
/// version some lockfile already chose is preferred when every claim admits it
/// (`lock_preference`, the current project's lock first — lockfiles advise,
/// specs decide). Claims from other OS users in system state are just more
/// claims here. Refuses with [`ClaimError::EmptyIntersection`] when no available
/// version satisfies them all, naming each manifest and spec.
pub fn select_across_claims(
    claims: &[Claim],
    vers_type: &str,
    available: &[Version],
    lock_preference: &[Version],
) -> Result<Version, ClaimError> {
    let specs = parse_claims(claims, vers_type)?;
    // The §7 pre-release guard, across claims: admit pre-releases only when some
    // claim's range names one.
    let allow_pre = specs.iter().any(|s| s.range().mentions_prerelease());
    let admissible = |v: &Version| {
        (allow_pre || !v.is_prerelease()) && specs.iter().all(|s| s.range().contains(v))
    };

    // Lockfiles advise: hold to a lock-chosen version every claim admits, the
    // current project's first, so switching projects does not flip the install.
    for preferred in lock_preference {
        if available.contains(preferred) && admissible(preferred) {
            return Ok(preferred.clone());
        }
    }
    // Specs decide: otherwise the highest available version in the intersection.
    available
        .iter()
        .filter(|v| admissible(v))
        .max()
        .cloned()
        .ok_or_else(|| ClaimError::EmptyIntersection {
            claims: claims_summary(claims),
            available: versions_summary(available),
        })
}

/// Hold a package to the version its current-project lockfile pinned
/// (`install --frozen`, §7): no re-resolution, but still refuse when another
/// claim's spec excludes that exact version, naming the clashing claims.
pub fn frozen_select(
    locked: &Version,
    claims: &[Claim],
    vers_type: &str,
) -> Result<Version, ClaimError> {
    let specs = parse_claims(claims, vers_type)?;
    let clashes: Vec<&Claim> = claims
        .iter()
        .zip(&specs)
        .filter(|(_, spec)| !spec.range().contains(locked))
        .map(|(claim, _)| claim)
        .collect();
    if clashes.is_empty() {
        Ok(locked.clone())
    } else {
        Err(ClaimError::FrozenClash {
            locked: locked.as_str().to_string(),
            clashes: claims_summary(clashes),
        })
    }
}

// --- lock merge and freshness (§4.5, §4.7) ---------------------------------

/// Fold a freshly resolved lockfile into an existing one per target key (§4.7):
/// the targets `fresh` resolved are replaced wholesale (their headers and all
/// their package leaves), and every other target in `existing` is left
/// untouched, so one platform's `bosun lock` never disturbs another's. A
/// package whose only leaves were at re-resolved targets and is absent from
/// `fresh` is dropped from those targets.
pub fn merge_lock(existing: &LockDocument, fresh: &LockDocument) -> LockDocument {
    let fresh_targets: BTreeSet<Target> = fresh.targets.keys().copied().collect();

    // Target headers: fresh overwrites its own keys; the rest stay (§4.7).
    let mut targets = existing.targets.clone();
    for (target, header) in &fresh.targets {
        targets.insert(*target, header.clone());
    }

    // Package names in a stable order: existing first, then new ones from fresh.
    let mut names: Vec<&str> = Vec::new();
    for package in existing.packages.iter().chain(&fresh.packages) {
        if !names.contains(&package.name.as_str()) {
            names.push(&package.name);
        }
    }
    let find =
        |packages: &[LockPackage], name: &str| packages.iter().find(|p| p.name == name).cloned();

    let mut packages = Vec::new();
    for name in names {
        let existing_pkg = find(&existing.packages, name);
        let fresh_pkg = find(&fresh.packages, name);
        let mut leaves: BTreeMap<Target, LockLeaf> = BTreeMap::new();
        // Keep existing leaves at targets this run did not re-resolve.
        if let Some(package) = &existing_pkg {
            for (target, leaf) in &package.targets {
                if !fresh_targets.contains(target) {
                    leaves.insert(*target, leaf.clone());
                }
            }
        }
        // Overlay the freshly resolved leaves (all at fresh target keys).
        if let Some(package) = &fresh_pkg {
            for (target, leaf) in &package.targets {
                leaves.insert(*target, leaf.clone());
            }
        }
        if leaves.is_empty() {
            continue; // re-resolved away for every target it held
        }
        // Package-level spec follows the fresh resolution when present.
        let meta = fresh_pkg
            .or(existing_pkg)
            .map(|p| p.meta)
            .expect("a contributing package exists");
        packages.push(Package {
            name: name.to_string(),
            meta,
            targets: leaves,
        });
    }

    LockDocument {
        lockfile_version: fresh.lockfile_version,
        targets,
        packages,
    }
}

/// Whether a declared target is recorded in the lock and current (§4.5).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Freshness {
    /// The target has no entry in the lock.
    Missing,
    /// The target is locked against a different manifest hash.
    Stale,
    /// The target is locked against the current manifest hash.
    Fresh,
}

/// One declared target's freshness against the lock.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TargetFreshness {
    /// The declared target.
    pub target: Target,
    /// Its freshness.
    pub freshness: Freshness,
}

/// Report each declared target's freshness against `lock` (`lock --check`,
/// §4.5): a target the lock omits is [`Freshness::Missing`]; one locked against
/// a different `manifest_hash` is [`Freshness::Stale`]. The command treats any
/// non-[`Freshness::Fresh`] target as a failure; this only classifies.
pub fn check_targets(
    declared: &[Target],
    manifest_hash: &str,
    lock: &LockDocument,
) -> Vec<TargetFreshness> {
    declared
        .iter()
        .map(|target| {
            let freshness = match lock.targets.get(target) {
                None => Freshness::Missing,
                Some(header) if header.manifest_hash == manifest_hash => Freshness::Fresh,
                Some(_) => Freshness::Stale,
            };
            TargetFreshness {
                target: *target,
                freshness,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    use bosun_core::version::Scheme;

    /// An index that returns a fixed version list and records the last query.
    struct FakeIndex {
        versions: Vec<Version>,
        last: RefCell<Option<(Ref, Target)>>,
    }

    impl FakeIndex {
        fn new(scheme: Scheme, versions: &[&str]) -> FakeIndex {
            FakeIndex {
                versions: versions
                    .iter()
                    .map(|v| Version::parse(*v, scheme).unwrap())
                    .collect(),
                last: RefCell::new(None),
            }
        }
    }

    impl Index for FakeIndex {
        fn refresh(&self) -> Result<(), PortError> {
            Ok(())
        }
        fn query_available(&self, pkg: &Ref, target: &Target) -> Result<Vec<Version>, PortError> {
            *self.last.borrow_mut() = Some((pkg.clone(), *target));
            Ok(self.versions.clone())
        }
    }

    /// An index whose query always fails.
    struct FailIndex;
    impl Index for FailIndex {
        fn refresh(&self) -> Result<(), PortError> {
            Ok(())
        }
        fn query_available(&self, _pkg: &Ref, _target: &Target) -> Result<Vec<Version>, PortError> {
            Err(PortError::Backend {
                detail: "boom".to_string(),
            })
        }
    }

    fn sel(s: &str) -> Selector {
        s.parse().unwrap()
    }

    fn block(backends: &[&str], package: Option<&str>, spec: Option<&str>) -> InstallBlock {
        InstallBlock {
            backends: backends.iter().map(|s| s.to_string()).collect(),
            package: package.map(str::to_string),
            spec: spec.map(str::to_string),
            ext: Default::default(),
        }
    }

    fn with_ext(mut b: InstallBlock, backend: &str, key: &str, value: &str) -> InstallBlock {
        b.ext
            .entry(backend.to_string())
            .or_default()
            .insert(key.to_string(), value.to_string());
        b
    }

    fn with_asset(b: InstallBlock, asset: &str) -> InstallBlock {
        with_ext(b, "github-release", "asset", asset)
    }

    fn entry(name: &str, spec: Option<&str>, install: &[(&str, InstallBlock)]) -> ManifestEntry {
        ManifestEntry {
            name: name.to_string(),
            spec: spec.map(str::to_string),
            formerly: Vec::new(),
            install: install.iter().map(|(s, b)| (sel(s), b.clone())).collect(),
        }
    }

    fn manifest(defaults: &[(&str, InstallBlock)], packages: Vec<ManifestEntry>) -> Manifest {
        Manifest {
            version: 1,
            targets: Vec::new(),
            defaults: defaults.iter().map(|(s, b)| (sel(s), b.clone())).collect(),
            packages,
        }
    }

    fn debian() -> Distro {
        Distro {
            id: "debian".to_string(),
            id_like: Vec::new(),
            version_id: Some("12".to_string()),
            version_codename: Some("bookworm".to_string()),
        }
    }

    fn deb_backend(index: &dyn Index) -> ResolvableBackend<'_> {
        ResolvableBackend {
            id: BackendId::new("apt"),
            purl_type: "deb".to_string(),
            vers_type: "deb".to_string(),
            platform_independent: false,
            index,
        }
    }

    fn cargo_backend(index: &dyn Index) -> ResolvableBackend<'_> {
        ResolvableBackend {
            id: BackendId::new("cargo"),
            purl_type: "cargo".to_string(),
            vers_type: "cargo".to_string(),
            platform_independent: true,
            index,
        }
    }

    fn github_backend(index: &dyn Index) -> ResolvableBackend<'_> {
        ResolvableBackend {
            id: BackendId::new("github-release"),
            purl_type: "github".to_string(),
            vers_type: "semver".to_string(),
            platform_independent: false,
            index,
        }
    }

    const AMD64: Target = Target::Platform {
        os: Os::Linux,
        arch: Arch::Amd64,
    };

    #[test]
    fn resolves_via_apt_under_the_current_platform() {
        let distro = debian();
        let host = Host {
            os: Os::Linux,
            arch: Arch::Amd64,
            distro: Some(&distro),
        };
        let index = FakeIndex::new(Scheme::Debian, &["1.7.0-1", "1.7.1-3", "1.8.0-1"]);
        let m = manifest(
            &[],
            vec![entry(
                "jq",
                Some("1.7.1-3"),
                &[("linux:debian", block(&["apt"], None, None))],
            )],
        );

        let r = resolve(&m, "sha256:abc", &host, &[deb_backend(&index)], &[]).unwrap();

        assert_eq!(
            r.outcomes,
            vec![EntryOutcome {
                name: "jq".to_string(),
                outcome: Outcome::Resolved {
                    target: AMD64,
                    backend: BackendId::new("apt"),
                    version: Version::parse("1.7.1-3", Scheme::Debian).unwrap(),
                },
            }]
        );
        // One package, locked under linux/amd64, identity carries arch + distro.
        assert_eq!(r.lock.packages.len(), 1);
        let pkg = &r.lock.packages[0];
        assert_eq!(pkg.meta.canonical(), "vers:deb/1.7.1-3");
        let leaf = &pkg.targets[&AMD64];
        assert_eq!(leaf.placement.backend, BackendId::new("apt"));
        assert_eq!(
            leaf.placement.purl.to_string(),
            "pkg:deb/debian/jq?arch=amd64&distro=bookworm"
        );
        assert_eq!(leaf.resolved.as_str(), "1.7.1-3");
        // The target header records the manifest hash.
        assert_eq!(r.lock.targets[&AMD64].manifest_hash, "sha256:abc");
    }

    #[test]
    fn cargo_resolves_under_any() {
        let host = Host {
            os: Os::Linux,
            arch: Arch::Amd64,
            distro: None,
        };
        let index = FakeIndex::new(Scheme::Semver, &["13.0.0", "14.1.0", "14.1.1"]);
        let m = manifest(
            &[],
            vec![entry(
                "ripgrep",
                Some(">=14.0.0|<15.0.0"),
                &[("linux", block(&["cargo"], None, None))],
            )],
        );

        let r = resolve(&m, "sha256:h", &host, &[cargo_backend(&index)], &[]).unwrap();

        assert_eq!(
            r.outcomes[0].outcome,
            Outcome::Resolved {
                target: Target::Any,
                backend: BackendId::new("cargo"),
                version: Version::parse("14.1.1", Scheme::Semver).unwrap(),
            }
        );
        assert!(r.lock.targets.contains_key(&Target::Any));
        assert_eq!(
            r.lock.packages[0].targets[&Target::Any]
                .placement
                .purl
                .to_string(),
            "pkg:cargo/ripgrep"
        );
    }

    #[test]
    fn github_binary_qualifier_defaults_to_the_logical_name() {
        // `- name: ltk` with `package: ctxloom/ctxloom` must pick the ltk binary.
        let host = Host {
            os: Os::Linux,
            arch: Arch::Amd64,
            distro: None,
        };
        let index = FakeIndex::new(Scheme::Semver, &["0.6.3", "0.6.4"]);
        let m = manifest(
            &[],
            vec![entry(
                "ltk",
                Some(">=0.6.0"),
                &[(
                    "linux",
                    block(&["github-release"], Some("ctxloom/ctxloom"), None),
                )],
            )],
        );

        let r = resolve(&m, "sha256:h", &host, &[github_backend(&index)], &[]).unwrap();

        assert_eq!(
            r.outcomes[0].outcome,
            Outcome::Resolved {
                target: AMD64,
                backend: BackendId::new("github-release"),
                version: Version::parse("0.6.4", Scheme::Semver).unwrap(),
            }
        );
        let (queried, target) = index.last.borrow().clone().unwrap();
        assert_eq!(
            queried.purl.to_string(),
            "pkg:github/ctxloom/ctxloom?binary=ltk"
        );
        assert_eq!(target, AMD64);
    }

    #[test]
    fn github_asset_template_is_carried_into_the_purl() {
        let host = Host {
            os: Os::Linux,
            arch: Arch::Amd64,
            distro: None,
        };
        let index = FakeIndex::new(Scheme::Semver, &["2.0.2"]);
        let template = "{{ binary }}_{{ version }}_{{ os | title }}_{{ arch_goreleaser }}.tar.gz";
        let m = manifest(
            &[],
            vec![entry(
                "gum",
                Some(">=2.0.0"),
                &[(
                    "linux",
                    with_asset(
                        block(&["github-release"], Some("charmbracelet/gum"), None),
                        template,
                    ),
                )],
            )],
        );

        let r = resolve(&m, "sha256:h", &host, &[github_backend(&index)], &[]).unwrap();

        let (queried, _) = index.last.borrow().clone().unwrap();
        assert_eq!(
            queried.purl.qualifiers().get("asset").map(String::as_str),
            Some(template)
        );
        assert_eq!(
            queried.purl.qualifiers().get("binary").map(String::as_str),
            Some("gum")
        );
        // The lock records the same identity, and it round-trips through text.
        let leaf = r.lock.packages[0].targets.values().next().unwrap();
        let reparsed: Purl = leaf.placement.purl.to_string().parse().unwrap();
        assert_eq!(reparsed, leaf.placement.purl);
        assert_eq!(
            reparsed.qualifiers().get("asset").map(String::as_str),
            Some(template)
        );
    }

    #[test]
    fn github_asset_is_inherited_from_defaults() {
        let host = Host {
            os: Os::Linux,
            arch: Arch::Amd64,
            distro: None,
        };
        let index = FakeIndex::new(Scheme::Semver, &["0.8.1"]);
        let template = "{{ binary }}_{{ version }}_{{ os }}_{{ arch_goreleaser }}.tar.gz";
        let m = manifest(
            &[(
                "linux",
                with_asset(block(&["github-release"], None, None), template),
            )],
            vec![entry(
                "duf",
                Some(">=0.8.0"),
                &[("linux", block(&[], Some("muesli/duf"), None))],
            )],
        );

        resolve(&m, "sha256:h", &host, &[github_backend(&index)], &[]).unwrap();

        let (queried, _) = index.last.borrow().clone().unwrap();
        assert_eq!(
            queried.purl.qualifiers().get("asset").map(String::as_str),
            Some(template)
        );
    }

    #[test]
    fn entry_ext_overrides_defaults_per_key_and_inherits_the_rest() {
        let index = FakeIndex::new(Scheme::Semver, &["0.8.1"]);
        let host = Host {
            os: Os::Linux,
            arch: Arch::Amd64,
            distro: None,
        };
        let defaults = with_ext(
            with_asset(block(&["github-release"], None, None), "default.tar.gz"),
            "github-release",
            "tag",
            "v{{ version }}",
        );
        let m = manifest(
            &[("linux", defaults)],
            vec![entry(
                "duf",
                Some(">=0.8.0"),
                &[(
                    "linux",
                    with_asset(block(&[], Some("muesli/duf"), None), "duf.tar.gz"),
                )],
            )],
        );

        resolve(&m, "sha256:h", &host, &[github_backend(&index)], &[]).unwrap();

        let (queried, _) = index.last.borrow().clone().unwrap();
        let q = queried.purl.qualifiers();
        assert_eq!(q.get("asset").map(String::as_str), Some("duf.tar.gz"));
        assert_eq!(q.get("tag").map(String::as_str), Some("v{{ version }}"));
    }

    #[test]
    fn the_chosen_backends_ext_becomes_its_purl_qualifiers() {
        let index = FakeIndex::new(Scheme::Semver, &["0.8.1"]);
        let host = Host {
            os: Os::Linux,
            arch: Arch::Amd64,
            distro: None,
        };
        let m = manifest(
            &[],
            vec![entry(
                "duf",
                Some(">=0.8.0"),
                &[(
                    "linux",
                    with_ext(block(&["cargo"], None, None), "cargo", "features", "x"),
                )],
            )],
        );

        resolve(&m, "sha256:h", &host, &[cargo_backend(&index)], &[]).unwrap();

        let (queried, _) = index.last.borrow().clone().unwrap();
        assert_eq!(queried.purl.to_string(), "pkg:cargo/duf?features=x");
    }

    #[test]
    fn asset_does_not_reach_a_non_github_identity() {
        // `backends: [github-release, cargo]` with an asset: when cargo is the
        // one chosen, its identity carries no asset qualifier.
        let index = FakeIndex::new(Scheme::Semver, &["0.8.1"]);
        let host = Host {
            os: Os::Linux,
            arch: Arch::Amd64,
            distro: None,
        };
        let m = manifest(
            &[],
            vec![entry(
                "duf",
                Some(">=0.8.0"),
                &[(
                    "linux",
                    with_asset(block(&["github-release", "cargo"], None, None), "x.tar.gz"),
                )],
            )],
        );

        resolve(&m, "sha256:h", &host, &[cargo_backend(&index)], &[]).unwrap();

        let (queried, _) = index.last.borrow().clone().unwrap();
        assert_eq!(queried.purl.to_string(), "pkg:cargo/duf");
    }

    #[test]
    fn selector_prefers_exact_id_then_id_like_then_plain_os() {
        // defaults give cargo on any linux; a linux:debian block gives apt.
        let apt = FakeIndex::new(Scheme::Debian, &["1.7.1-3"]);
        let cargo = FakeIndex::new(Scheme::Semver, &["1.7.1"]);
        let backends = [deb_backend(&apt), cargo_backend(&cargo)];
        let m = manifest(
            &[("linux", block(&["cargo"], None, Some("1.7.1")))],
            vec![entry(
                "jq",
                None,
                &[("linux:debian", block(&["apt"], None, Some("1.7.1-3")))],
            )],
        );

        // On Debian: the exact-ID block wins → apt.
        let distro = debian();
        let debian_host = Host {
            os: Os::Linux,
            arch: Arch::Amd64,
            distro: Some(&distro),
        };
        let on_debian = resolve(&m, "h", &debian_host, &backends, &[]).unwrap();
        assert!(matches!(
            &on_debian.outcomes[0].outcome,
            Outcome::Resolved { backend, .. } if *backend == BackendId::new("apt")
        ));

        // On Ubuntu (ID_LIKE debian): still matches the linux:debian block → apt.
        let ubuntu = Distro {
            id: "ubuntu".to_string(),
            id_like: vec!["debian".to_string()],
            version_id: None,
            version_codename: Some("noble".to_string()),
        };
        let ubuntu_host = Host {
            os: Os::Linux,
            arch: Arch::Amd64,
            distro: Some(&ubuntu),
        };
        let on_ubuntu = resolve(&m, "h", &ubuntu_host, &backends, &[]).unwrap();
        assert!(matches!(
            &on_ubuntu.outcomes[0].outcome,
            Outcome::Resolved { backend, .. } if *backend == BackendId::new("apt")
        ));

        // On Fedora (not debian-family): falls back to defaults → cargo.
        let fedora = Distro {
            id: "fedora".to_string(),
            id_like: Vec::new(),
            version_id: Some("41".to_string()),
            version_codename: None,
        };
        let fedora_host = Host {
            os: Os::Linux,
            arch: Arch::Amd64,
            distro: Some(&fedora),
        };
        let on_fedora = resolve(&m, "h", &fedora_host, &backends, &[]).unwrap();
        assert!(matches!(
            &on_fedora.outcomes[0].outcome,
            Outcome::Resolved { backend, .. } if *backend == BackendId::new("cargo")
        ));
    }

    #[test]
    fn installer_preference_picks_among_several_candidates() {
        // `1.0.0` is a valid version under both the deb and semver schemes, so
        // the shared spec resolves whichever backend is chosen.
        let apt = FakeIndex::new(Scheme::Debian, &["1.0.0"]);
        let cargo = FakeIndex::new(Scheme::Semver, &["1.0.0"]);
        let backends = [deb_backend(&apt), cargo_backend(&cargo)];
        let distro = debian();
        let host = Host {
            os: Os::Linux,
            arch: Arch::Amd64,
            distro: Some(&distro),
        };
        let m = manifest(
            &[],
            vec![entry(
                "fd",
                Some(">=1.0.0"),
                &[("linux", block(&["apt", "cargo"], None, None))],
            )],
        );

        // No preference: manifest order wins → apt.
        let default = resolve(&m, "h", &host, &backends, &[]).unwrap();
        assert!(matches!(
            &default.outcomes[0].outcome,
            Outcome::Resolved { backend, .. } if *backend == BackendId::new("apt")
        ));

        // Preference for cargo reorders → cargo.
        let pref = resolve(&m, "h", &host, &backends, &[BackendId::new("cargo")]).unwrap();
        assert!(matches!(
            &pref.outcomes[0].outcome,
            Outcome::Resolved { backend, .. } if *backend == BackendId::new("cargo")
        ));
    }

    #[test]
    fn a_single_explicit_backend_is_never_overridden_by_preference() {
        let apt = FakeIndex::new(Scheme::Debian, &["1.0.0"]);
        let cargo = FakeIndex::new(Scheme::Semver, &["1.0.0"]);
        let backends = [deb_backend(&apt), cargo_backend(&cargo)];
        let distro = debian();
        let host = Host {
            os: Os::Linux,
            arch: Arch::Amd64,
            distro: Some(&distro),
        };
        let m = manifest(
            &[],
            vec![entry(
                "fd",
                Some(">=1.0.0"),
                &[("linux", block(&["apt"], None, None))],
            )],
        );

        let r = resolve(&m, "h", &host, &backends, &[BackendId::new("cargo")]).unwrap();
        assert!(matches!(
            &r.outcomes[0].outcome,
            Outcome::Resolved { backend, .. } if *backend == BackendId::new("apt")
        ));
    }

    #[test]
    fn not_applicable_when_no_selector_matches() {
        let index = FakeIndex::new(Scheme::Semver, &["1.0.0"]);
        let host = Host {
            os: Os::Linux,
            arch: Arch::Amd64,
            distro: None,
        };
        let m = manifest(
            &[],
            vec![entry(
                "mac-tool",
                Some("1.0"),
                &[("darwin", block(&["cargo"], None, None))],
            )],
        );

        let r = resolve(&m, "h", &host, &[cargo_backend(&index)], &[]).unwrap();
        assert_eq!(
            r.outcomes[0].outcome,
            Outcome::NotApplicable(NotApplicable::NoSelector)
        );
        assert!(r.lock.packages.is_empty());
        assert!(r.lock.targets.is_empty());
    }

    #[test]
    fn not_applicable_when_no_named_backend_is_registered() {
        let apt = FakeIndex::new(Scheme::Debian, &["1.0-1"]);
        let distro = debian();
        let host = Host {
            os: Os::Linux,
            arch: Arch::Amd64,
            distro: Some(&distro),
        };
        let m = manifest(
            &[],
            vec![entry(
                "fd",
                Some("1.0"),
                &[("linux", block(&["brew"], None, None))],
            )],
        );

        let r = resolve(&m, "h", &host, &[deb_backend(&apt)], &[]).unwrap();
        assert_eq!(
            r.outcomes[0].outcome,
            Outcome::NotApplicable(NotApplicable::NoRegisteredBackend {
                candidates: vec![BackendId::new("brew")],
            })
        );
    }

    #[test]
    fn a_block_with_no_backend_is_not_applicable() {
        let apt = FakeIndex::new(Scheme::Debian, &["1.0-1"]);
        let distro = debian();
        let host = Host {
            os: Os::Linux,
            arch: Arch::Amd64,
            distro: Some(&distro),
        };
        let m = manifest(
            &[],
            vec![entry(
                "fd",
                Some("1.0"),
                &[("linux", block(&[], Some("fd-find"), None))],
            )],
        );

        let r = resolve(&m, "h", &host, &[deb_backend(&apt)], &[]).unwrap();
        assert_eq!(
            r.outcomes[0].outcome,
            Outcome::NotApplicable(NotApplicable::NoBackend)
        );
    }

    #[test]
    fn unsatisfiable_when_nothing_available_is_in_the_set() {
        let index = FakeIndex::new(Scheme::Semver, &["1.0.0", "2.0.0"]);
        let host = Host {
            os: Os::Linux,
            arch: Arch::Amd64,
            distro: None,
        };
        let m = manifest(
            &[],
            vec![entry(
                "fd",
                Some(">=3.0.0"),
                &[("linux", block(&["cargo"], None, None))],
            )],
        );

        let r = resolve(&m, "h", &host, &[cargo_backend(&index)], &[]).unwrap();
        match &r.outcomes[0].outcome {
            Outcome::Unsatisfiable { spec, available } => {
                assert_eq!(spec, "vers:cargo/>=3.0.0");
                assert_eq!(available, "2.0.0, 1.0.0"); // newest first
            }
            other => panic!("expected unsatisfiable, got {other:?}"),
        }
        assert!(r.lock.packages.is_empty());
    }

    #[test]
    fn a_per_block_spec_overrides_the_entry_spec() {
        let index = FakeIndex::new(Scheme::Semver, &["1.0.0", "2.0.0", "3.0.0"]);
        let host = Host {
            os: Os::Linux,
            arch: Arch::Amd64,
            distro: None,
        };
        // Entry spec says <2, but the linux block overrides to >=3.
        let m = manifest(
            &[],
            vec![entry(
                "fd",
                Some("<2.0.0"),
                &[("linux", block(&["cargo"], None, Some(">=3.0.0")))],
            )],
        );

        let r = resolve(&m, "h", &host, &[cargo_backend(&index)], &[]).unwrap();
        assert!(matches!(
            &r.outcomes[0].outcome,
            Outcome::Resolved { version, .. } if version.as_str() == "3.0.0"
        ));
    }

    #[test]
    fn missing_spec_is_a_hard_error() {
        let index = FakeIndex::new(Scheme::Semver, &["1.0.0"]);
        let host = Host {
            os: Os::Linux,
            arch: Arch::Amd64,
            distro: None,
        };
        let m = manifest(
            &[],
            vec![entry(
                "fd",
                None,
                &[("linux", block(&["cargo"], None, None))],
            )],
        );

        let err = resolve(&m, "h", &host, &[cargo_backend(&index)], &[]).unwrap_err();
        assert_eq!(
            err,
            ResolveError::MissingSpec {
                name: "fd".to_string()
            }
        );
    }

    #[test]
    fn a_deb_identity_without_distro_facts_is_an_error() {
        let apt = FakeIndex::new(Scheme::Debian, &["1.0-1"]);
        let host = Host {
            os: Os::Linux,
            arch: Arch::Amd64,
            distro: None, // no os-release
        };
        let m = manifest(
            &[],
            vec![entry(
                "jq",
                Some("1.0-1"),
                &[("linux", block(&["apt"], None, None))],
            )],
        );

        let err = resolve(&m, "h", &host, &[deb_backend(&apt)], &[]).unwrap_err();
        assert_eq!(
            err,
            ResolveError::NoDistro {
                name: "jq".to_string()
            }
        );
    }

    #[test]
    fn a_github_id_without_a_slash_is_an_error() {
        let index = FakeIndex::new(Scheme::Semver, &["0.6.4"]);
        let host = Host {
            os: Os::Linux,
            arch: Arch::Amd64,
            distro: None,
        };
        let m = manifest(
            &[],
            vec![entry(
                "ctxloom",
                Some(">=0.6.0"),
                &[("linux", block(&["github-release"], Some("ctxloom"), None))],
            )],
        );

        let err = resolve(&m, "h", &host, &[github_backend(&index)], &[]).unwrap_err();
        assert_eq!(
            err,
            ResolveError::BadGithubId {
                name: "ctxloom".to_string(),
                value: "ctxloom".to_string(),
            }
        );
    }

    #[test]
    fn deb_identity_omits_distro_qualifier_without_a_codename() {
        let apt = FakeIndex::new(Scheme::Debian, &["1.0-1"]);
        let rolling = Distro {
            id: "debian".to_string(),
            id_like: Vec::new(),
            version_id: None,
            version_codename: None,
        };
        let host = Host {
            os: Os::Linux,
            arch: Arch::Amd64,
            distro: Some(&rolling),
        };
        let m = manifest(
            &[],
            vec![entry(
                "jq",
                Some("1.0-1"),
                &[("linux", block(&["apt"], None, None))],
            )],
        );

        let r = resolve(&m, "h", &host, &[deb_backend(&apt)], &[]).unwrap();
        assert_eq!(
            r.lock.packages[0].targets[&AMD64]
                .placement
                .purl
                .to_string(),
            "pkg:deb/debian/jq?arch=amd64"
        );
    }

    #[test]
    fn an_index_failure_is_a_hard_error() {
        let host = Host {
            os: Os::Linux,
            arch: Arch::Amd64,
            distro: None,
        };
        let m = manifest(
            &[],
            vec![entry(
                "fd",
                Some("1.0.0"),
                &[("linux", block(&["cargo"], None, None))],
            )],
        );
        let fail = FailIndex;
        let backend = ResolvableBackend {
            id: BackendId::new("cargo"),
            purl_type: "cargo".to_string(),
            vers_type: "cargo".to_string(),
            platform_independent: true,
            index: &fail,
        };

        let err = resolve(&m, "h", &host, &[backend], &[]).unwrap_err();
        assert!(matches!(err, ResolveError::Query { name, .. } if name == "fd"));
    }

    // --- 8-2: multi-claim, frozen, merge, freshness ------------------------

    const ARM64: Target = Target::Platform {
        os: Os::Linux,
        arch: Arch::Arm64,
    };
    const DARWIN: Target = Target::Platform {
        os: Os::Macos,
        arch: Arch::Arm64,
    };

    fn claim(manifest: &str, spec: &str, user: Option<&str>) -> Claim {
        Claim {
            manifest: std::path::PathBuf::from(manifest),
            spec: spec.to_string(),
            user: user.map(str::to_string),
        }
    }

    fn deb(raw: &str) -> Version {
        Version::parse(raw, Scheme::Debian).unwrap()
    }

    fn deb_versions(raw: &[&str]) -> Vec<Version> {
        raw.iter().map(|v| deb(v)).collect()
    }

    fn lock_leaf(resolved: &str) -> LockLeaf {
        LockLeaf {
            placement: Placement {
                backend: BackendId::new("apt"),
                purl: "pkg:deb/debian/x".parse().unwrap(),
            },
            spec: None,
            resolved: deb(resolved),
            index_snapshot: None,
            checksum: None,
        }
    }

    fn lock_pkg(name: &str, leaves: &[(Target, &str)]) -> LockPackage {
        Package {
            name: name.to_string(),
            meta: Spec::new(">=0", "deb").unwrap(),
            targets: leaves.iter().map(|(t, v)| (*t, lock_leaf(v))).collect(),
        }
    }

    fn lock_doc(targets: &[(Target, &str)], packages: Vec<LockPackage>) -> LockDocument {
        LockDocument {
            lockfile_version: 1,
            targets: targets
                .iter()
                .map(|(t, h)| {
                    (
                        *t,
                        TargetLock {
                            manifest_hash: h.to_string(),
                        },
                    )
                })
                .collect(),
            packages,
        }
    }

    #[test]
    fn select_across_claims_takes_the_highest_in_the_intersection() {
        let claims = [
            claim("/w/a/bosun.yaml", ">=1.7|<2", None),
            claim("/w/b/bosun.yaml", ">=1.7.1|<1.7.2~", None),
        ];
        let available = deb_versions(&["1.7.0-1", "1.7.1-3", "1.7.1-4", "1.8.0-1"]);
        // 1.8.0-1 is in A but not B; 1.7.0-1 is in A but not B; so the
        // intersection is {1.7.1-3, 1.7.1-4} and the highest is 1.7.1-4.
        let chosen = select_across_claims(&claims, "deb", &available, &[]).unwrap();
        assert_eq!(chosen.as_str(), "1.7.1-4");
    }

    #[test]
    fn lockfiles_advise_a_locked_version_is_preferred_when_admissible() {
        let claims = [
            claim("/w/a/bosun.yaml", ">=1.7|<2", None),
            claim("/w/b/bosun.yaml", ">=1.7.1|<1.7.2~", None),
        ];
        let available = deb_versions(&["1.7.1-3", "1.7.1-4"]);
        // Both admissible; the lock pins 1.7.1-3, so it wins over the higher one.
        let chosen = select_across_claims(&claims, "deb", &available, &[deb("1.7.1-3")]).unwrap();
        assert_eq!(chosen.as_str(), "1.7.1-3");
    }

    #[test]
    fn a_locked_version_no_claim_admits_is_skipped_for_the_highest() {
        let claims = [
            claim("/w/a/bosun.yaml", ">=1.7|<2", None),
            claim("/w/b/bosun.yaml", ">=1.7.1|<1.7.2~", None),
        ];
        let available = deb_versions(&["1.7.1-3", "1.7.1-4", "1.8.0-1"]);
        // The lock pins 1.8.0-1, which B excludes: fall back to the highest
        // version the intersection admits.
        let chosen = select_across_claims(&claims, "deb", &available, &[deb("1.8.0-1")]).unwrap();
        assert_eq!(chosen.as_str(), "1.7.1-4");
    }

    #[test]
    fn empty_intersection_refuses_naming_every_claim() {
        // Two different exact pins cannot both be satisfied (§4.6).
        let claims = [
            claim("/w/a/bosun.yaml", "1.7.1-3", None),
            claim("/w/b/bosun.yaml", "1.7.1-4", None),
        ];
        let available = deb_versions(&["1.7.1-3", "1.7.1-4"]);
        let err = select_across_claims(&claims, "deb", &available, &[]).unwrap_err();
        match err {
            ClaimError::EmptyIntersection { claims, available } => {
                assert!(
                    claims.contains("/w/a/bosun.yaml=1.7.1-3"),
                    "claims: {claims}"
                );
                assert!(
                    claims.contains("/w/b/bosun.yaml=1.7.1-4"),
                    "claims: {claims}"
                );
                assert_eq!(available, "1.7.1-3, 1.7.1-4");
            }
            other => panic!("expected empty intersection, got {other:?}"),
        }
    }

    #[test]
    fn another_os_users_claim_constrains_the_intersection() {
        // A system-scope package: babbitt wants >=1.7, alice caps it below 1.7.1,
        // so the shared install must be 1.7.0-1 even though 1.7.1-3 is available.
        let available = deb_versions(&["1.7.0-1", "1.7.1-3"]);
        let babbitt = claim("/home/babbitt/w/bosun.yaml", ">=1.7", Some("babbitt"));
        let alice = claim("/home/alice/w/bosun.yaml", "<1.7.1", Some("alice"));

        let with_alice =
            select_across_claims(&[babbitt.clone(), alice], "deb", &available, &[]).unwrap();
        assert_eq!(with_alice.as_str(), "1.7.0-1");
        // Without alice's claim, babbitt alone would take the higher version.
        let alone = select_across_claims(&[babbitt], "deb", &available, &[]).unwrap();
        assert_eq!(alone.as_str(), "1.7.1-3");
    }

    #[test]
    fn frozen_select_holds_the_locked_version_when_every_claim_admits_it() {
        let claims = [
            claim("/w/a/bosun.yaml", ">=1.7|<2", None),
            claim("/w/b/bosun.yaml", ">=1.7.1|<1.7.2~", None),
        ];
        let chosen = frozen_select(&deb("1.7.1-3"), &claims, "deb").unwrap();
        assert_eq!(chosen.as_str(), "1.7.1-3");
    }

    #[test]
    fn frozen_select_refuses_a_clash_naming_only_the_clashing_claim() {
        let claims = [
            claim("/w/a/bosun.yaml", ">=1.7", None),
            claim("/w/b/bosun.yaml", ">=1.8", None), // excludes 1.7.1-3
        ];
        let err = frozen_select(&deb("1.7.1-3"), &claims, "deb").unwrap_err();
        match err {
            ClaimError::FrozenClash { locked, clashes } => {
                assert_eq!(locked, "1.7.1-3");
                assert!(
                    clashes.contains("/w/b/bosun.yaml=>=1.8"),
                    "clashes: {clashes}"
                );
                assert!(
                    !clashes.contains("/w/a"),
                    "non-clashing claim named: {clashes}"
                );
            }
            other => panic!("expected frozen clash, got {other:?}"),
        }
    }

    #[test]
    fn merge_lock_replaces_fresh_targets_and_leaves_others_untouched() {
        let existing = lock_doc(
            &[(AMD64, "h-old"), (ARM64, "h-arm")],
            vec![lock_pkg("jq", &[(AMD64, "1.7.1-3"), (ARM64, "1.7.1-2")])],
        );
        let fresh = lock_doc(
            &[(AMD64, "h-new")],
            vec![
                lock_pkg("jq", &[(AMD64, "1.7.1-4")]),
                lock_pkg("fd", &[(AMD64, "9.0.0-1")]),
            ],
        );

        let merged = merge_lock(&existing, &fresh);

        // Headers: amd64 overwritten, arm64 untouched.
        assert_eq!(merged.targets[&AMD64].manifest_hash, "h-new");
        assert_eq!(merged.targets[&ARM64].manifest_hash, "h-arm");
        // jq: amd64 re-resolved, arm64 kept from the old lock.
        let jq = merged.packages.iter().find(|p| p.name == "jq").unwrap();
        assert_eq!(jq.targets[&AMD64].resolved.as_str(), "1.7.1-4");
        assert_eq!(jq.targets[&ARM64].resolved.as_str(), "1.7.1-2");
        // fd is new this run.
        assert!(merged.packages.iter().any(|p| p.name == "fd"));
    }

    #[test]
    fn merge_lock_drops_a_package_absent_from_a_reresolved_target() {
        let existing = lock_doc(
            &[(AMD64, "h-old")],
            vec![lock_pkg("old", &[(AMD64, "1.0.0-1")])],
        );
        let fresh = lock_doc(
            &[(AMD64, "h-new")],
            vec![lock_pkg("new", &[(AMD64, "2.0.0-1")])],
        );

        let merged = merge_lock(&existing, &fresh);

        // `old` had only an amd64 leaf, amd64 was re-resolved without it → gone.
        assert!(!merged.packages.iter().any(|p| p.name == "old"));
        assert!(merged.packages.iter().any(|p| p.name == "new"));
    }

    #[test]
    fn merge_lock_keeps_a_package_whose_target_was_not_reresolved() {
        let existing = lock_doc(
            &[(DARWIN, "h-mac")],
            vec![lock_pkg("mac-tool", &[(DARWIN, "1.0.0-1")])],
        );
        let fresh = lock_doc(
            &[(AMD64, "h-new")],
            vec![lock_pkg("linux-tool", &[(AMD64, "2.0.0-1")])],
        );

        let merged = merge_lock(&existing, &fresh);

        // darwin/arm64 was not re-resolved, so mac-tool survives untouched.
        let mac = merged
            .packages
            .iter()
            .find(|p| p.name == "mac-tool")
            .unwrap();
        assert_eq!(mac.targets[&DARWIN].resolved.as_str(), "1.0.0-1");
        assert!(merged.packages.iter().any(|p| p.name == "linux-tool"));
        assert_eq!(merged.targets[&DARWIN].manifest_hash, "h-mac");
        assert_eq!(merged.targets[&AMD64].manifest_hash, "h-new");
    }

    #[test]
    fn check_targets_flags_missing_and_stale_against_the_current_hash() {
        let lock = lock_doc(&[(AMD64, "cur"), (ARM64, "old")], vec![]);
        let report = check_targets(&[AMD64, ARM64, DARWIN], "cur", &lock);
        assert_eq!(
            report,
            vec![
                TargetFreshness {
                    target: AMD64,
                    freshness: Freshness::Fresh,
                },
                TargetFreshness {
                    target: ARM64,
                    freshness: Freshness::Stale,
                },
                TargetFreshness {
                    target: DARWIN,
                    freshness: Freshness::Missing,
                },
            ]
        );
    }
}
