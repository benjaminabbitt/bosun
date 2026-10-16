//! bosun plan building and the privilege check (§4.8, §5.5) — the read-only
//! half of every machine-changing command (probe → load state → **plan** →
//! **privilege check** → apply). Applying a plan to the host is slice 9-2; the
//! container renderer is a peer driver (§8), out of M1.
//!
//! A [`Plan`] is a value: the same thing `--format json` prints and apply
//! executes. It carries ordered [`Action`]s — each with its scope, backend,
//! identity, from→to versions, the claims that drove it, and **its [`Steps`]**
//! (the exact commands) — plus [`Diagnostic`]s. Ordering is fixed (installs,
//! upgrades, downgrades, then holds) so a renamed package's old version is gone
//! before the new one lands; removals live only in the uninstall and prune
//! plans (§4.8). [`Plan::exit_code`] mirrors `terraform plan -detailed-exitcode`
//! (0 no changes, 2 changes pending, 1 error).
//!
//! The plan serialises for `--format json`; it holds domain types for apply to
//! execute, and the JSON view lives here (bosun-io owns lock/state wire format,
//! this owns the plan's). Serialisation is one-way — apply runs the in-memory
//! value, so there is no `Deserialize` (and no version-scheme to reconstruct).

mod applier;
mod errmsg;
mod view;

pub use applier::{ActionOutcome, ApplyReport, ApplyResult, Driver, HostApplier};

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use bosun_core::record::StateDocument;
use bosun_core::spec::Spec;
use bosun_core::{BackendId, Checksum, Claim, HoldSupport, Origin, Purl, Ref, Scope, Version};
use bosun_ports::error::PortError;
use bosun_ports::step::{Step, Steps};
use bosun_ports::{Installer, Machine, Privilege};

/// What an action does (§4.8). M1 covers these; `migrate`, `adopt` and
/// `ensure-source` are out of M1 scope.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ActionKind {
    /// Install a package not present.
    Install,
    /// Move an installed package up to the resolved version.
    Upgrade,
    /// Move an installed package down to the resolved version.
    Downgrade,
    /// Record a claim on a package already present and satisfying (origin
    /// `accepted`): no change to the machine.
    Accept,
    /// Durably pin an installed package at its version (`apt-mark hold`).
    Hold,
    /// Uninstall a package bosun installed, now unclaimed (uninstall/prune).
    Remove,
    /// Drop a manifest's claim on a package that stays installed (another
    /// manifest still wants it, or it was only accepted).
    DropClaim,
    /// Record a manifest's claim on a package bosun installed that is already
    /// at the resolved version (another project installed it, or this one's
    /// spec changed): no change to the machine, version or origin.
    Claim,
}

impl ActionKind {
    /// Whether this action mutates the machine through the package manager and
    /// so is subject to the privilege check (§5.5). `Accept`, `Claim` and
    /// `DropClaim` are state-only, so an all-accepted plan needs no elevation.
    pub fn changes_machine(self) -> bool {
        matches!(
            self,
            ActionKind::Install
                | ActionKind::Upgrade
                | ActionKind::Downgrade
                | ActionKind::Hold
                | ActionKind::Remove
        )
    }

    /// The lowercase label used in the plan (`install`, `drop-claim`, …).
    pub fn label(self) -> &'static str {
        match self {
            ActionKind::Install => "install",
            ActionKind::Upgrade => "upgrade",
            ActionKind::Downgrade => "downgrade",
            ActionKind::Accept => "accept",
            ActionKind::Hold => "hold",
            ActionKind::Remove => "remove",
            ActionKind::DropClaim => "drop-claim",
            ActionKind::Claim => "claim",
        }
    }
}

