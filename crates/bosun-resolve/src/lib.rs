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
//! block (distro release > exact distro ID > ID_LIKE > libc > plain OS >
//! `any`, then `defaults.install`, §5.4);
//! chooses a backend by installer preference (flag > env > config > manifest
//! order, a single explicit backend never overridden, §5.3); builds the
//! canonical purl from the backend type and host facts (§5.6); queries the
//! backend index; and selects the highest version in the spec's vers set (§7).
//!
//! A platform-independent backend (cargo, go, npm, …) resolves under the `any`
//! target; every other backend under the current platform (§4.7). [`resolve`]
//! gives this machine's whole lock: the lock is the machine's own, so there is
//! nothing to merge.
//!
//! Where one install scope holds a package claimed by several manifests (§4.6),
//! [`select_across_claims`] intersects every claim's vers set — refusing when
//! they disagree — and [`frozen_select`] holds a claim to its locked version,
//! refusing a clash (`install --frozen`, §7).

mod errmsg;

use std::collections::BTreeMap;

use bosun_core::manifest::{Backends, InstallBlock, Manifest, ManifestEntry, PACKAGE_OPTION};
use bosun_core::platform::{Arch, Distro, Libc, Os, Selector, Target};
use bosun_core::record::{LockDocument, LockLeaf, LockPackage, Package, Placement};
use bosun_core::spec::{Spec, SpecError};
use bosun_core::{BackendId, Claim, Purl, PurlError, Ref, Version, Want};
use bosun_ports::backend::Index;
use bosun_ports::error::PortError;

/// The lockfile format version this resolver writes (§4.5); bosun-io's
/// `LOCKFILE_VERSION`, which reads and writes the file, is the same number.
pub const LOCKFILE_VERSION: u32 = 2;

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
    /// The C library, when detected (§5.4).
    pub libc: Option<&'a Libc>,
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
    /// A deb, rpm or apk identity needs distro facts this host lacks.
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
/// order is appended per entry. Every entry gets the newest version its spec
/// admits; [`resolve_with_lock`] keeps versions a lock already pins.
pub fn resolve(
    manifest: &Manifest,
    manifest_hash: &str,
    host: &Host<'_>,
    backends: &[ResolvableBackend<'_>],
    preference: &[BackendId],
) -> Result<Resolution, ResolveError> {
    resolve_with_lock(manifest, manifest_hash, host, backends, preference, None)
}

/// [`resolve`], keeping each version `locked` pins while it still fits (§7):
/// the same backend and package identity, still available, still in the
/// entry's spec. An entry the lock doesn't pin, or whose pin no longer fits,
/// gets the newest version its spec admits. Moving a pin forward is
/// `upgrade`'s job, which resolves without the pins it releases.
pub fn resolve_with_lock(
    manifest: &Manifest,
    manifest_hash: &str,
    host: &Host<'_>,
    backends: &[ResolvableBackend<'_>],
    preference: &[BackendId],
    locked: Option<&LockDocument>,
) -> Result<Resolution, ResolveError> {
    let mut outcomes = Vec::with_capacity(manifest.packages.len());
    let mut packages = Vec::new();

    for entry in &manifest.packages {
        let pinned = locked.and_then(|l| l.packages.iter().find(|p| p.name == entry.name));
        let (outcome, resolved) =
            resolve_entry(entry, manifest, host, backends, preference, pinned)?;
        if let Some((_, package)) = resolved {
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
            // The caller knows which file this is for; the resolver doesn't.
            manifest: String::new(),
            manifest_hash: manifest_hash.to_string(),
            packages,
        },
        outcomes,
    })
}

/// The names of `manifest`'s entries that apply on `host`: an install block
/// matches it and names a backend registered here (§5.3, §5.4). The rest
/// resolve to [`Outcome::NotApplicable`], so these are what a complete lock
/// for this machine pins.
pub fn applicable<'m>(
    manifest: &'m Manifest,
    host: &Host<'_>,
    backends: &[ResolvableBackend<'_>],
    preference: &[BackendId],
) -> Vec<&'m str> {
    manifest
        .packages
        .iter()
        .filter(|e| applying(e, manifest, host, backends, preference).is_ok())
        .map(|e| e.name.as_str())
        .collect()
}

/// The install block that applies to `entry` on `host`, defaults merged
/// (§5.4), and the registered backend it installs with (§5.3); or why none
/// does.
fn applying<'b>(
    entry: &ManifestEntry,
    manifest: &Manifest,
    host: &Host<'_>,
    backends: &'b [ResolvableBackend<'b>],
    preference: &[BackendId],
) -> Result<(InstallBlock, &'b ResolvableBackend<'b>), NotApplicable> {
    let block =
        effective_block(entry, manifest, host, backends).ok_or(NotApplicable::NoSelector)?;
    if block.backends.is_empty() {
        return Err(NotApplicable::NoBackend);
    }
    // §5.3: first candidate that is registered and highest in preference.
    let chosen = choose_backend(&block.backends, backends, preference).ok_or_else(|| {
        NotApplicable::NoRegisteredBackend {
            candidates: block.backends.keys().map(BackendId::new).collect(),
        }
    })?;
    Ok((block, chosen))
}