/// One planned action (§4.8). Carries everything apply and `--format json`
/// need: what to do, to what, under which scope/backend, the claims behind it,
/// and the exact [`Steps`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Action {
    /// What this action does.
    pub kind: ActionKind,
    /// bosun's logical name for the package.
    pub name: String,
    /// The install scope (drives the privilege check and the state file).
    pub scope: Scope,
    /// The backend performing it.
    pub backend: BackendId,
    /// The package's canonical unversioned identity (§5.6).
    pub purl: Purl,
    /// The version before (`None` for a fresh install or a hold).
    pub from: Option<Version>,
    /// The version after (`None` for a removal or a dropped claim).
    pub to: Option<Version>,
    /// The claims that drove this action (for a `DropClaim`/`Remove`, the
    /// claims being dropped).
    pub claims: Vec<Claim>,
    /// The exact commands (empty for `Accept`/`DropClaim`, which change no
    /// machine state).
    pub steps: Steps,
}

impl Action {
    /// Whether this action needs elevation: a machine-changing action in system
    /// scope (§5.5).
    pub fn needs_elevation(&self) -> bool {
        self.scope == Scope::System && self.kind.changes_machine()
    }
}

/// How serious a [`Diagnostic`] is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Severity {
    /// Blocks apply (§4.8).
    Error,
    /// Reported, does not block.
    Warning,
}

/// A note attached to the plan (§4.8): why an entry did not become an action,
/// or a condition the user should see. Errors block apply.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Diagnostic {
    /// No `install` selector matched this machine, or a matched block named no
    /// backend (§5.3). A warning.
    NotApplicable {
        /// The entry's name.
        name: String,
    },
    /// The entry's backends are none of them registered here (§5.3). A warning.
    MissingBackend {
        /// The entry's name.
        name: String,
        /// The backends it named.
        candidates: Vec<BackendId>,
    },
    /// No available version is in the spec's set (§7). An error.
    Unsatisfiable {
        /// The entry's name.
        name: String,
        /// The spec, canonical vers form.
        spec: String,
        /// The versions available.
        available: String,
    },
    /// Claims on the package cannot be jointly satisfied (§4.6). An error.
    EmptyIntersection {
        /// The entry's name.
        name: String,
        /// The claims that disagree, `manifest=spec` comma-separated.
        claims: String,
    },
    /// A package present outside bosun no longer satisfies its spec (§4.6): a
    /// warning; bosun changes nothing.
    AcceptedUnsatisfied {
        /// The entry's name.
        name: String,
        /// The version actually installed.
        installed: String,
        /// The spec it no longer satisfies.
        spec: String,
    },
    /// The backend cannot durably hold the version it installed (§5.2): a
    /// warning; the install still happens, just unpinned.
    UnsupportedHold {
        /// The entry's name.
        name: String,
        /// The backend that cannot hold.
        backend: BackendId,
    },
    /// The backend cannot move a package down to the resolved (lower) version
    /// (§5.2): an error for that package; apply is blocked.
    UnsupportedDowngrade {
        /// The entry's name.
        name: String,
        /// The backend that cannot downgrade.
        backend: BackendId,
    },
    /// Under `--frozen`, a package present but not bosun-owned (`accepted`) is
    /// installed at a different version than the lock pins (§4.6, §7). An error;
    /// bosun will not change a package it does not own.
    FrozenAcceptedConflict {
        /// The entry's name.
        name: String,
        /// The version actually installed, not owned by bosun.
        installed: String,
        /// The version the lockfile pins.
        locked: String,
    },
}

impl Diagnostic {
    /// Whether this diagnostic blocks apply (§4.8).
    pub fn severity(&self) -> Severity {
        match self {
            Diagnostic::Unsatisfiable { .. }
            | Diagnostic::EmptyIntersection { .. }
            | Diagnostic::UnsupportedDowngrade { .. }
            | Diagnostic::FrozenAcceptedConflict { .. } => Severity::Error,
            Diagnostic::NotApplicable { .. }
            | Diagnostic::MissingBackend { .. }
            | Diagnostic::AcceptedUnsatisfied { .. }
            | Diagnostic::UnsupportedHold { .. } => Severity::Warning,
        }
    }
}

impl std::fmt::Display for Diagnostic {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Diagnostic::NotApplicable { name } => write!(f, "{name}: {}", errmsg::NOT_APPLICABLE),
            Diagnostic::MissingBackend { name, candidates } => {
                let named = candidates
                    .iter()
                    .map(BackendId::as_str)
                    .collect::<Vec<_>>()
                    .join(", ");
                write!(f, "{name}: {} ({named})", errmsg::MISSING_BACKEND)
            }
            Diagnostic::Unsatisfiable {
                name,
                spec,
                available,
            } => write!(
                f,
                "{name}: {} {spec} (available: {available})",
                errmsg::UNSATISFIABLE
            ),
            Diagnostic::EmptyIntersection { name, claims } => {
                write!(f, "{name}: {} ({claims})", errmsg::EMPTY_INTERSECTION)
            }
            Diagnostic::AcceptedUnsatisfied {
                name,
                installed,
                spec,
            } => write!(
                f,
                "{name}: {installed} {} ({spec})",
                errmsg::ACCEPTED_UNSATISFIED
            ),
            Diagnostic::UnsupportedHold { name, backend } => {
                write!(
                    f,
                    "{name}: {} ({})",
                    errmsg::UNSUPPORTED_HOLD,
                    backend.as_str()
                )
            }
            Diagnostic::UnsupportedDowngrade { name, backend } => write!(
                f,
                "{name}: {} ({})",
                errmsg::UNSUPPORTED_DOWNGRADE,
                backend.as_str()
            ),
            Diagnostic::FrozenAcceptedConflict {
                name,
                installed,
                locked,
            } => write!(
                f,
                "{name}: installed {installed}, lock pins {locked}: {}",
                errmsg::FROZEN_ACCEPTED_CONFLICT
            ),
        }
    }
}

/// A whole plan (§4.8): the ordered actions and the diagnostics.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Plan {
    /// Actions in apply order (installs, upgrades, downgrades, accepts, holds;
    /// or removals and dropped claims for uninstall/prune).
    pub actions: Vec<Action>,
    /// Diagnostics; any [`Severity::Error`] blocks apply.
    pub diagnostics: Vec<Diagnostic>,
}

impl Plan {
    /// Whether any diagnostic is an error that blocks apply (§4.8).
    pub fn has_errors(&self) -> bool {
        self.diagnostics
            .iter()
            .any(|d| d.severity() == Severity::Error)
    }

    /// The `terraform plan -detailed-exitcode` style code (§4.8): `1` if any
    /// diagnostic is an error, else `2` if any action changes a package
    /// (install/upgrade/downgrade/hold/remove), else `0`. Pure `accept`,
    /// `claim` and `drop-claim` are shown and applied but do not count, so a drift check on
    /// a machine whose tools are already present exits `0`.
    pub fn exit_code(&self) -> i32 {
        if self.has_errors() {
            1
        } else if self.actions.iter().any(|a| a.kind.changes_machine()) {
            2
        } else {
            0
        }
    }
}

// --- the install plan (§4.8) -----------------------------------------------

/// The current state of a package on the machine, as the plan builder sees it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Current {
    /// Not installed.
    Absent,
    /// Installed; `version` is `None` when the backend cannot read it and state
    /// did not record one (§4.6). `origin` is how it came to be present.
    Present {
        /// The installed version, if known.
        version: Option<Version>,
        /// How it came to be present.
        origin: Origin,
        /// The claims bosun state records on it; empty when state doesn't
        /// record it (found present on the machine).
        claimed_by: Vec<Claim>,
    },
}