/// Resolve a single entry. Returns its outcome and, when resolved, the target
/// key and lockfile package to record.
fn resolve_entry(
    entry: &ManifestEntry,
    manifest: &Manifest,
    host: &Host<'_>,
    backends: &[ResolvableBackend<'_>],
    preference: &[BackendId],
    pinned: Option<&LockPackage>,
) -> Result<(Outcome, Option<(Target, LockPackage)>), ResolveError> {
    let (block, chosen) = match applying(entry, manifest, host, backends, preference) {
        Ok(applies) => applies,
        Err(na) => return Ok((Outcome::NotApplicable(na), None)),
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
    let native = block.package_for(chosen.id.as_str()).unwrap_or(&entry.name);
    let mut qualifiers = block
        .backends
        .get(chosen.id.as_str())
        .cloned()
        .unwrap_or_default();
    // The package is already the identity's name, not a qualifier.
    qualifiers.remove(PACKAGE_OPTION);
    let purl = build_purl(&entry.name, &chosen.purl_type, native, qualifiers, host)?;

    // §4.7: platform-independent backends resolve under `any`.
    let target = if chosen.platform_independent {
        Target::Any
    } else {
        Target::Platform {
            os: host.os,
            arch: host.arch,
        }
    };

    // §7: the version the backend's package manager would choose in the
    // spec's set, keeping a pin that still fits.
    let pin = pinned
        .and_then(|p| p.targets.get(&target))
        .filter(|leaf| leaf.placement.backend == chosen.id && leaf.placement.purl == purl);
    let prefer: Vec<Version> = pin.iter().map(|leaf| leaf.resolved.clone()).collect();
    let selection = chosen
        .index
        .select(
            &Ref::new(purl.clone()),
            &target,
            &Want::of(spec.range().clone()),
            &prefer,
        )
        .map_err(|source| ResolveError::Query {
            name: entry.name.clone(),
            source,
        })?;
    let Some(version) = selection.chosen else {
        let mut available = selection.available;
        available.sort_by(|a, b| b.cmp(a)); // newest first
        return Ok((
            Outcome::Unsatisfiable {
                spec: spec.canonical(),
                available: versions_summary(&available),
            },
            None,
        ));
    };
    let kept = pin.filter(|leaf| leaf.resolved == version);
    // §12: the checksum recorded for a kept pin stands; otherwise record the
    // backend's (github-release: the published one, else the download's on
    // first use; apt/cargo have none).
    let checksum = match kept.and_then(|leaf| leaf.checksum.clone()) {
        Some(recorded) => Some(recorded),
        None => chosen
            .index
            .checksum(&Ref::new(purl.clone()), &version, &target)
            .map_err(|source| ResolveError::Query {
                name: entry.name.clone(),
                source,
            })?,
    };

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
/// block that applies to this host. A block naming no backends is matched
/// through the defaults' backends, the ones it would install with.
fn effective_block(
    entry: &ManifestEntry,
    manifest: &Manifest,
    host: &Host<'_>,
    registered: &[ResolvableBackend<'_>],
) -> Option<InstallBlock> {
    let from_defaults = most_specific(&manifest.defaults, host, registered, &Backends::new());
    let inherited = from_defaults
        .as_ref()
        .map(|b| b.backends.clone())
        .unwrap_or_default();
    let from_entry = most_specific(&entry.install, host, registered, &inherited);
    match (from_defaults, from_entry) {
        (None, None) => None,
        (Some(block), None) | (None, Some(block)) => Some(block),
        (Some(defaults), Some(entry)) => Some(InstallBlock {
            backends: merge_backends(defaults.backends, entry.backends),
            package: entry.package.or(defaults.package),
            spec: entry.spec.or(defaults.spec),
        }),
    }
}

/// The entry's backends, each inheriting its `defaults` options per key (an
/// option the entry sets wins); the defaults' set when the entry names none.
fn merge_backends(mut defaults: Backends, entry: Backends) -> Backends {
    if entry.is_empty() {
        return defaults;
    }
    entry
        .into_iter()
        .map(|(backend, options)| {
            let mut merged = defaults.remove(&backend).unwrap_or_default();
            merged.extend(options);
            (backend, merged)
        })
        .collect()
}

/// The most specific selector in `map` that applies to `host`, by the §5.4
/// ladder (release > exact distro ID > ID_LIKE > libc with a range > libc >
/// plain OS > any), or `None` if none apply. A block's release is spelled by
/// the purl types of its registered backends, or of `inherited` when it names
/// none.
fn most_specific(
    map: &BTreeMap<Selector, InstallBlock>,
    host: &Host<'_>,
    registered: &[ResolvableBackend<'_>],
    inherited: &Backends,
) -> Option<InstallBlock> {
    map.iter()
        .filter_map(|(sel, block)| {
            let candidates = if block.backends.is_empty() {
                inherited
            } else {
                &block.backends
            };
            let purl_types: Vec<&str> = registered
                .iter()
                .filter(|b| candidates.contains_key(b.id.as_str()))
                .map(|b| b.purl_type.as_str())
                .collect();
            specificity(sel, host, &purl_types).map(|rank| (rank, block))
        })
        .max_by_key(|(rank, _)| *rank)
        .map(|(_, block)| block.clone())
}

/// How specifically `sel` applies to `host`: higher is more specific. `None`
/// when it does not apply (wrong OS, a distro family the host is not in, a
/// release none of `purl_types` spells as the host's own, or a libc the host
/// lacks or whose range its version is outside).
fn specificity(sel: &Selector, host: &Host<'_>, purl_types: &[&str]) -> Option<u8> {
    match sel.os() {
        None => return Some(0), // any
        Some(os) if os != host.os => return None,
        Some(_) => {}
    }
    if let Some((family, range)) = sel.libc() {
        // A libc names a binary's ABI, not a package source: below any distro
        // match, and a range (a glibc floor) above the bare family.
        return host
            .libc
            .filter(|libc| libc.satisfies(family, range))
            .map(|_| if range.is_some() { 3 } else { 2 });
    }
    let Some(family) = sel.distro() else {
        return Some(1); // plain OS
    };
    let distro = host.distro?;
    match sel.release() {
        // A release names the distro's own ID, spelled as one of the block's
        // backends spells it in its purl `distro` qualifier.
        Some(release) => (distro.id == family
            && purl_types
                .iter()
                .any(|t| distro.release(t).as_deref() == Some(release)))
        .then_some(6),
        None if distro.id == family => Some(5), // exact ID
        None if distro.id_like.iter().any(|f| f == family) => Some(4), // ID_LIKE family
        None => None,
    }
}

/// Choose a backend among `candidates` (a set: the manifest says which are
/// acceptable, never in what order), restricted to the `registered` ones, by
/// installer preference (§5.3). A single candidate is used as-is; otherwise
/// the registered candidate highest in `preference` wins, falling back to the
/// order backends are registered in when none is preferred. `None` when no
/// candidate is registered.
fn choose_backend<'b>(
    candidates: &Backends,
    registered: &'b [ResolvableBackend<'b>],
    preference: &[BackendId],
) -> Option<&'b ResolvableBackend<'b>> {
    let available: Vec<&ResolvableBackend> = registered
        .iter()
        .filter(|b| candidates.contains_key(b.id.as_str()))
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
/// - `rpm`: `pkg:rpm/<distro-id>/<name>?arch=<rpm arch>&distro=<id>-<version-id>`
///   — the namespace is the distro ID, the arch is rpm's spelling (`x86_64`),
///   and `distro` is `ID-VERSION_ID` (`fedora-41`), omitted without a
///   `VERSION_ID`.
/// - `apk`: `pkg:apk/<distro-id>/<name>?arch=<apk arch>` (`x86_64`,
///   `aarch64`); no release qualifier, as Alpine has no release selectors.
/// - `github`: `pkg:github/<owner>/<repo>?binary=<logical>` — the package id
///   is `owner/repo`; the `binary` qualifier defaults to the entry's logical
///   name so `- name: ltk` with `package: owner/repo` selects the `ltk` binary.
/// - `npm`: `pkg:npm/%40<scope>/<name>` for `@scope/name` — the scope,
///   `@` and all, is the namespace — else `pkg:npm/<name>`.
/// - anything else: `pkg:<type>/<name>` (cargo, generic, …).
///
/// `qualifiers` are the chosen backend's options, without its package, kept
/// verbatim for the backend to read and validate (github-release's `asset`
/// template). Another backend's options never reach this identity. The qualifiers above are set after them, so an
/// option can't replace one.
fn build_purl(
    name: &str,
    purl_type: &str,
    native: &str,
    mut qualifiers: BTreeMap<String, String>,
    host: &Host<'_>,
) -> Result<Purl, ResolveError> {
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
            if let Some(release) = distro.release("deb") {
                qualifiers.insert("distro".to_string(), release);
            }
            Purl::from_components("deb", Some(&distro.id), native, None, qualifiers, None)
                .map_err(bad_purl)
        }
        "rpm" => {
            let distro = host.distro.ok_or_else(|| ResolveError::NoDistro {
                name: name.to_string(),
            })?;
            qualifiers.insert("arch".to_string(), rpm_arch(host.arch).to_string());
            if let Some(release) = &distro.version_id {
                qualifiers.insert("distro".to_string(), format!("{}-{release}", distro.id));
            }
            Purl::from_components("rpm", Some(&distro.id), native, None, qualifiers, None)
                .map_err(bad_purl)
        }
        "apk" => {
            let distro = host.distro.ok_or_else(|| ResolveError::NoDistro {
                name: name.to_string(),
            })?;
            // apk spells architectures as rpm does.
            qualifiers.insert("arch".to_string(), rpm_arch(host.arch).to_string());
            Purl::from_components("apk", Some(&distro.id), native, None, qualifiers, None)
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
        "npm" => {
            let (scope, bare) = match native.split_once('/') {
                Some((scope, bare)) if scope.starts_with('@') => (Some(scope), bare),
                _ => (None, native),
            };
            Purl::from_components("npm", scope, bare, None, qualifiers, None).map_err(bad_purl)
        }
        other => {
            Purl::from_components(other, None, native, None, qualifiers, None).map_err(bad_purl)
        }
    }
}

/// rpm's name for an architecture (`uname -m`).
fn rpm_arch(arch: Arch) -> &'static str {
    match arch {
        Arch::Amd64 => "x86_64",
        Arch::Arm64 => "aarch64",
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
    /// Asking the backend's index for the claims' version failed.
    #[error("{kind}: {0}", kind = errmsg::QUERY_FAILED)]
    Query(#[source] PortError),
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
/// admit it; within the intersection the backend's package manager chooses
/// ([`Index::select`]: apt by pin priority, npm and uv by resolving it
/// themselves, the rest the highest available version), except a
/// version some lockfile already chose is preferred when every claim admits it
/// (`lock_preference`, the current project's lock first — lockfiles advise,
/// specs decide). Claims from other OS users in system state are just more
/// claims here. Refuses with [`ClaimError::EmptyIntersection`] when no available
/// version satisfies them all, naming each manifest and spec.
pub fn select_across_claims(
    claims: &[Claim],
    vers_type: &str,
    index: &dyn Index,
    pkg: &Ref,
    target: &Target,
    lock_preference: &[Version],
) -> Result<Version, ClaimError> {
    let specs = parse_claims(claims, vers_type)?;
    let want = Want::new(specs.iter().map(|s| s.range().clone()).collect());
    // Lockfiles advise, specs decide; the backend's package manager orders
    // what the intersection admits (§5.6).
    let selection = index
        .select(pkg, target, &want, lock_preference)
        .map_err(ClaimError::Query)?;
    selection
        .chosen
        .ok_or_else(|| ClaimError::EmptyIntersection {
            claims: claims_summary(claims),
            available: versions_summary(&selection.available),
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    use bosun_core::manifest::Options;
    use bosun_core::version::Scheme;
    use bosun_core::{Checksum, LibcFamily};

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
            backends: backends
                .iter()
                .map(|s| (s.to_string(), Options::new()))
                .collect(),
            package: package.map(str::to_string),
            spec: spec.map(str::to_string),
        }
    }

    fn with_option(mut b: InstallBlock, backend: &str, key: &str, value: &str) -> InstallBlock {
        b.backends
            .entry(backend.to_string())
            .or_default()
            .insert(key.to_string(), value.to_string());
        b
    }

    fn with_asset(b: InstallBlock, asset: &str) -> InstallBlock {
        with_option(b, "github-release", "asset", asset)
    }

    fn entry(name: &str, spec: Option<&str>, install: &[(&str, InstallBlock)]) -> ManifestEntry {
        ManifestEntry {
            name: name.to_string(),
            spec: spec.map(str::to_string),
            formerly: Vec::new(),
            var: None,
            install: install.iter().map(|(s, b)| (sel(s), b.clone())).collect(),
        }
    }

    fn manifest(defaults: &[(&str, InstallBlock)], packages: Vec<ManifestEntry>) -> Manifest {
        Manifest {
            version: 1,
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
            base_codename: None,
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
            libc: None,
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
        // The lock records the manifest hash it was resolved against.
        assert_eq!(r.lock.manifest_hash, "sha256:abc");
    }

    #[test]
    fn cargo_resolves_under_any() {
        let host = Host {
            os: Os::Linux,
            arch: Arch::Amd64,
            distro: None,
            libc: None,
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
        assert_eq!(
            r.lock.packages[0].targets[&Target::Any]
                .placement
                .purl
                .to_string(),
            "pkg:cargo/ripgrep"
        );
    }

    // --- keeping locked versions ---------------------------------------------

    fn ripgrep_manifest(spec: &str, package: Option<&str>) -> Manifest {
        manifest(
            &[],
            vec![entry(
                "ripgrep",
                Some(spec),
                &[("linux", block(&["cargo"], package, None))],
            )],
        )
    }

    /// A lock pinning ripgrep at `version` under `purl` (cargo, `any`).
    fn ripgrep_locked(purl: &str, version: &str, checksum: Option<&str>) -> LockDocument {
        LockDocument {
            lockfile_version: LOCKFILE_VERSION,
            manifest: "/p/bosun.yaml".into(),
            manifest_hash: "sha256:old".into(),
            packages: vec![Package {
                name: "ripgrep".into(),
                meta: Spec::new(">=14.0.0|<15.0.0", "cargo").unwrap(),
                targets: BTreeMap::from([(
                    Target::Any,
                    LockLeaf {
                        placement: Placement {
                            backend: BackendId::new("cargo"),
                            purl: purl.parse().unwrap(),
                        },
                        spec: None,
                        resolved: Version::parse(version, Scheme::Semver).unwrap(),
                        index_snapshot: None,
                        checksum: checksum.map(|c| Checksum(c.to_string())),
                    },
                )]),
            }],
        }
    }

    fn resolved_version(r: &Resolution) -> &str {
        match &r.outcomes[0].outcome {
            Outcome::Resolved { version, .. } => version.as_str(),
            other => panic!("not resolved: {other:?}"),
        }
    }

    fn any_host() -> Host<'static> {
        Host {
            os: Os::Linux,
            arch: Arch::Amd64,
            distro: None,
            libc: None,
        }
    }

    #[test]
    fn a_locked_version_still_in_its_spec_is_kept() {
        let index = FakeIndex::new(Scheme::Semver, &["14.0.0", "14.1.1"]);
        let locked = ripgrep_locked("pkg:cargo/ripgrep", "14.0.0", None);
        let r = resolve_with_lock(
            &ripgrep_manifest(">=14.0.0|<15.0.0", None),
            "sha256:new",
            &any_host(),
            &[cargo_backend(&index)],
            &[],
            Some(&locked),
        )
        .unwrap();
        assert_eq!(resolved_version(&r), "14.0.0");
        assert_eq!(
            r.lock.packages[0].targets[&Target::Any].resolved.as_str(),
            "14.0.0"
        );
        assert_eq!(r.lock.manifest_hash, "sha256:new");
    }

    #[test]
    fn a_locked_version_the_spec_no_longer_admits_is_resolved_again() {
        let index = FakeIndex::new(Scheme::Semver, &["14.0.0", "14.1.1"]);
        let locked = ripgrep_locked("pkg:cargo/ripgrep", "14.0.0", None);
        let r = resolve_with_lock(
            &ripgrep_manifest(">=14.1.0|<15.0.0", None),
            "h",
            &any_host(),
            &[cargo_backend(&index)],
            &[],
            Some(&locked),
        )
        .unwrap();
        assert_eq!(resolved_version(&r), "14.1.1");
    }

    #[test]
    fn a_locked_version_no_longer_available_is_resolved_again() {
        let index = FakeIndex::new(Scheme::Semver, &["14.1.1"]);
        let locked = ripgrep_locked("pkg:cargo/ripgrep", "14.0.0", None);
        let r = resolve_with_lock(
            &ripgrep_manifest(">=14.0.0|<15.0.0", None),
            "h",
            &any_host(),
            &[cargo_backend(&index)],
            &[],
            Some(&locked),
        )
        .unwrap();
        assert_eq!(resolved_version(&r), "14.1.1");
    }

    #[test]
    fn a_pin_for_another_package_id_is_not_kept() {
        // The manifest now names a different crate for ripgrep.
        let index = FakeIndex::new(Scheme::Semver, &["14.0.0", "14.1.1"]);
        let locked = ripgrep_locked("pkg:cargo/ripgrep", "14.0.0", None);
        let r = resolve_with_lock(
            &ripgrep_manifest(">=14.0.0|<15.0.0", Some("ripgrep-fork")),
            "h",
            &any_host(),
            &[cargo_backend(&index)],
            &[],
            Some(&locked),
        )
        .unwrap();
        assert_eq!(resolved_version(&r), "14.1.1");
    }

    #[test]
    fn a_kept_pin_keeps_its_recorded_checksum() {
        let index = FakeIndex::new(Scheme::Semver, &["14.0.0", "14.1.1"]);
        let locked = ripgrep_locked("pkg:cargo/ripgrep", "14.0.0", Some("sha256:pinned"));
        let r = resolve_with_lock(
            &ripgrep_manifest(">=14.0.0|<15.0.0", None),
            "h",
            &any_host(),
            &[cargo_backend(&index)],
            &[],
            Some(&locked),
        )
        .unwrap();
        let leaf = &r.lock.packages[0].targets[&Target::Any];
        assert_eq!(leaf.checksum, Some(Checksum("sha256:pinned".into())));
    }

    /// An index whose package manager chooses a fixed version, recording
    /// what it was asked to choose within and to prefer.
    struct ChoosingIndex {
        chooses: Option<&'static str>,
        asked: RefCell<Option<(Want, Vec<Version>)>>,
    }
    impl Index for ChoosingIndex {
        fn refresh(&self) -> Result<(), PortError> {
            Ok(())
        }
        fn query_available(&self, _: &Ref, _: &Target) -> Result<Vec<Version>, PortError> {
            unreachable!("selection goes through select")
        }
        fn select(
            &self,
            _: &Ref,
            _: &Target,
            want: &Want,
            prefer: &[Version],
        ) -> Result<bosun_ports::Selection, PortError> {
            *self.asked.borrow_mut() = Some((want.clone(), prefer.to_vec()));
            let sem = |v: &str| Version::parse(v, Scheme::Semver).unwrap();
            Ok(bosun_ports::Selection {
                chosen: self.chooses.map(sem),
                available: vec![sem("14.1.1"), sem("14.0.0"), sem("15.0.0")],
            })
        }
    }

    fn choosing(chooses: Option<&'static str>) -> ChoosingIndex {
        ChoosingIndex {
            chooses,
            asked: RefCell::new(None),
        }
    }

    #[test]
    fn the_backends_own_choice_is_resolved_and_a_fitting_pin_offered_to_it() {
        let index = choosing(Some("14.0.0"));
        let locked = ripgrep_locked("pkg:cargo/ripgrep", "14.0.0", Some("sha256:pinned"));
        let r = resolve_with_lock(
            &ripgrep_manifest(">=14.0.0|<15.0.0", None),
            "h",
            &any_host(),
            &[cargo_backend(&index)],
            &[],
            Some(&locked),
        )
        .unwrap();
        assert_eq!(resolved_version(&r), "14.0.0");
        let leaf = &r.lock.packages[0].targets[&Target::Any];
        assert_eq!(leaf.checksum, Some(Checksum("sha256:pinned".into())));
        let (want, prefer) = index.asked.borrow().clone().unwrap();
        assert_eq!(want.summary(), "vers:cargo/>=14.0.0|<15.0.0");
        assert_eq!(
            prefer,
            vec![Version::parse("14.0.0", Scheme::Semver).unwrap()]
        );
    }

    #[test]
    fn a_choice_other_than_the_pin_drops_the_pins_checksum() {
        let index = choosing(Some("14.1.1"));
        let locked = ripgrep_locked("pkg:cargo/ripgrep", "14.0.0", Some("sha256:pinned"));
        let r = resolve_with_lock(
            &ripgrep_manifest(">=14.0.0|<15.0.0", None),
            "h",
            &any_host(),
            &[cargo_backend(&index)],
            &[],
            Some(&locked),
        )
        .unwrap();
        assert_eq!(resolved_version(&r), "14.1.1");
        assert_eq!(r.lock.packages[0].targets[&Target::Any].checksum, None);
    }

    #[test]
    fn a_pin_for_another_identity_is_not_offered() {
        let index = choosing(Some("14.1.1"));
        let locked = ripgrep_locked("pkg:cargo/ripgrep-fork", "14.0.0", None);
        resolve_with_lock(
            &ripgrep_manifest(">=14.0.0|<15.0.0", None),
            "h",
            &any_host(),
            &[cargo_backend(&index)],
            &[],
            Some(&locked),
        )
        .unwrap();
        assert_eq!(
            index.asked.borrow().as_ref().unwrap().1,
            Vec::<Version>::new()
        );
    }

    #[test]
    fn nothing_chosen_is_unsatisfiable_listing_what_was_considered_newest_first() {
        let index = choosing(None);
        let r = resolve(
            &ripgrep_manifest(">=16.0.0", None),
            "h",
            &any_host(),
            &[cargo_backend(&index)],
            &[],
        )
        .unwrap();
        assert_eq!(
            r.outcomes[0].outcome,
            Outcome::Unsatisfiable {
                spec: "vers:cargo/>=16.0.0".to_string(),
                available: "15.0.0, 14.1.1, 14.0.0".to_string(),
            }
        );
    }

    /// An index that reports a checksum for every version.
    struct SummedIndex(FakeIndex);
    impl Index for SummedIndex {
        fn refresh(&self) -> Result<(), PortError> {
            Ok(())
        }
        fn query_available(&self, pkg: &Ref, target: &Target) -> Result<Vec<Version>, PortError> {
            self.0.query_available(pkg, target)
        }
        fn checksum(
            &self,
            _pkg: &Ref,
            version: &Version,
            _target: &Target,
        ) -> Result<Option<Checksum>, PortError> {
            Ok(Some(Checksum(format!("sum-of-{}", version.as_str()))))
        }
    }

    #[test]
    fn a_kept_pin_with_no_recorded_checksum_gets_one() {
        // A lock written before checksums were recorded for every release.
        let index = SummedIndex(FakeIndex::new(Scheme::Semver, &["14.0.0", "14.1.1"]));
        let locked = ripgrep_locked("pkg:cargo/ripgrep", "14.0.0", None);
        let r = resolve_with_lock(
            &ripgrep_manifest(">=14.0.0|<15.0.0", None),
            "h",
            &any_host(),
            &[cargo_backend(&index)],
            &[],
            Some(&locked),
        )
        .unwrap();
        let leaf = &r.lock.packages[0].targets[&Target::Any];
        assert_eq!(leaf.resolved.as_str(), "14.0.0");
        assert_eq!(leaf.checksum, Some(Checksum("sum-of-14.0.0".into())));
    }

    #[test]
    fn without_a_lock_the_newest_in_the_spec_is_chosen() {
        let index = FakeIndex::new(Scheme::Semver, &["14.0.0", "14.1.1"]);
        let r = resolve_with_lock(
            &ripgrep_manifest(">=14.0.0|<15.0.0", None),
            "h",
            &any_host(),
            &[cargo_backend(&index)],
            &[],
            None,
        )
        .unwrap();
        assert_eq!(resolved_version(&r), "14.1.1");
    }

    #[test]
    fn github_binary_qualifier_defaults_to_the_logical_name() {
        // `- name: ltk` with `package: ctxloom/ctxloom` must pick the ltk binary.
        let host = Host {
            os: Os::Linux,
            arch: Arch::Amd64,
            distro: None,
            libc: None,
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
            libc: None,
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
            libc: None,
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
            libc: None,
        };
        let defaults = with_option(
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
            libc: None,
        };
        let m = manifest(
            &[],
            vec![entry(
                "duf",
                Some(">=0.8.0"),
                &[(
                    "linux",
                    with_option(block(&["cargo"], None, None), "cargo", "features", "x"),
                )],
            )],
        );

        resolve(&m, "sha256:h", &host, &[cargo_backend(&index)], &[]).unwrap();

        let (queried, _) = index.last.borrow().clone().unwrap();
        assert_eq!(queried.purl.to_string(), "pkg:cargo/duf?features=x");
    }

    /// `github-release` and `cargo` both acceptable, a shared `package`, and
    /// cargo's own under `backends.cargo.package`.
    fn fallback_with_per_backend_package() -> Manifest {
        let b = with_option(
            block(&["github-release", "cargo"], Some("muesli/duf"), None),
            "cargo",
            "package",
            "duf-cli",
        );
        manifest(&[], vec![entry("duf", Some(">=0.8.0"), &[("linux", b)])])
    }

    #[test]
    fn a_per_backend_package_names_the_package_when_that_backend_is_chosen() {
        let index = FakeIndex::new(Scheme::Semver, &["0.8.1"]);
        let host = Host {
            os: Os::Linux,
            arch: Arch::Amd64,
            distro: None,
            libc: None,
        };

        resolve(
            &fallback_with_per_backend_package(),
            "sha256:h",
            &host,
            &[cargo_backend(&index)],
            &[],
        )
        .unwrap();

        // The per-backend name is the identity, not a qualifier.
        let (queried, _) = index.last.borrow().clone().unwrap();
        assert_eq!(queried.purl.to_string(), "pkg:cargo/duf-cli");
    }

    #[test]
    fn another_backends_package_leaves_the_shared_package_in_force() {
        let index = FakeIndex::new(Scheme::Semver, &["0.8.1"]);
        let host = Host {
            os: Os::Linux,
            arch: Arch::Amd64,
            distro: None,
            libc: None,
        };

        resolve(
            &fallback_with_per_backend_package(),
            "sha256:h",
            &host,
            &[github_backend(&index)],
            &[],
        )
        .unwrap();

        let (queried, _) = index.last.borrow().clone().unwrap();
        assert_eq!(queried.purl.to_string(), "pkg:github/muesli/duf?binary=duf");
    }

    #[test]
    fn a_per_backend_package_alone_needs_no_shared_package() {
        let index = FakeIndex::new(Scheme::Semver, &["0.8.1"]);
        let host = Host {
            os: Os::Linux,
            arch: Arch::Amd64,
            distro: None,
            libc: None,
        };
        let b = with_option(block(&["cargo"], None, None), "cargo", "package", "duf-cli");
        let m = manifest(&[], vec![entry("duf", Some(">=0.8.0"), &[("linux", b)])]);

        resolve(&m, "sha256:h", &host, &[cargo_backend(&index)], &[]).unwrap();

        let (queried, _) = index.last.borrow().clone().unwrap();
        assert_eq!(queried.purl.to_string(), "pkg:cargo/duf-cli");
    }

    #[test]
    fn an_npm_scope_is_the_purls_namespace_and_named_with_it_again() {
        for (native, want, namespace, name) in [
            (
                "@biomejs/biome",
                "pkg:npm/%40biomejs/biome",
                Some("@biomejs"),
                "biome",
            ),
            ("cowsay", "pkg:npm/cowsay", None, "cowsay"),
        ] {
            let purl = build_purl("x", "npm", native, BTreeMap::new(), &any_host()).unwrap();
            assert_eq!(purl.to_string(), want);
            assert_eq!(purl.namespace(), namespace);
            assert_eq!(purl.name(), name);
            assert_eq!(purl.native_id(), native);
            let parsed: Purl = want.parse().unwrap();
            assert_eq!(parsed, purl);
        }
        // Only an `@` names a scope: any other slash stays in the name.
        let unscoped = build_purl("x", "npm", "a/b", BTreeMap::new(), &any_host()).unwrap();
        assert_eq!(unscoped.namespace(), None);
        assert_eq!(unscoped.name(), "a/b");
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
            libc: None,
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
            libc: None,
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
            base_codename: None,
        };
        let ubuntu_host = Host {
            os: Os::Linux,
            arch: Arch::Amd64,
            distro: Some(&ubuntu),
            libc: None,
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
            base_codename: None,
        };
        let fedora_host = Host {
            os: Os::Linux,
            arch: Arch::Amd64,
            distro: Some(&fedora),
            libc: None,
        };
        let on_fedora = resolve(&m, "h", &fedora_host, &backends, &[]).unwrap();
        assert!(matches!(
            &on_fedora.outcomes[0].outcome,
            Outcome::Resolved { backend, .. } if *backend == BackendId::new("cargo")
        ));
    }

    // --- release selectors (§5.4) --------------------------------------------

    fn linux(distro: Option<&Distro>) -> Host<'_> {
        Host {
            os: Os::Linux,
            arch: Arch::Amd64,
            distro,
            libc: None,
        }
    }

    fn trixie() -> Distro {
        Distro {
            version_id: Some("13".to_string()),
            version_codename: Some("trixie".to_string()),
            base_codename: None,
            ..debian()
        }
    }

    fn jammy() -> Distro {
        Distro {
            id: "ubuntu".to_string(),
            id_like: vec!["debian".to_string()],
            version_id: Some("22.04".to_string()),
            version_codename: Some("jammy".to_string()),
            base_codename: None,
        }
    }

    /// The spec of the block resolution picked, read off the resolved version:
    /// each block below admits exactly one of the published versions.
    fn picked(m: &Manifest, host: &Host<'_>, backends: &[ResolvableBackend<'_>]) -> String {
        match &resolve(m, "h", host, backends, &[]).unwrap().outcomes[0].outcome {
            Outcome::Resolved { version, .. } => version.as_str().to_string(),
            other => panic!("expected a resolution, got {other:?}"),
        }
    }

    /// A jq entry offering one apt block per selector, each pinned to its own
    /// version, so the resolved version names the block that won.
    fn jq_by_selector(selectors: &[(&str, &str)]) -> Manifest {
        let install: Vec<(&str, InstallBlock)> = selectors
            .iter()
            .map(|(s, v)| (*s, block(&["apt"], None, Some(v))))
            .collect();
        manifest(&[], vec![entry("jq", None, &install)])
    }

    const EVERY_JQ: &[&str] = &["0.1-1", "0.2-1", "0.3-1", "0.4-1", "0.5-1"];

    #[test]
    fn precedence_is_release_then_id_then_id_like_then_os_then_any() {
        let apt = FakeIndex::new(Scheme::Debian, EVERY_JQ);
        let backends = [deb_backend(&apt)];
        let distro = jammy();
        let host = linux(Some(&distro));
        let mut ladder = vec![
            ("any", "0.1-1"),
            ("linux", "0.2-1"),
            ("linux:debian", "0.3-1"),
            ("linux:ubuntu", "0.4-1"),
            ("linux:ubuntu@jammy", "0.5-1"),
        ];
        // Each rung beats every rung below it.
        while let Some((_, want)) = ladder.last().copied() {
            assert_eq!(picked(&jq_by_selector(&ladder), &host, &backends), want);
            ladder.pop();
        }
    }

    #[test]
    fn bookworm_and_trixie_pick_their_own_release_block() {
        let apt = FakeIndex::new(Scheme::Debian, EVERY_JQ);
        let backends = [deb_backend(&apt)];
        let m = jq_by_selector(&[
            ("linux:debian", "0.1-1"),
            ("linux:debian@bookworm", "0.2-1"),
            ("linux:debian@trixie", "0.3-1"),
        ]);
        let (bookworm, trixie) = (debian(), trixie());
        assert_eq!(picked(&m, &linux(Some(&bookworm)), &backends), "0.2-1");
        assert_eq!(picked(&m, &linux(Some(&trixie)), &backends), "0.3-1");
    }

    #[test]
    fn a_release_names_the_exact_id_never_an_id_like_family() {
        let apt = FakeIndex::new(Scheme::Debian, EVERY_JQ);
        let backends = [deb_backend(&apt)];
        let m = jq_by_selector(&[("linux", "0.1-1"), ("linux:debian@jammy", "0.2-1")]);
        let distro = jammy();
        assert_eq!(picked(&m, &linux(Some(&distro)), &backends), "0.1-1");
    }

    #[test]
    fn a_host_without_a_release_falls_back_never_errs() {
        let apt = FakeIndex::new(Scheme::Debian, EVERY_JQ);
        let backends = [deb_backend(&apt)];
        let m = jq_by_selector(&[
            ("linux:debian", "0.1-1"),
            ("linux:debian@bookworm", "0.2-1"),
        ]);
        let sid = Distro {
            version_id: None,
            version_codename: None,
            base_codename: None,
            ..debian()
        };
        assert_eq!(picked(&m, &linux(Some(&sid)), &backends), "0.1-1");

        // No os-release at all (scratch): only the OS-level blocks apply.
        let cargo = FakeIndex::new(Scheme::Semver, &["1.0.0", "2.0.0"]);
        let backends = [cargo_backend(&cargo)];
        let m = manifest(
            &[],
            vec![entry(
                "jq",
                None,
                &[
                    ("linux", block(&["cargo"], None, Some("1.0.0"))),
                    (
                        "linux:debian@bookworm",
                        block(&["cargo"], None, Some("2.0.0")),
                    ),
                ],
            )],
        );
        assert_eq!(picked(&m, &linux(None), &backends), "1.0.0");
    }

    #[test]
    fn a_release_only_matches_through_a_backend_whose_type_carries_one() {
        // github has no distro qualifier, so a bookworm block naming only
        // github-release never applies, even on bookworm.
        let apt = FakeIndex::new(Scheme::Debian, &["1.6-6"]);
        let gh = FakeIndex::new(Scheme::Semver, &["1.7.1"]);
        let backends = [deb_backend(&apt), github_backend(&gh)];
        let m = manifest(
            &[],
            vec![entry(
                "jq",
                Some("*"),
                &[
                    ("linux:debian", block(&["apt"], None, None)),
                    (
                        "linux:debian@bookworm",
                        block(&["github-release"], Some("jqlang/jq"), None),
                    ),
                ],
            )],
        );
        let distro = debian();
        assert_eq!(picked(&m, &linux(Some(&distro)), &backends), "1.6-6");
    }

    #[test]
    fn a_release_block_with_several_backends_applies_and_preference_still_chooses() {
        // apt carries the release, so the block applies; which of its
        // backends installs stays the machine's preference (§5.3).
        let apt = FakeIndex::new(Scheme::Debian, &["1.6-6"]);
        let gh = FakeIndex::new(Scheme::Semver, &["1.7.1"]);
        let backends = [deb_backend(&apt), github_backend(&gh)];
        let mut both = block(&["apt", "github-release"], None, None);
        both = with_option(both, "github-release", PACKAGE_OPTION, "jqlang/jq");
        let m = manifest(
            &[],
            vec![entry(
                "jq",
                Some("*"),
                &[
                    ("linux", block(&["apt"], None, Some("0.0-0"))),
                    ("linux:debian@bookworm", both),
                ],
            )],
        );
        let distro = debian();
        let host = linux(Some(&distro));
        let preferred = resolve(
            &m,
            "h",
            &host,
            &backends,
            &[BackendId::new("github-release")],
        );
        assert!(matches!(
            &preferred.unwrap().outcomes[0].outcome,
            Outcome::Resolved { backend, .. } if *backend == BackendId::new("github-release")
        ));
        assert_eq!(picked(&m, &host, &backends), "1.6-6");
    }

    #[test]
    fn a_release_backend_must_be_registered_here_to_carry_the_release() {
        // apt is named but not registered: nothing here spells the release.
        let cargo = FakeIndex::new(Scheme::Semver, &["1.0.0", "2.0.0"]);
        let backends = [cargo_backend(&cargo)];
        let m = manifest(
            &[],
            vec![entry(
                "jq",
                None,
                &[
                    ("linux", block(&["cargo"], None, Some("1.0.0"))),
                    (
                        "linux:debian@bookworm",
                        block(&["apt", "cargo"], None, Some("2.0.0")),
                    ),
                ],
            )],
        );
        let distro = debian();
        assert_eq!(picked(&m, &linux(Some(&distro)), &backends), "1.0.0");
    }

    #[test]
    fn a_release_block_naming_no_backend_matches_through_the_defaults_backends() {
        let apt = FakeIndex::new(Scheme::Debian, EVERY_JQ);
        let backends = [deb_backend(&apt)];
        let m = manifest(
            &[("linux:debian", block(&["apt"], None, None))],
            vec![entry(
                "jq",
                Some("0.1-1"),
                &[("linux:debian@bookworm", block(&[], None, Some("0.2-1")))],
            )],
        );
        let (bookworm, trixie) = (debian(), trixie());
        assert_eq!(picked(&m, &linux(Some(&bookworm)), &backends), "0.2-1");
        assert_eq!(picked(&m, &linux(Some(&trixie)), &backends), "0.1-1");
    }

    #[test]
    fn a_defaults_release_block_matches_through_its_own_backends() {
        let apt = FakeIndex::new(Scheme::Debian, EVERY_JQ);
        let backends = [deb_backend(&apt)];
        let m = manifest(
            &[
                ("linux:debian", block(&["apt"], None, Some("0.1-1"))),
                (
                    "linux:debian@bookworm",
                    block(&["apt"], None, Some("0.2-1")),
                ),
            ],
            vec![entry("jq", None, &[])],
        );
        let distro = debian();
        assert_eq!(picked(&m, &linux(Some(&distro)), &backends), "0.2-1");
    }

    #[test]
    fn an_rpm_release_is_id_dash_version_id() {
        let dnf = FakeIndex::new(Scheme::Rpm, &["1.6-1", "1.7.1-1"]);
        let backends = [rpm_backend(&dnf)];
        let m = manifest(
            &[],
            vec![entry(
                "jq",
                None,
                &[
                    ("linux:fedora", block(&["dnf"], None, Some("1.7.1-1"))),
                    (
                        "linux:fedora@fedora-41",
                        block(&["dnf"], None, Some("1.6-1")),
                    ),
                    ("linux:fedora@41", block(&["dnf"], None, Some("0"))),
                ],
            )],
        );
        let fedora = Distro {
            id: "fedora".to_string(),
            id_like: Vec::new(),
            version_id: Some("41".to_string()),
            version_codename: None,
            base_codename: None,
        };
        assert_eq!(picked(&m, &linux(Some(&fedora)), &backends), "1.6-1");
    }

    #[test]
    fn a_release_selector_for_another_os_never_applies() {
        let apt = FakeIndex::new(Scheme::Debian, EVERY_JQ);
        let backends = [deb_backend(&apt)];
        let m = jq_by_selector(&[("any", "0.1-1"), ("windows:debian@bookworm", "0.2-1")]);
        let distro = debian();
        assert_eq!(picked(&m, &linux(Some(&distro)), &backends), "0.1-1");
    }

    // --- libc selectors (§5.4) ------------------------------------------------

    fn with_libc<'a>(distro: Option<&'a Distro>, libc: Option<&'a Libc>) -> Host<'a> {
        Host {
            libc,
            ..linux(distro)
        }
    }

    fn libc(family: LibcFamily, version: Option<&str>) -> Libc {
        Libc {
            family,
            version: version.map(str::to_string),
        }
    }

    fn alpine() -> Distro {
        Distro {
            id: "alpine".to_string(),
            id_like: Vec::new(),
            version_id: Some("3.20.3".to_string()),
            version_codename: None,
            base_codename: None,
        }
    }

    const SEVEN_JQ: &[&str] = &[
        "0.1-1", "0.2-1", "0.3-1", "0.4-1", "0.5-1", "0.6-1", "0.7-1",
    ];

    #[test]
    fn libc_ranks_between_id_like_and_plain_os_and_a_range_beats_the_bare_family() {
        let apt = FakeIndex::new(Scheme::Debian, SEVEN_JQ);
        let backends = [deb_backend(&apt)];
        let distro = jammy();
        let glibc = libc(LibcFamily::Gnu, Some("2.35"));
        let host = with_libc(Some(&distro), Some(&glibc));
        let mut ladder = vec![
            ("any", "0.1-1"),
            ("linux", "0.2-1"),
            ("linux:gnu", "0.3-1"),
            ("linux:gnu@>=2.31", "0.4-1"),
            ("linux:debian", "0.5-1"),
            ("linux:ubuntu", "0.6-1"),
            ("linux:ubuntu@jammy", "0.7-1"),
        ];
        while let Some((_, want)) = ladder.last().copied() {
            assert_eq!(picked(&jq_by_selector(&ladder), &host, &backends), want);
            ladder.pop();
        }
    }

    #[test]
    fn a_distro_with_no_key_of_its_own_falls_through_to_its_libc() {
        let apt = FakeIndex::new(Scheme::Debian, EVERY_JQ);
        let backends = [deb_backend(&apt)];
        let m = jq_by_selector(&[
            ("linux", "0.1-1"),
            ("linux:gnu", "0.2-1"),
            ("linux:musl", "0.3-1"),
            ("linux:debian", "0.4-1"),
        ]);
        let (alpine, musl) = (alpine(), libc(LibcFamily::Musl, Some("1.2.5")));
        assert_eq!(
            picked(&m, &with_libc(Some(&alpine), Some(&musl)), &backends),
            "0.3-1"
        );
    }

    #[test]
    fn libc_applies_on_a_machine_with_no_distro() {
        // A scratch image with a libc: cargo needs no distro for its identity.
        let cargo = FakeIndex::new(Scheme::Semver, &["0.1.0", "0.2.0"]);
        let backends = [cargo_backend(&cargo)];
        let m = manifest(
            &[],
            vec![entry(
                "jq",
                None,
                &[
                    ("linux", block(&["cargo"], None, Some("0.1.0"))),
                    ("linux:musl", block(&["cargo"], None, Some("0.2.0"))),
                ],
            )],
        );
        let musl = libc(LibcFamily::Musl, Some("1.2.5"));
        assert_eq!(
            picked(&m, &with_libc(None, Some(&musl)), &backends),
            "0.2.0"
        );
    }

    #[test]
    fn a_glibc_older_than_the_floor_falls_back_to_plain_linux() {
        let apt = FakeIndex::new(Scheme::Debian, EVERY_JQ);
        let backends = [deb_backend(&apt)];
        let alpine = alpine();
        let m = jq_by_selector(&[("linux", "0.1-1"), ("linux:gnu@>=2.28", "0.2-1")]);
        let old = libc(LibcFamily::Gnu, Some("2.17"));
        let new = libc(LibcFamily::Gnu, Some("2.28"));
        assert_eq!(
            picked(&m, &with_libc(Some(&alpine), Some(&old)), &backends),
            "0.1-1"
        );
        assert_eq!(
            picked(&m, &with_libc(Some(&alpine), Some(&new)), &backends),
            "0.2-1"
        );
    }

    #[test]
    fn an_unknown_libc_matches_no_libc_selector() {
        let apt = FakeIndex::new(Scheme::Debian, EVERY_JQ);
        let backends = [deb_backend(&apt)];
        let alpine = alpine();
        let m = jq_by_selector(&[
            ("linux", "0.1-1"),
            ("linux:gnu", "0.2-1"),
            ("linux:musl", "0.3-1"),
        ]);
        assert_eq!(
            picked(&m, &with_libc(Some(&alpine), None), &backends),
            "0.1-1"
        );
    }

    #[test]
    fn an_unknown_libc_version_matches_the_family_but_no_range() {
        let apt = FakeIndex::new(Scheme::Debian, EVERY_JQ);
        let backends = [deb_backend(&apt)];
        let alpine = alpine();
        let m = jq_by_selector(&[
            ("linux", "0.1-1"),
            ("linux:gnu", "0.2-1"),
            ("linux:gnu@>=2.0", "0.3-1"),
        ]);
        let versionless = libc(LibcFamily::Gnu, None);
        assert_eq!(
            picked(&m, &with_libc(Some(&alpine), Some(&versionless)), &backends),
            "0.2-1"
        );
    }

    #[test]
    fn the_other_family_never_applies() {
        let apt = FakeIndex::new(Scheme::Debian, EVERY_JQ);
        let backends = [deb_backend(&apt)];
        let alpine = alpine();
        let m = jq_by_selector(&[("any", "0.1-1"), ("linux:musl", "0.2-1")]);
        let glibc = libc(LibcFamily::Gnu, Some("2.36"));
        assert_eq!(
            picked(&m, &with_libc(Some(&alpine), Some(&glibc)), &backends),
            "0.1-1"
        );
    }

    #[test]
    fn deb_identity_omits_a_blank_codename() {
        let apt = FakeIndex::new(Scheme::Debian, &["1.0-1"]);
        let blank = Distro {
            version_codename: Some(String::new()),
            base_codename: None,
            ..debian()
        };
        let host = linux(Some(&blank));
        let m = jq_by_selector(&[("linux", "1.0-1")]);
        let r = resolve(&m, "h", &host, &[deb_backend(&apt)], &[]).unwrap();
        assert_eq!(
            r.lock.packages[0].targets[&AMD64]
                .placement
                .purl
                .to_string(),
            "pkg:deb/debian/jq?arch=amd64"
        );
    }

    fn apk_backend(index: &dyn Index) -> ResolvableBackend<'_> {
        ResolvableBackend {
            id: BackendId::new("apk"),
            purl_type: "apk".to_string(),
            vers_type: "apk".to_string(),
            platform_independent: false,
            index,
        }
    }

    fn apk_purl(arch: Arch, distro: Option<&Distro>) -> Result<String, ResolveError> {
        let apk = FakeIndex::new(Scheme::Alpine, &["1.7.1-r0"]);
        let host = Host {
            os: Os::Linux,
            arch,
            distro,
            libc: None,
        };
        let m = manifest(
            &[],
            vec![entry(
                "jq",
                Some("1.7.1-r0"),
                &[("linux", block(&["apk"], None, None))],
            )],
        );
        let r = resolve(&m, "h", &host, &[apk_backend(&apk)], &[])?;
        let target = Target::Platform {
            os: Os::Linux,
            arch,
        };
        Ok(r.lock.packages[0].targets[&target]
            .placement
            .purl
            .to_string())
    }

    #[test]
    fn apk_identity_names_the_distro_and_apk_arch_but_no_release() {
        let distro = alpine();
        assert_eq!(
            apk_purl(Arch::Amd64, Some(&distro)).unwrap(),
            "pkg:apk/alpine/jq?arch=x86_64"
        );
        assert_eq!(
            apk_purl(Arch::Arm64, Some(&distro)).unwrap(),
            "pkg:apk/alpine/jq?arch=aarch64"
        );
    }

    #[test]
    fn an_apk_identity_without_distro_facts_is_an_error() {
        assert_eq!(
            apk_purl(Arch::Amd64, None).unwrap_err(),
            ResolveError::NoDistro {
                name: "jq".to_string()
            }
        );
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
            libc: None,
        };
        let m = manifest(
            &[],
            vec![entry(
                "fd",
                Some(">=1.0.0"),
                &[("linux", block(&["apt", "cargo"], None, None))],
            )],
        );

        // No preference: the order the backends are registered in → apt.
        let default = resolve(&m, "h", &host, &backends, &[]).unwrap();
        assert!(matches!(
            &default.outcomes[0].outcome,
            Outcome::Resolved { backend, .. } if *backend == BackendId::new("apt")
        ));
        let swapped = [cargo_backend(&cargo), deb_backend(&apt)];
        let default = resolve(&m, "h", &host, &swapped, &[]).unwrap();
        assert!(matches!(
            &default.outcomes[0].outcome,
            Outcome::Resolved { backend, .. } if *backend == BackendId::new("cargo")
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
            libc: None,
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
    fn the_any_selector_applies_on_every_os() {
        let index = FakeIndex::new(Scheme::Semver, &["1.0.0"]);
        let m = manifest(
            &[],
            vec![entry(
                "fd",
                Some(">=1.0.0"),
                &[("any", block(&["cargo"], None, None))],
            )],
        );
        for os in [Os::Linux, Os::Macos, Os::Windows] {
            let host = Host {
                os,
                arch: Arch::Amd64,
                distro: None,
                libc: None,
            };
            let r = resolve(&m, "h", &host, &[cargo_backend(&index)], &[]).unwrap();
            assert!(
                matches!(&r.outcomes[0].outcome, Outcome::Resolved { .. }),
                "{os}: {:?}",
                r.outcomes[0].outcome
            );
        }
    }

    #[test]
    fn an_os_selector_beats_any() {
        let apt = FakeIndex::new(Scheme::Debian, &["1.0.0"]);
        let cargo = FakeIndex::new(Scheme::Semver, &["1.0.0"]);
        let backends = [deb_backend(&apt), cargo_backend(&cargo)];
        let distro = debian();
        let host = Host {
            os: Os::Linux,
            arch: Arch::Amd64,
            distro: Some(&distro),
            libc: None,
        };
        let m = manifest(
            &[],
            vec![entry(
                "fd",
                Some(">=1.0.0"),
                &[
                    ("any", block(&["cargo"], None, None)),
                    ("linux", block(&["apt"], None, None)),
                ],
            )],
        );

        let r = resolve(&m, "h", &host, &backends, &[]).unwrap();
        assert!(matches!(
            &r.outcomes[0].outcome,
            Outcome::Resolved { backend, .. } if *backend == BackendId::new("apt")
        ));
    }

    #[test]
    fn an_entrys_any_block_beats_a_defaults_os_block() {
        // Entry fields win over defaults, whatever the selectors: `prettier`
        // asks for npm everywhere, so a defaults `linux: apt` doesn't apply.
        let apt = FakeIndex::new(Scheme::Debian, &["1.0.0"]);
        let cargo = FakeIndex::new(Scheme::Semver, &["1.0.0"]);
        let backends = [deb_backend(&apt), cargo_backend(&cargo)];
        let distro = debian();
        let host = Host {
            os: Os::Linux,
            arch: Arch::Amd64,
            distro: Some(&distro),
            libc: None,
        };
        let m = manifest(
            &[("linux", block(&["apt"], None, None))],
            vec![entry(
                "fd",
                Some(">=1.0.0"),
                &[("any", block(&["cargo"], None, None))],
            )],
        );

        let r = resolve(&m, "h", &host, &backends, &[]).unwrap();
        assert!(matches!(
            &r.outcomes[0].outcome,
            Outcome::Resolved { backend, .. } if *backend == BackendId::new("cargo")
        ));
    }

    #[test]
    fn a_named_backend_inherits_its_defaults_options_per_key() {
        let index = FakeIndex::new(Scheme::Semver, &["0.8.1"]);
        let host = Host {
            os: Os::Linux,
            arch: Arch::Amd64,
            distro: None,
            libc: None,
        };
        // defaults: github-release's tag template; the entry names github-release
        // with its own asset and package.
        let defaults = with_option(
            block(&[], None, None),
            "github-release",
            "tag",
            "v{{ version }}",
        );
        let own = with_option(
            with_asset(block(&[], None, None), "duf.tar.gz"),
            "github-release",
            "package",
            "muesli/duf",
        );
        let m = manifest(
            &[("linux", defaults)],
            vec![entry("duf", Some(">=0.8.0"), &[("linux", own)])],
        );

        resolve(&m, "sha256:h", &host, &[github_backend(&index)], &[]).unwrap();

        let (queried, _) = index.last.borrow().clone().unwrap();
        assert_eq!(queried.purl.name(), "duf");
        let q = queried.purl.qualifiers();
        assert_eq!(q.get("asset").map(String::as_str), Some("duf.tar.gz"));
        assert_eq!(q.get("tag").map(String::as_str), Some("v{{ version }}"));
        assert_eq!(q.get("package"), None);
    }

    #[test]
    fn not_applicable_when_no_selector_matches() {
        let index = FakeIndex::new(Scheme::Semver, &["1.0.0"]);
        let host = Host {
            os: Os::Linux,
            arch: Arch::Amd64,
            distro: None,
            libc: None,
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
    }

    #[test]
    fn not_applicable_when_no_named_backend_is_registered() {
        let apt = FakeIndex::new(Scheme::Debian, &["1.0-1"]);
        let distro = debian();
        let host = Host {
            os: Os::Linux,
            arch: Arch::Amd64,
            distro: Some(&distro),
            libc: None,
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
            libc: None,
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
    fn applicable_names_the_entries_a_registered_backend_installs_here() {
        let apt = FakeIndex::new(Scheme::Debian, &["1.0-1"]);
        let distro = debian();
        let host = Host {
            os: Os::Linux,
            arch: Arch::Amd64,
            distro: Some(&distro),
            libc: None,
        };
        let m = manifest(
            &[],
            vec![
                entry("jq", Some("*"), &[("linux", block(&["apt"], None, None))]),
                entry("fd", Some("*"), &[("linux", block(&["brew"], None, None))]),
                entry("bat", Some("*"), &[("darwin", block(&["apt"], None, None))]),
                entry("tree", Some("*"), &[("linux", block(&[], Some("t"), None))]),
            ],
        );
        assert_eq!(applicable(&m, &host, &[deb_backend(&apt)], &[]), ["jq"]);
    }

    #[test]
    fn unsatisfiable_when_nothing_available_is_in_the_set() {
        let index = FakeIndex::new(Scheme::Semver, &["1.0.0", "2.0.0"]);
        let host = Host {
            os: Os::Linux,
            arch: Arch::Amd64,
            distro: None,
            libc: None,
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
            libc: None,
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
            libc: None,
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
            libc: None,
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

    fn rpm_backend(index: &dyn Index) -> ResolvableBackend<'_> {
        ResolvableBackend {
            id: BackendId::new("dnf"),
            purl_type: "rpm".to_string(),
            vers_type: "rpm".to_string(),
            platform_independent: false,
            index,
        }
    }

    fn rpm_distro(id: &str, id_like: &[&str], version_id: Option<&str>) -> Distro {
        Distro {
            id: id.to_string(),
            id_like: id_like.iter().map(|s| s.to_string()).collect(),
            version_id: version_id.map(str::to_string),
            version_codename: None,
            base_codename: None,
        }
    }

    /// The identity and version `jq` resolves to through dnf on `distro`/`arch`.
    fn rpm_identity(distro: Option<&Distro>, arch: Arch) -> Result<(String, String), ResolveError> {
        let dnf = FakeIndex::new(Scheme::Rpm, &["1.7.1-8.fc41", "1.7.1-10.fc41", "1:1.6-1"]);
        let host = Host {
            os: Os::Linux,
            arch,
            distro,
            libc: None,
        };
        let m = manifest(
            &[],
            vec![entry(
                "jq",
                Some("<1:0"),
                &[("linux", block(&["dnf"], None, None))],
            )],
        );
        let r = resolve(&m, "h", &host, &[rpm_backend(&dnf)], &[])?;
        let leaf = &r.lock.packages[0].targets[&Target::Platform {
            os: Os::Linux,
            arch,
        }];
        Ok((
            leaf.placement.purl.to_string(),
            leaf.resolved.as_str().to_string(),
        ))
    }

    #[test]
    fn an_rpm_identity_names_the_vendor_rpm_arch_and_release() {
        let fedora = rpm_distro("fedora", &[], Some("41"));
        assert_eq!(
            rpm_identity(Some(&fedora), Arch::Amd64).unwrap(),
            (
                "pkg:rpm/fedora/jq?arch=x86_64&distro=fedora-41".to_string(),
                // rpm order: release 10 is above 8; the epoch-1 build is
                // excluded by the spec.
                "1.7.1-10.fc41".to_string()
            )
        );
        let rocky = rpm_distro("rocky", &["rhel", "centos", "fedora"], Some("9.8"));
        assert_eq!(
            rpm_identity(Some(&rocky), Arch::Arm64).unwrap().0,
            "pkg:rpm/rocky/jq?arch=aarch64&distro=rocky-9.8"
        );
    }

    #[test]
    fn an_rpm_identity_omits_the_distro_qualifier_without_a_version_id() {
        let rolling = rpm_distro("fedora", &[], None);
        assert_eq!(
            rpm_identity(Some(&rolling), Arch::Amd64).unwrap().0,
            "pkg:rpm/fedora/jq?arch=x86_64"
        );
    }

    #[test]
    fn an_rpm_identity_without_distro_facts_is_an_error() {
        assert_eq!(
            rpm_identity(None, Arch::Amd64).unwrap_err(),
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
            libc: None,
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
            base_codename: None,
        };
        let host = Host {
            os: Os::Linux,
            arch: Arch::Amd64,
            distro: Some(&rolling),
            libc: None,
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
            libc: None,
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

    // --- multi-claim and frozen ----------------------------------------------

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

    /// [`select_across_claims`] over an index offering `available`.
    fn across(
        claims: &[Claim],
        available: &[Version],
        lock_preference: &[Version],
    ) -> Result<Version, ClaimError> {
        let index = FakeIndex {
            versions: available.to_vec(),
            last: RefCell::new(None),
        };
        let pkg = Ref::new("pkg:deb/debian/jq".parse().unwrap());
        let target = Target::Platform {
            os: Os::Linux,
            arch: Arch::Amd64,
        };
        let chosen = select_across_claims(claims, "deb", &index, &pkg, &target, lock_preference);
        assert_eq!(*index.last.borrow(), Some((pkg, target)));
        chosen
    }

    #[test]
    fn select_across_claims_reports_a_failing_index() {
        let claims = [claim("/w/a/bosun.yaml", ">=1.7", None)];
        let pkg = Ref::new("pkg:deb/debian/jq".parse().unwrap());
        let err =
            select_across_claims(&claims, "deb", &FailIndex, &pkg, &Target::Any, &[]).unwrap_err();
        assert!(matches!(err, ClaimError::Query(_)), "{err:?}");
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
        let chosen = across(&claims, &available, &[]).unwrap();
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
        let chosen = across(&claims, &available, &[deb("1.7.1-3")]).unwrap();
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
        let chosen = across(&claims, &available, &[deb("1.8.0-1")]).unwrap();
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
        let err = across(&claims, &available, &[]).unwrap_err();
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

        let with_alice = across(&[babbitt.clone(), alice], &available, &[]).unwrap();
        assert_eq!(with_alice.as_str(), "1.7.0-1");
        // Without alice's claim, babbitt alone would take the higher version.
        let alone = across(&[babbitt], &available, &[]).unwrap();
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
}