/// One package resolution decided to manage, with the facts the plan needs: the
/// version to end at, its identity, the claims driving it, its current state,
/// and the backend installer that produces its steps and reports its scope.
pub struct Managed<'a> {
    /// bosun's logical name.
    pub name: String,
    /// The canonical unversioned identity (§5.6).
    pub purl: Purl,
    /// The backend that installs it.
    pub backend: BackendId,
    /// The version resolution chose (§7).
    pub resolved: Version,
    /// The vers type claims parse under, for the accept-satisfaction check.
    pub vers_type: String,
    /// The claims driving this package (current project and others).
    pub claims: Vec<Claim>,
    /// Its current state on the machine.
    pub current: Current,
    /// The backend installer: scope, capabilities, and step production.
    pub installer: &'a dyn Installer,
    /// The checksum this machine's lock recorded for `resolved` (the
    /// published one, or the first download's), when it has one. Every
    /// download of the install is verified against it.
    pub checksum: Option<Checksum>,
}

/// Whether `recorded` (bosun state) already holds exactly the claims `wanted`:
/// the same manifests asking the same specs. The user state stamps on a
/// system-scope claim is not part of what the manifest asks.
fn same_claims(recorded: &[Claim], wanted: &[Claim]) -> bool {
    let asks = |claims: &[Claim]| -> BTreeSet<(PathBuf, String)> {
        claims
            .iter()
            .map(|c| (c.manifest.clone(), c.spec.clone()))
            .collect()
    };
    asks(recorded) == asks(wanted)
}

/// Does `version` satisfy every claim's vers set?
fn satisfies_all(version: &Version, claims: &[Claim], vers_type: &str) -> bool {
    claims
        .iter()
        .all(|c| Spec::new(&c.spec, vers_type).is_ok_and(|spec| spec.range().contains(version)))
}

/// Build the install plan (§4.8): compare each managed package's resolved
/// version to its current state, emitting `install`/`upgrade`/`downgrade`
/// (each followed by a `hold` on a hold-capable backend), or `accept` for a
/// present-and-satisfying accepted package. `carried` are resolution's own
/// diagnostics (not-applicable, missing-backend, unsatisfiable, empty
/// intersection); the builder adds `accepted-unsatisfied`. Actions come out in
/// fixed order: installs, upgrades, downgrades, accepts, then holds.
pub fn build_install_plan(
    managed: &[Managed<'_>],
    carried: Vec<Diagnostic>,
) -> Result<Plan, PortError> {
    let mut installs = Vec::new();
    let mut upgrades = Vec::new();
    let mut downgrades = Vec::new();
    let mut accepts = Vec::new();
    let mut claim_actions = Vec::new();
    let mut holds = Vec::new();
    let mut diagnostics = carried;

    for m in managed {
        let scope = m.installer.scope();
        let pkg = Ref::new(m.purl.clone());

        // Decide the changing action, if any. `accept` and the unsatisfied
        // warning are terminal (no version moves, so no follow-up hold).
        let change: Option<(ActionKind, Option<Version>)> = match &m.current {
            Current::Absent => Some((ActionKind::Install, None)),
            Current::Present {
                version,
                origin: Origin::Installed,
                claimed_by,
            } => {
                let change = match version {
                    Some(v) => match v.cmp(&m.resolved) {
                        std::cmp::Ordering::Equal => None, // already at the resolved version
                        std::cmp::Ordering::Less => Some((ActionKind::Upgrade, Some(v.clone()))),
                        std::cmp::Ordering::Greater => {
                            Some((ActionKind::Downgrade, Some(v.clone())))
                        }
                    },
                    None => None, // present, version unknown: trust state, no churn
                };
                // Nothing to install, but the claims behind it may be new (another
                // project installed it) or changed (this one's spec moved): record
                // them, or the package goes with the last claim state knows of.
                if change.is_none() && !same_claims(claimed_by, &m.claims) {
                    claim_actions.push(Action {
                        kind: ActionKind::Claim,
                        name: m.name.clone(),
                        scope,
                        backend: m.backend.clone(),
                        purl: m.purl.clone(),
                        from: version.clone(),
                        to: None,
                        claims: m.claims.clone(),
                        steps: Vec::new(),
                    });
                }
                change
            }
            Current::Present {
                version,
                origin: Origin::Accepted,
                ..
            } => {
                let satisfied = version
                    .as_ref()
                    .is_none_or(|v| satisfies_all(v, &m.claims, &m.vers_type));
                if satisfied {
                    accepts.push(Action {
                        kind: ActionKind::Accept,
                        name: m.name.clone(),
                        scope,
                        backend: m.backend.clone(),
                        purl: m.purl.clone(),
                        from: version.clone(),
                        to: None,
                        claims: m.claims.clone(),
                        steps: Vec::new(),
                    });
                } else {
                    diagnostics.push(Diagnostic::AcceptedUnsatisfied {
                        name: m.name.clone(),
                        installed: version
                            .as_ref()
                            .map(Version::as_str)
                            .unwrap_or("?")
                            .to_string(),
                        spec: claims_summary(&m.claims),
                    });
                }
                None
            }
        };

        let Some((kind, from)) = change else { continue };
        let caps = m.installer.capabilities();

        // A needed downgrade the backend can't do is an error for this package,
        // not a whole-plan failure (§5.2): skip it and record the diagnostic.
        if kind == ActionKind::Downgrade && !caps.supports_downgrade {
            diagnostics.push(Diagnostic::UnsupportedDowngrade {
                name: m.name.clone(),
                backend: m.backend.clone(),
            });
            continue;
        }

        let mut steps = if kind == ActionKind::Downgrade {
            m.installer.downgrade(&pkg, &m.resolved)?
        } else {
            m.installer.install_exact(&pkg, &m.resolved)?
        };
        // The lock's checksum is what this version was when it was locked, so a
        // replaced asset fails to verify (trust on first use).
        if let Some(locked) = &m.checksum {
            for step in &mut steps {
                if let Step::Fetch { sha256, .. } = step {
                    *sha256 = Some(locked.0.clone());
                }
            }
        }
        let action = Action {
            kind,
            name: m.name.clone(),
            scope,
            backend: m.backend.clone(),
            purl: m.purl.clone(),
            from,
            to: Some(m.resolved.clone()),
            claims: m.claims.clone(),
            steps,
        };
        match kind {
            ActionKind::Install => installs.push(action),
            ActionKind::Upgrade => upgrades.push(action),
            ActionKind::Downgrade => downgrades.push(action),
            _ => unreachable!("change is only install/upgrade/downgrade"),
        }
        // Pin the version we chose (§5.2). A backend that cannot hold but
        // would benefit from one degrades to a warning; one that does not need
        // a hold (it owns the binary or re-asserts each run) stays silent.
        match caps.hold {
            HoldSupport::Supported => holds.push(Action {
                kind: ActionKind::Hold,
                name: m.name.clone(),
                scope,
                backend: m.backend.clone(),
                purl: m.purl.clone(),
                from: None,
                to: Some(m.resolved.clone()),
                claims: m.claims.clone(),
                steps: m.installer.hold(&pkg, &m.resolved)?,
            }),
            HoldSupport::Unsupported => diagnostics.push(Diagnostic::UnsupportedHold {
                name: m.name.clone(),
                backend: m.backend.clone(),
            }),
            HoldSupport::NotNeeded => {}
        }
    }

    let mut actions = installs;
    actions.append(&mut upgrades);
    actions.append(&mut downgrades);
    actions.append(&mut accepts);
    actions.append(&mut claim_actions);
    actions.append(&mut holds);
    Ok(Plan {
        actions,
        diagnostics,
    })
}

// --- the uninstall and prune plans (§9) ------------------------------------

/// Render claims as `manifest=spec, …` for a diagnostic.
fn claims_summary(claims: &[Claim]) -> String {
    claims
        .iter()
        .map(|c| format!("{}={}", c.manifest.display(), c.spec))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Build the action for a package losing `dropped` claims: a `Remove` when that
/// leaves it unclaimed and bosun installed it, else a `DropClaim` (another
/// manifest still wants it, or it was only accepted). §9.
fn removal_action(
    name: &str,
    leaf: &bosun_core::record::StateLeaf,
    dropped: Vec<Claim>,
    remaining_is_empty: bool,
    installers: &BTreeMap<BackendId, &dyn Installer>,
) -> Result<Action, PortError> {
    let backend = &leaf.placement.backend;
    if remaining_is_empty && leaf.origin == Origin::Installed {
        let installer = installers.get(backend).ok_or_else(|| PortError::Backend {
            detail: format!("no installer registered for {}", backend.as_str()),
        })?;
        let pkg = Ref::new(leaf.placement.purl.clone());
        Ok(Action {
            kind: ActionKind::Remove,
            name: name.to_string(),
            scope: installer.scope(),
            backend: backend.clone(),
            purl: leaf.placement.purl.clone(),
            from: Some(leaf.installed.clone()),
            to: None,
            claims: dropped,
            steps: installer.uninstall(&pkg)?,
        })
    } else {
        Ok(Action {
            kind: ActionKind::DropClaim,
            name: name.to_string(),
            scope: installers.get(backend).map_or(Scope::User, |i| i.scope()),
            backend: backend.clone(),
            purl: leaf.placement.purl.clone(),
            from: None,
            to: None,
            claims: dropped,
            steps: Vec::new(),
        })
    }
}

/// Build the uninstall plan for one manifest (§9): drop every claim recorded
/// under `manifest`, uninstalling a package only when that was its last claim
/// and bosun installed it. Works from state, so it holds after the manifest is
/// edited or deleted.
pub fn build_uninstall_plan(
    manifest: &Path,
    state: &StateDocument,
    installers: &BTreeMap<BackendId, &dyn Installer>,
) -> Result<Plan, PortError> {
    build_removal_plan(state, installers, |_, claim| claim.manifest == manifest)
}

/// Build the plan for `bosun remove` (§4.2): drop `manifest`'s claim on the
/// one package `name`, and uninstall it only if bosun installed it and no
/// other manifest still claims it. Empty when the manifest never claimed it.
pub fn build_release_plan(
    manifest: &Path,
    name: &str,
    state: &StateDocument,
    installers: &BTreeMap<BackendId, &dyn Installer>,
) -> Result<Plan, PortError> {
    build_removal_plan(state, installers, |package, claim| {
        package == name && claim.manifest == manifest
    })
}

/// Build the prune plan (§9): drop claims whose manifest no longer exists, and
/// uninstall bosun-installed packages that are left with none. `manifest_present`
/// answers whether a claim's manifest file still exists (the caller does the
/// I/O, so this stays pure).
pub fn build_prune_plan(
    state: &StateDocument,
    installers: &BTreeMap<BackendId, &dyn Installer>,
    manifest_present: impl Fn(&Path) -> bool,
) -> Result<Plan, PortError> {
    build_removal_plan(state, installers, |_, claim| {
        !manifest_present(&claim.manifest)
    })
}

/// Shared walk for uninstall, release and prune: `drop` selects, by package
/// name and claim, the claims to remove from each package.
fn build_removal_plan(
    state: &StateDocument,
    installers: &BTreeMap<BackendId, &dyn Installer>,
    drop: impl Fn(&str, &Claim) -> bool,
) -> Result<Plan, PortError> {
    let mut removes = Vec::new();
    let mut drop_claims = Vec::new();

    for package in &state.packages {
        for leaf in package.targets.values() {
            let (dropped, remaining): (Vec<Claim>, Vec<Claim>) = leaf
                .claimed_by
                .iter()
                .cloned()
                .partition(|c| drop(&package.name, c));
            if dropped.is_empty() {
                continue;
            }
            let action = removal_action(
                &package.name,
                leaf,
                dropped,
                remaining.is_empty(),
                installers,
            )?;
            match action.kind {
                ActionKind::Remove => removes.push(action),
                _ => drop_claims.push(action),
            }
        }
    }

    // Removals first, then bare claim drops (mirrors the §4.8 prune layout).
    removes.append(&mut drop_claims);
    Ok(Plan {
        actions: removes,
        diagnostics: Vec::new(),
    })
}

// --- the privilege check (§5.5) --------------------------------------------

/// A package named in a privilege error.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PackageRef {
    /// bosun's logical name.
    pub name: String,
    /// The backend that would change it.
    pub backend: BackendId,
}

/// Why the plan cannot run under the current privilege (§5.5). Structured so
/// the CLI can render the remedy from the machine's `Elevation`; `Display` is
/// the label text (house rule: from [`errmsg`]). Implemented by hand because
/// the messages enumerate the packages.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PrivilegeError {
    /// The plan changes system scope but the process is not elevated.
    ElevationRequired {
        /// The system-scope packages that need elevation.
        packages: Vec<PackageRef>,
    },
    /// The plan changes user scope but the process is elevated.
    MustNotBeElevated {
        /// The user-scope packages that must not run elevated.
        packages: Vec<PackageRef>,
    },
    /// The plan changes both scopes and no `--scope` was given: refuse.
    MixedScope {
        /// The system-scope packages.
        system: Vec<PackageRef>,
        /// The user-scope packages.
        user: Vec<PackageRef>,
    },
}

/// Render a package list as `name (backend), …` for an error message.
fn refs(packages: &[PackageRef]) -> String {
    packages
        .iter()
        .map(|p| format!("{} ({})", p.name, p.backend.as_str()))
        .collect::<Vec<_>>()
        .join(", ")
}

impl std::fmt::Display for PrivilegeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PrivilegeError::ElevationRequired { packages } => {
                write!(f, "{}: {}", errmsg::ELEVATION_REQUIRED, refs(packages))
            }
            PrivilegeError::MustNotBeElevated { packages } => {
                write!(f, "{}: {}", errmsg::MUST_NOT_BE_ELEVATED, refs(packages))
            }
            PrivilegeError::MixedScope { system, user } => write!(
                f,
                "{}: system [{}], user [{}]",
                errmsg::MIXED_SCOPE,
                refs(system),
                refs(user)
            ),
        }
    }
}

impl std::error::Error for PrivilegeError {}

/// Check the plan against the machine's privilege (§5.5), considering only what
/// the plan *changes*. `requested` restricts the check to one scope (the
/// `--scope system|user` split); `None` means the whole plan, and a plan that
/// changes both scopes is then refused ([`PrivilegeError::MixedScope`]), never
/// half-applied. Nothing is changed by this call.
pub fn check_privilege(
    plan: &Plan,
    machine: &dyn Machine,
    requested: Option<Scope>,
) -> Result<(), PrivilegeError> {
    let in_scope = |scope: Scope| requested.is_none_or(|r| r == scope);
    let collect = |scope: Scope| -> Vec<PackageRef> {
        // One entry per package, not per action: a package with both an install
        // and a hold needs root once, and must be named once (§5.5).
        let mut refs: Vec<PackageRef> = Vec::new();
        for a in plan
            .actions
            .iter()
            .filter(|a| a.kind.changes_machine() && a.scope == scope && in_scope(scope))
        {
            let pkg = PackageRef {
                name: a.name.clone(),
                backend: a.backend.clone(),
            };
            if !refs.contains(&pkg) {
                refs.push(pkg);
            }
        }
        refs
    };
    let system = collect(Scope::System);
    let user = collect(Scope::User);

    // No --scope and both halves change: refuse rather than apply one (§5.5).
    if requested.is_none() && !system.is_empty() && !user.is_empty() {
        return Err(PrivilegeError::MixedScope { system, user });
    }
    let elevated = machine.privilege() == Privilege::Elevated;
    if !system.is_empty() && !elevated {
        return Err(PrivilegeError::ElevationRequired { packages: system });
    }
    if !user.is_empty() && elevated {
        return Err(PrivilegeError::MustNotBeElevated { packages: user });
    }
    Ok(())
}

#[cfg(test)]
mod tests;
