//! The M1 read/resolve commands (§4.2): `backends`, `status`, `lock`.
//! Each returns one typed value; the mutating commands are slice 10-2.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use bosun_core::platform::Selector;
use bosun_core::record::{LockDocument, LockLeaf, LockPackage, Package, StateDocument, StateLeaf};
use bosun_core::{BackendId, Distro, Libc, Manifest, Os, Scope, Spec, Target, Version};
use bosun_io::{check_options, load_manifest, lock, manifest_hash};
use bosun_ports::FileAccess;
use bosun_ports::Installer;
use bosun_resolve::{applicable, resolve, resolve_with_lock, NotApplicable, Outcome, Resolution};
use serde::Serialize;
use tracing::{debug, info, warn};

use crate::env::{Env, UserSide};
use crate::error::CliError;
use crate::output::Render;

/// The installer preference order (§5.3) for this machine: the command-line
/// flag, `BOSUN_INSTALLER`, the user config, then the built-in order. The
/// manifest says only which backends are acceptable, never in what order.
pub fn preference(env: &Env, flag: &[String]) -> Result<Vec<BackendId>, CliError> {
    let registered: Vec<(BackendId, Scope)> = env
        .resolvables()
        .iter()
        .filter_map(|b| env.installer_for(&b.id).map(|i| (b.id.clone(), i.scope())))
        .collect();
    configured_order(&env.user_side(), flag, &registered)
}

/// The installer preference order (§5.3) for a machine bosun isn't running on:
/// the flag, `BOSUN_INSTALLER` and the user config as here, then `ids` (the
/// backends with a remote index) in registration order, since a backend's
/// install scope is known only from a probe.
pub fn remote_preference(
    user: &UserSide,
    flag: &[String],
    ids: &[BackendId],
) -> Result<Vec<BackendId>, CliError> {
    let registered: Vec<(BackendId, Scope)> =
        ids.iter().map(|id| (id.clone(), Scope::User)).collect();
    configured_order(user, flag, &registered)
}

/// [`preference_order`] over the flag, `BOSUN_INSTALLER` and the user config,
/// read as its user (§4.5).
fn configured_order(
    user: &UserSide,
    flag: &[String],
    registered: &[(BackendId, Scope)],
) -> Result<Vec<BackendId>, CliError> {
    let config = match &user.config {
        Some(path) => config_installer(user.files.as_ref(), path)?,
        // No home directory: no user config to read.
        None => Vec::new(),
    };
    let from_env = std::env::var("BOSUN_INSTALLER").ok();
    Ok(preference_order(
        flag,
        from_env.as_deref(),
        &config,
        registered,
    ))
}

/// The preference order from each source, highest first, each name once. The
/// built-in order, last, is every registered backend with user-scope ones
/// before system-scope ones (so nothing asks for root or Administrator when a
/// user-scope backend can do it), each group in registration order.
pub(crate) fn preference_order(
    flag: &[String],
    from_env: Option<&str>,
    config: &[String],
    registered: &[(BackendId, Scope)],
) -> Vec<BackendId> {
    let built_in = [Scope::User, Scope::System].into_iter().flat_map(|scope| {
        registered
            .iter()
            .filter(move |(_, s)| *s == scope)
            .map(|(id, _)| id.as_str().to_string())
    });
    let env_names = from_env
        .into_iter()
        .flat_map(|v| v.split(','))
        .map(|s| s.trim().to_string());
    let mut order: Vec<String> = Vec::new();
    for name in flag
        .iter()
        .cloned()
        .chain(env_names)
        .chain(config.iter().cloned())
        .chain(built_in)
    {
        if !name.is_empty() && !order.contains(&name) {
            order.push(name);
        }
    }
    order.into_iter().map(BackendId::new).collect()
}

/// The `installer` order from the user config at `path`, read through
/// `files`; none when there is no config file. A config that doesn't load is
/// an error, never a silently dropped preference.
pub(crate) fn config_installer(
    files: &dyn FileAccess,
    path: &Path,
) -> Result<Vec<String>, CliError> {
    let io_err = |detail: String| CliError::Io {
        path: path.to_path_buf(),
        detail,
    };
    let text = match files.read(path) {
        Ok(bytes) => String::from_utf8(bytes).map_err(|e| io_err(e.to_string()))?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(io_err(e.to_string())),
    };
    bosun_io::config::from_yaml(&text, path)
        .map(|cfg| cfg.installer)
        .map_err(|e| CliError::Parse {
            what: "config".to_string(),
            detail: e.to_string(),
        })
}

/// The manifest `path` names: `bosun.yaml` in it when it is a directory
/// (default: the current one), else the file itself, which must exist. Asked
/// through `files`, as the manifest's user (§4.5).
pub(crate) fn manifest_file(
    files: &dyn FileAccess,
    path: Option<PathBuf>,
) -> Result<PathBuf, CliError> {
    let base = path.unwrap_or_else(|| PathBuf::from("."));
    let file = if files.is_dir(&base).unwrap_or(false) {
        base.join("bosun.yaml")
    } else {
        base
    };
    match files.exists(&file) {
        Ok(true) => Ok(file),
        Ok(false) => Err(CliError::ManifestNotFound(file)),
        Err(e) => Err(CliError::Io {
            path: file,
            detail: e.to_string(),
        }),
    }
}

/// Load the manifest at `file`, read as its user, and check its backend
/// options against the backends this build has (§5.4).
pub(crate) fn load(env: &Env, file: &Path) -> Result<Manifest, CliError> {
    let manifest = load_checked(env.option_keys(), env.user_files().as_ref(), file)?;
    warn_of_own_codenames(&manifest, env.machine().distro());
    Ok(manifest)
}

/// Warn, once per selector, of each release selector that names `distro`'s
/// own codename where its packages spell the release by the base distro's
/// (`linux:linuxmint@wilma` on Linux Mint 22, whose release is `noble`): it
/// matches nothing, and the warning names the selector to write (§5.4).
pub(crate) fn warn_of_own_codenames(manifest: &Manifest, distro: Option<&Distro>) {
    let Some(distro) = distro else { return };
    let selectors: BTreeSet<&Selector> = manifest
        .defaults
        .keys()
        .chain(manifest.packages.iter().flat_map(|e| e.install.keys()))
        .collect();
    for sel in selectors {
        let (Some(os), Some(family), Some(release)) = (sel.os(), sel.distro(), sel.release())
        else {
            continue;
        };
        if family != distro.id {
            continue;
        }
        if let Some(deb) = distro.deb_release_for_own_codename(release) {
            let instead = Selector::OsDistroRelease(os, family.to_string(), deb.clone());
            warn!(
                message = crate::logmsg::OWN_CODENAME_SELECTOR,
                selector = %sel,
                release = %deb,
                instead = %instead,
            );
        }
    }
}

/// Load the manifest at `file`, read through `files`, checking its backend
/// options against `keys`.
pub(crate) fn load_checked(
    keys: &bosun_io::BackendOptionKeys,
    files: &dyn FileAccess,
    file: &Path,
) -> Result<Manifest, CliError> {
    let text = crate::edit::read(files, file)?;
    let parse_err = |detail: String| CliError::Parse {
        what: "manifest".to_string(),
        detail,
    };
    let manifest = load_manifest(&text, file).map_err(|e| parse_err(e.to_string()))?;
    check_options(&manifest, keys).map_err(|e| parse_err(format!("{}: {e}", file.display())))?;
    Ok(manifest)
}

// --- the machine's C library ---------------------------------------------------

/// The machine's C library (§5.4), as `backends` and `status` show it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
struct LibcInfo {
    family: String,
    version: Option<String>,
}

/// The C library facts for output: `None` when unknown.
fn libc_info(libc: Option<&Libc>) -> Option<LibcInfo> {
    libc.map(|l| LibcInfo {
        family: l.family.to_string(),
        version: l.version.clone(),
    })
}

/// The text line naming the C library, on Linux (`libc: gnu 2.36`, or
/// `libc: unknown`); elsewhere there is none.
fn libc_line(os: Os, libc: Option<&LibcInfo>) -> Option<String> {
    (os == Os::Linux).then(|| match libc {
        Some(LibcInfo {
            family,
            version: Some(v),
        }) => format!("libc: {family} {v}"),
        Some(LibcInfo {
            family,
            version: None,
        }) => format!("libc: {family}"),
        None => "libc: unknown".to_string(),
    })
}

// --- backends --------------------------------------------------------------

/// The `backends` output (§5.3).
#[derive(Serialize)]
pub struct BackendsOutput {
    registered: Vec<BackendInfo>,
    skipped: Vec<SkippedInfo>,
    /// The machine's C library, which libc selectors match (§5.4).
    libc: Option<LibcInfo>,
    #[serde(skip)]
    os: Os,
}

#[derive(Serialize)]
struct BackendInfo {
    id: String,
    version: String,
}

#[derive(Serialize)]
struct SkippedInfo {
    id: String,
    reason: String,
}

/// List the registered backends and why the others were skipped.
pub fn backends(env: &Env) -> BackendsOutput {
    BackendsOutput {
        registered: env
            .backends
            .iter()
            .map(|b| BackendInfo {
                id: b.id.as_str().to_string(),
                version: b.version.as_str().to_string(),
            })
            .collect(),
        skipped: env
            .skipped
            .iter()
            .map(|s| SkippedInfo {
                id: s.id.clone(),
                reason: s.reason.clone(),
            })
            .collect(),
        libc: libc_info(env.machine().libc()),
        os: env.machine().os(),
    }
}

impl Render for BackendsOutput {
    fn text(&self) -> String {
        let mut lines = Vec::new();
        for b in &self.registered {
            lines.push(format!("{:<16} {}", b.id, b.version));
        }
        for s in &self.skipped {
            lines.push(format!("{:<16} skipped: {}", s.id, s.reason));
        }
        if lines.is_empty() {
            lines.push("no backends registered".to_string());
        }
        lines.extend(libc_line(self.os, self.libc.as_ref()));
        lines.join("\n")
    }
}

// --- status ----------------------------------------------------------------

/// The `status` output (§4.6).
#[derive(Serialize)]
pub struct StatusOutput {
    packages: Vec<PackageStatus>,
    diagnostics: Vec<String>,
    /// The C library the manifest's libc selectors were matched against.
    libc: Option<LibcInfo>,
    #[serde(skip)]
    os: Os,
}

#[derive(Serialize)]
struct PackageStatus {
    name: String,
    backend: String,
    /// The installed version, if known: from bosun state when managed, else the
    /// live version the backend reports (which may be unknown, e.g. github).
    installed: Option<String>,
    /// Recorded in bosun state (bosun installed or accepted it).
    managed: bool,
    /// Installed on the machine at all (state or a live `verify_installed`).
    present: bool,
    wanted: String,
    up_to_date: bool,
    /// Every manifest that claims it, from bosun state; empty when unmanaged.
    claimed_by: Vec<ClaimStatus>,
}

/// One claim on a managed package (§4.6), as `status` shows it.
#[derive(Serialize)]
struct ClaimStatus {
    /// The claiming manifest's absolute path.
    manifest: String,
    /// The spec it asked for, as recorded (`vers:<type>/<constraints>`).
    spec: String,
    /// The OS user who claimed it (system scope only).
    #[serde(skip_serializing_if = "Option::is_none")]
    user: Option<String>,
    /// False once the manifest is gone: `prune` drops such a claim (§9).
    manifest_exists: bool,
}

fn claim_statuses(claims: &[Claim]) -> Vec<ClaimStatus> {
    claims
        .iter()
        .map(|c| ClaimStatus {
            manifest: c.manifest.display().to_string(),
            spec: c.spec.clone(),
            user: c.user.clone(),
            manifest_exists: manifest_present(&c.manifest),
        })
        .collect()
}

/// One line per claim, under its package's line.
fn claim_lines(claims: &[ClaimStatus], lines: &mut Vec<String>) {
    for c in claims {
        let mut line = format!("  claimed by {} ({})", c.manifest, c.spec);
        if let Some(user) = &c.user {
            line.push_str(&format!(" as {user}"));
        }
        if !c.manifest_exists {
            line.push_str(", manifest gone");
        }
        lines.push(line);
    }
}

/// Report installed-vs-wanted for each applicable package of the manifest at
/// `path`, with every manifest that claims it. A package present on the machine
/// but not in bosun state is shown as present-but-unmanaged (it would be
/// accepted, §4.6), not "not installed".
pub fn status(env: &Env, path: PathBuf, pref: &[BackendId]) -> Result<StatusOutput, CliError> {
    let file = manifest_file(env.user_files().as_ref(), Some(path))?;
    let manifest = load(env, &file)?;
    let hash = manifest_hash(&manifest);
    let resolution = resolve(&manifest, &hash, &env.host(), &env.resolvables(), pref)
        .map_err(|e| CliError::Resolve(e.to_string()))?;

    let system = read_state(env, Scope::System)?;
    let user = read_state(env, Scope::User)?;

    let mut packages = Vec::new();
    let mut diagnostics = Vec::new();
    for entry in &resolution.outcomes {
        match &entry.outcome {
            Outcome::Resolved {
                target,
                backend,
                version,
            } => {
                if in_hidden_scope(env, backend) {
                    continue;
                }
                // bosun state is authoritative for what it manages; a package it
                // does not record is checked live on the machine (§4.6 A).
                let leaf = managed_leaf(&[&system, &user], &entry.name);
                let claimed_by = leaf.map_or_else(Vec::new, |l| claim_statuses(&l.claimed_by));
                let (installed, managed, present) = match leaf {
                    Some(l) => (Some(installed_now(env, l).as_str().to_string()), true, true),
                    None => match live_presence(env, &resolution, &entry.name, target, backend) {
                        Some(v) => (v, false, true),
                        None => (None, false, false),
                    },
                };
                let up_to_date = managed && installed.as_deref() == Some(version.as_str());
                packages.push(PackageStatus {
                    name: entry.name.clone(),
                    backend: backend.as_str().to_string(),
                    installed,
                    managed,
                    present,
                    wanted: version.as_str().to_string(),
                    up_to_date,
                    claimed_by,
                });
            }
            Outcome::NotApplicable(na) => {
                diagnostics.push(format!("{}: not applicable ({})", entry.name, na_text(na)))
            }
            Outcome::Unsatisfiable { spec, available } => diagnostics.push(format!(
                "{}: unsatisfiable {spec} (available: {available})",
                entry.name
            )),
        }
    }
    Ok(StatusOutput {
        packages,
        diagnostics,
        libc: libc_info(env.machine().libc()),
        os: env.machine().os(),
    })
}

/// The version of a package bosun state records that its package manager
/// reports installed now (§4.6), or the recorded one where it reports none or
/// cannot be asked.
fn installed_now(env: &Env, leaf: &StateLeaf) -> Version {
    env.installer_for(&leaf.placement.backend)
        .and_then(|i| bosun_apply::reported_installed(i, &leaf.placement.purl).ok())
        .flatten()
        .unwrap_or_else(|| leaf.installed.clone())
}

/// Whether `backend` installs into a scope this run leaves out
/// ([`Env::visible_state`]).
fn in_hidden_scope(env: &Env, backend: &BackendId) -> bool {
    env.system_hidden()
        && env
            .installer_for(backend)
            .is_some_and(|i| i.scope() == Scope::System)
}

/// Whether a package absent from bosun state is nonetheless present on the
/// machine, by asking the backend directly (§4.6 A). `Some(version)` is present
/// (version `None` when the backend cannot read it, e.g. github); `None` is
/// absent. A backend or lock entry we cannot find is treated as absent.
fn live_presence(
    env: &Env,
    resolution: &Resolution,
    name: &str,
    target: &Target,
    backend: &BackendId,
) -> Option<Option<String>> {
    let installer = env.installer_for(backend)?;
    let purl = resolution
        .lock
        .packages
        .iter()
        .find(|p| p.name == name)
        .and_then(|p| p.targets.get(target))
        .map(|leaf| leaf.placement.purl.clone())?;
    match installer.verify_installed(&Ref::new(purl)).ok()? {
        Installed::Present { version } => Some(version.map(|v| v.as_str().to_string())),
        Installed::Absent => None,
    }
}

impl Render for StatusOutput {
    fn text(&self) -> String {
        let mut lines = Vec::new();
        for p in &self.packages {
            let state = if !p.present {
                "not installed".to_string()
            } else if !p.managed {
                // Present outside bosun: would be accepted, never reinstalled.
                match &p.installed {
                    Some(v) => format!("{v} present, not managed (would be accepted)"),
                    None => "present, not managed (would be accepted)".to_string(),
                }
            } else {
                match &p.installed {
                    Some(v) if p.up_to_date => format!("{v} (up to date)"),
                    Some(v) => format!("{v} → {} (outdated)", p.wanted),
                    None => "present, version unknown".to_string(),
                }
            };
            lines.push(format!("{:<16} {:<8} {}", p.name, p.backend, state));
            claim_lines(&p.claimed_by, &mut lines);
        }
        for d in &self.diagnostics {
            lines.push(d.clone());
        }
        if lines.is_empty() {
            lines.push("no packages".to_string());
        }
        lines.extend(libc_line(self.os, self.libc.as_ref()));
        lines.join("\n")
    }
}

/// `status` with no manifest: everything bosun manages on this machine (§4.2),
/// both scopes, each with the manifests that claim it.
#[derive(Serialize)]
pub struct MachineStatus {
    packages: Vec<ManagedStatus>,
}

#[derive(Serialize)]
struct ManagedStatus {
    name: String,
    scope: &'static str,
    target: String,
    backend: String,
    installed: String,
    /// `installed` (bosun put it there) or `accepted` (found present, §4.6).
    origin: &'static str,
    claimed_by: Vec<ClaimStatus>,
}

/// Read both scopes' machine state; no manifest is needed.
pub fn machine_status(env: &Env) -> Result<MachineStatus, CliError> {
    let mut packages = Vec::new();
    for scope in [Scope::System, Scope::User] {
        let doc = read_state(env, scope)?;
        for pkg in &doc.packages {
            for (target, leaf) in &pkg.targets {
                packages.push(ManagedStatus {
                    name: pkg.name.clone(),
                    scope: scope_label(scope),
                    target: target.to_string(),
                    backend: leaf.placement.backend.as_str().to_string(),
                    installed: installed_now(env, leaf).as_str().to_string(),
                    origin: origin_label(leaf.origin),
                    claimed_by: claim_statuses(&leaf.claimed_by),
                });
            }
        }
    }
    Ok(MachineStatus { packages })
}

impl Render for MachineStatus {
    fn text(&self) -> String {
        if self.packages.is_empty() {
            return "bosun manages nothing on this machine".to_string();
        }
        let mut lines = Vec::new();
        for p in &self.packages {
            lines.push(format!(
                "{:<16} {:<8} {} ({}, {})",
                p.name, p.backend, p.installed, p.origin, p.scope
            ));
            claim_lines(&p.claimed_by, &mut lines);
        }
        lines.join("\n")
    }
}

fn scope_label(scope: Scope) -> &'static str {
    match scope {
        Scope::System => "system",
        Scope::User => "user",
    }
}

fn origin_label(origin: Origin) -> &'static str {
    match origin {
        Origin::Installed => "installed",
        Origin::Accepted => "accepted",
    }
}

// --- lock ------------------------------------------------------------------

/// The `lock` output: the resolution (when writing) or the freshness report
/// (when `--check`).
#[derive(Serialize)]
pub struct LockOutput {
    /// An entry was unsatisfiable: nothing was written, exit 1 (§4.7).
    error: bool,
    resolved: Vec<ResolvedInfo>,
    diagnostics: Vec<String>,
    /// The lock written; `None` when nothing was.
    lock: Option<String>,
}

#[derive(Serialize)]
struct ResolvedInfo {
    name: String,
    backend: String,
    target: String,
    version: String,
}

/// Resolve the manifest for this machine (its platform and `any`) and write
/// this machine's lock for it (§4.5). A version the lock already pins is kept
/// while it still fits (§7); `upgrade` moves pins forward.
pub fn lock(env: &Env, path: Option<PathBuf>, pref: &[BackendId]) -> Result<LockOutput, CliError> {
    let file = manifest_file(env.user_files().as_ref(), path)?;
    let manifest = load(env, &file)?;
    let relocked = relock(env, &file, &manifest, pref, &Release::None)?;
    let written = match &relocked.pending {
        Some(pending) => Some(pending.write()?),
        None => None,
    };
    Ok(lock_output(relocked.resolution, written.as_deref()))
}

/// What `bosun upgrade` moved: each package whose locked version changed, and
/// the lock that followed.
#[derive(Serialize)]
pub struct UpgradeOutput {
    changes: Vec<VersionChange>,
    lock: LockOutput,
}

/// One package's locked version before and after (`from` is `None` for a
/// package the lock didn't have).
#[derive(Serialize)]
struct VersionChange {
    name: String,
    from: Option<String>,
    to: String,
}

impl Render for UpgradeOutput {
    fn text(&self) -> String {
        let mut lines: Vec<String> = self
            .changes
            .iter()
            .map(|c| match &c.from {
                Some(from) => format!("{:<16} {from} -> {}", c.name, c.to),
                None => format!("{:<16} {} (new)", c.name, c.to),
            })
            .collect();
        if self.changes.is_empty() && !self.lock.error {
            lines.push("Everything is at the newest version its spec allows.".to_string());
        }
        lines.push(self.lock.text());
        if !self.changes.is_empty() && !self.lock.error {
            lines.push("Run `bosun install` to install them.".to_string());
        }
        lines.join("\n")
    }

    fn exit_code(&self) -> i32 {
        self.lock.exit_code()
    }
}

/// `bosun upgrade [name…]`: re-resolve the named packages (all, when none are
/// named) to the newest version their spec admits, keep every other pin, and
/// write the lock. Installs nothing.
pub fn upgrade(
    env: &Env,
    names: &[String],
    path: Option<PathBuf>,
    pref: &[BackendId],
) -> Result<UpgradeOutput, CliError> {
    let file = manifest_file(env.user_files().as_ref(), path)?;
    let manifest = load(env, &file)?;
    if let Some(missing) = names
        .iter()
        .find(|n| !manifest.packages.iter().any(|e| &e.name == *n))
    {
        return Err(CliError::Usage(format!(
            "upgrade: {} has no package named {missing:?}",
            file.display()
        )));
    }
    let release = if names.is_empty() {
        Release::All
    } else {
        Release::Named(names.to_vec())
    };
    let relocked = relock(env, &file, &manifest, pref, &release)?;
    let locked_version = |doc: &LockDocument, name: &str| {
        doc.packages
            .iter()
            .find(|p| p.name == name)
            .and_then(|p| p.targets.values().next())
            .map(|leaf| leaf.resolved.as_str().to_string())
    };
    let changes = relocked
        .resolution
        .lock
        .packages
        .iter()
        .filter_map(|p| {
            let to = locked_version(&relocked.resolution.lock, &p.name)?;
            let from = relocked
                .previous
                .as_ref()
                .and_then(|prev| locked_version(prev, &p.name));
            (from.as_deref() != Some(to.as_str())).then(|| VersionChange {
                name: p.name.clone(),
                from,
                to,
            })
        })
        .collect();
    let written = match &relocked.pending {
        Some(pending) => Some(pending.write()?),
        None => None,
    };
    Ok(UpgradeOutput {
        changes,
        lock: lock_output(relocked.resolution, written.as_deref()),
    })
}

/// Which pins a re-lock lets go of, so their packages resolve to the newest
/// version their spec admits.
pub(crate) enum Release {
    /// Keep every pin that still fits (`lock`, `install`).
    None,
    /// Let every pin go (`upgrade`).
    All,
    /// Let these packages' pins go (`upgrade <name>…`).
    Named(Vec<String>),
}

/// A lock worked out but not yet written: `install` writes it only once the
/// install succeeds, so a declined or failed install leaves the old lock.
pub(crate) struct PendingLock {
    path: PathBuf,
    doc: LockDocument,
    /// The lock this one replaces, if any.
    previous: Option<LockDocument>,
    /// The install scope of each package `install` resolved.
    scopes: BTreeMap<String, Scope>,
    /// How the lock is written: as the user who elevated the run (§4.5).
    files: std::rc::Rc<dyn FileAccess>,
}

impl PendingLock {
    /// Keep the previous lock's entry (or no entry) for each package in a
    /// scope `requested` leaves out (`--scope`, §5.5). Every other package
    /// takes its resolved entry, a deferred record's and one whose state the
    /// run left out too, so the next elevated `install --frozen` applies them
    /// (§4.6).
    pub(crate) fn only_requested_scope(&mut self, requested: Option<Scope>) {
        let Some(requested) = requested else { return };
        let mut packages = Vec::new();
        for pkg in std::mem::take(&mut self.doc.packages) {
            if self.scopes.get(&pkg.name).is_none_or(|s| *s == requested) {
                packages.push(pkg);
                continue;
            }
            debug!(message = crate::logmsg::LOCK_KEPT, package = %pkg.name);
            let before = self
                .previous
                .iter()
                .flat_map(|l| &l.packages)
                .find(|p| p.name == pkg.name);
            packages.extend(before.cloned());
        }
        self.doc.packages = packages;
    }

    /// Pin each `(name, target, version)` the install ends the package at
    /// where that is not the lock's pin (the package manager moved it, or
    /// every claim's intersection chose another), so the lock follows what is
    /// installed (§4.6).
    fn follow(&mut self, followed: &[(String, Target, Version)]) {
        for (name, target, version) in followed {
            let leaf = self
                .doc
                .packages
                .iter_mut()
                .find(|p| &p.name == name)
                .and_then(|p| p.targets.get_mut(target));
            if let Some(leaf) = leaf {
                leaf.resolved = version.clone();
                leaf.checksum = None;
            }
        }
    }

    /// Write the lock, making its directory; gives where it went.
    pub(crate) fn write(&self) -> Result<PathBuf, CliError> {
        let io_err = |e: std::io::Error| CliError::Io {
            path: self.path.clone(),
            detail: e.to_string(),
        };
        self.files
            .write(&self.path, lock::to_json(&self.doc).as_bytes())
            .map_err(io_err)?;
        Ok(self.path.clone())
    }
}

/// A re-lock's result: the resolution, the lock it read (if any), and the lock
/// to write, `None` when an entry was unsatisfiable (a lock never records an
/// incomplete resolution as current, §4.7).
pub(crate) struct Relocked {
    pub(crate) resolution: Resolution,
    pub(crate) previous: Option<LockDocument>,
    pub(crate) pending: Option<PendingLock>,
}

/// Resolve `manifest` for this machine against its current lock, keeping the
/// pins `release` doesn't let go of. Writes nothing.
pub(crate) fn relock(
    env: &Env,
    file: &Path,
    manifest: &Manifest,
    pref: &[BackendId],
    release: &Release,
) -> Result<Relocked, CliError> {
    let hash = manifest_hash(manifest);
    let files = env.user_files();
    let claim_path = canonical_manifest(files.as_ref(), file);
    let lock_path = env.lock_path(&claim_path);
    warn_of_a_project_lock(files.as_ref(), file, &lock_path);
    let previous = this_lock(env, &claim_path)?;
    let released = |name: &str| match release {
        Release::None => false,
        Release::All => true,
        Release::Named(names) => names.iter().any(|n| n == name),
    };
    let mut kept = previous.clone().unwrap_or_else(|| LockDocument {
        lockfile_version: bosun_resolve::LOCKFILE_VERSION,
        manifest: claim_path.display().to_string(),
        manifest_hash: hash.clone(),
        packages: Vec::new(),
    });
    kept.packages.retain(|p| !released(&p.name));
    // §4.6: an entry this lock doesn't pin yet takes what bosun already
    // installed (for any project) when it fits, so a shared tool isn't bumped
    // just because another project resolves it later.
    let state = state_of_named_scopes(env, manifest)?;
    for entry in &manifest.packages {
        if released(&entry.name) || kept.packages.iter().any(|p| p.name == entry.name) {
            continue;
        }
        if let Some(pin) = installed_pin(&state, &entry.name) {
            kept.packages.push(pin);
        }
    }
    let resolution = resolve_with_lock(
        manifest,
        &hash,
        &env.host(),
        &env.resolvables(),
        pref,
        Some(&kept),
    )
    .map_err(|e| CliError::Resolve(e.to_string()))?;
    let complete = !resolution
        .outcomes
        .iter()
        .any(|o| matches!(o.outcome, Outcome::Unsatisfiable { .. }));
    let pending = complete.then(|| {
        let mut doc = resolution.lock.clone();
        doc.manifest = claim_path.display().to_string();
        PendingLock {
            path: lock_path,
            doc,
            previous: previous.clone(),
            scopes: BTreeMap::new(),
            files: env.user_files(),
        }
    });
    Ok(Relocked {
        resolution,
        previous,
        pending,
    })
}

/// The machine state of every scope a backend the manifest names installs
/// into, and no other: a manifest of user-scope tools never opens the
/// root-owned system state (§4.6).
fn state_of_named_scopes(env: &Env, manifest: &Manifest) -> Result<StateDocument, CliError> {
    let blocks = manifest
        .defaults
        .values()
        .chain(manifest.packages.iter().flat_map(|e| e.install.values()));
    let mut scopes = Vec::new();
    for block in blocks {
        for backend in block.backends.keys() {
            if let Some(installer) = env.installer_for(&BackendId::new(backend)) {
                if !scopes.contains(&installer.scope()) {
                    scopes.push(installer.scope());
                }
            }
        }
    }
    let mut packages = Vec::new();
    for scope in scopes {
        packages.extend(read_state(env, scope)?.packages);
    }
    Ok(StateDocument {
        state_version: 1,
        packages,
    })
}

/// A candidate pin from what bosun recorded installing `name` (any scope,
/// any project): the resolver keeps it only if it is the same backend and
/// package, still published, and in this project's spec. State records the
/// machine's platform; the pin is offered under `any` too, for
/// platform-independent backends.
fn installed_pin(state: &StateDocument, name: &str) -> Option<LockPackage> {
    let pkg = state.packages.iter().find(|p| p.name == name)?;
    let (target, leaf) = pkg.targets.iter().next()?;
    let lock_leaf = LockLeaf {
        placement: leaf.placement.clone(),
        spec: None,
        resolved: leaf.installed.clone(),
        index_snapshot: None,
        checksum: None,
    };
    Some(Package {
        name: name.to_string(),
        meta: Spec::new("*", "generic").expect("`*` is a spec"),
        targets: BTreeMap::from([(*target, lock_leaf.clone()), (Target::Any, lock_leaf)]),
    })
}

/// This machine's lock for the manifest at `claim_path`, if it has one: a lock
/// file recording another manifest (a path-hash clash) is not this one's.
pub(crate) fn this_lock(env: &Env, claim_path: &Path) -> Result<Option<LockDocument>, CliError> {
    let files = env.user_files();
    Ok(
        read_lock(files.as_ref(), &env.lock_path(claim_path), &env.schemes())?
            .filter(|l| Path::new(&l.manifest) == claim_path),
    )
}

/// A `bosun.lock` beside the manifest is the shared lock of older bosun
/// builds, which this one no longer reads; say so, and where the lock is now.
/// Looked for through `files`, as the manifest's user.
fn warn_of_a_project_lock(files: &dyn FileAccess, manifest: &Path, lock_path: &Path) {
    let old = manifest
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("bosun.lock");
    if files.exists(&old).unwrap_or(false) {
        tracing::warn!(
            message = crate::logmsg::PROJECT_LOCK_IGNORED,
            path = %old.display(),
            lock = %lock_path.display()
        );
    }
}

fn lock_output(resolution: Resolution, written: Option<&Path>) -> LockOutput {
    let mut resolved = Vec::new();
    let mut diagnostics = Vec::new();
    let mut error = false;
    for entry in resolution.outcomes {
        match entry.outcome {
            Outcome::Resolved {
                target,
                backend,
                version,
            } => resolved.push(ResolvedInfo {
                name: entry.name,
                backend: backend.as_str().to_string(),
                target: target.to_string(),
                version: version.as_str().to_string(),
            }),
            Outcome::NotApplicable(na) => {
                diagnostics.push(format!("{}: not applicable ({})", entry.name, na_text(&na)))
            }
            Outcome::Unsatisfiable { spec, available } => {
                error = true;
                diagnostics.push(format!(
                    "{}: unsatisfiable {spec} (available: {available})",
                    entry.name
                ));
            }
        }
    }
    LockOutput {
        error,
        resolved,
        diagnostics,
        lock: written.map(|p| p.display().to_string()),
    }
}

impl Render for LockOutput {
    fn text(&self) -> String {
        let mut lines = Vec::new();
        for r in &self.resolved {
            lines.push(format!(
                "{:<16} {:<8} {} ({})",
                r.name, r.backend, r.version, r.target
            ));
        }
        for d in &self.diagnostics {
            lines.push(d.clone());
        }
        match &self.lock {
            Some(path) => lines.push(format!(
                "Locked: {} resolved, in {path}",
                self.resolved.len()
            )),
            None => lines.push(format!("Locked: {} resolved", self.resolved.len())),
        }
        lines.join("\n")
    }

    fn exit_code(&self) -> i32 {
        if self.error {
            // An unsatisfiable entry: nothing written, this is a failure (§4.7).
            1
        } else {
            0
        }
    }
}

// --- helpers ---------------------------------------------------------------

/// The state leaf bosun records for `name`, from the first scope that has it.
fn managed_leaf<'a>(docs: &[&'a StateDocument], name: &str) -> Option<&'a StateLeaf> {
    docs.iter()
        .find_map(|doc| doc.packages.iter().find(|p| p.name == name))
        .and_then(|pkg| pkg.targets.values().next())
}

fn na_text(na: &NotApplicable) -> String {
    match na {
        NotApplicable::NoSelector => "no matching selector".to_string(),
        NotApplicable::NoBackend => "no backend named".to_string(),
        NotApplicable::NoRegisteredBackend { candidates } => format!(
            "no registered backend among {}",
            candidates
                .iter()
                .map(|b| b.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

/// The lock at `path`, read through `files` (as its user, §4.5); `None` when
/// there is none.
fn read_lock(
    files: &dyn FileAccess,
    path: &Path,
    schemes: &bosun_io::BackendSchemes,
) -> Result<Option<LockDocument>, CliError> {
    let bytes = match files.read(path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => {
            return Err(CliError::Io {
                path: path.to_path_buf(),
                detail: e.to_string(),
            })
        }
    };
    let parse_err = |detail: String| CliError::Parse {
        what: "lockfile".to_string(),
        detail,
    };
    let text = String::from_utf8(bytes).map_err(|e| parse_err(e.to_string()))?;
    let doc = lock::from_json(&text, path, schemes).map_err(|e| parse_err(e.to_string()))?;
    Ok(Some(doc))
}

// --- install / uninstall / prune (§4.2, §4.8) ------------------------------

use bosun_apply::{
    build_install_plan, build_prune_plan, build_uninstall_plan, ApplyReport, ApplyResult, Current,
    Diagnostic, Managed, Plan,
};
use bosun_core::{Claim, Installed, Origin, Ref};
use bosun_resolve::{frozen_select, select_across_claims};

/// Options for `install`.
pub struct InstallOpts {
    /// The manifest directory or file.
    pub path: Option<PathBuf>,
    /// Apply the lockfile verbatim, no re-resolve (§7).
    pub frozen: bool,
}

/// Build the `install` plan (§4.8): re-lock the manifest, keeping the lock's
/// pins that still fit (or, with `--frozen`, read the lock verbatim), compare
/// to machine state, and plan installs/upgrades/holds. Also gives the lock to
/// write once the install succeeds (`None` with `--frozen`, or when an entry
/// is unsatisfiable).
pub fn install_plan(
    env: &Env,
    opts: InstallOpts,
    pref: &[BackendId],
) -> Result<(Plan, Option<PendingLock>), CliError> {
    let file = manifest_file(env.user_files().as_ref(), opts.path)?;
    let manifest = load(env, &file)?;
    // Read state lazily per scope: a user-scope install must not touch (and
    // lock) root-owned system state it has no business reading.
    let mut state = ScopeCache::new(env);

    // §4.6: a claim records the manifest by its canonical absolute path, so two
    // projects' `./bosun.yaml` never collide and uninstall/prune can match it.
    let claim_path = canonical_manifest(env.user_files().as_ref(), &file);

    let (managed, carried, pending) = if opts.frozen {
        let lock = frozen_lock(env, &file, &claim_path, &manifest, pref)?;
        let (managed, carried) = managed_from_lock(env, &claim_path, &lock, &mut state)?;
        (managed, carried, None)
    } else {
        let relocked = relock(env, &file, &manifest, pref, &Release::None)?;
        let resolved = managed_from_resolution(env, &claim_path, &relocked.resolution, &mut state)?;
        let mut pending = relocked.pending;
        if let Some(pending) = &mut pending {
            pending.follow(&resolved.followed);
            pending.scopes = resolved.scopes;
        }
        (resolved.managed, resolved.carried, pending)
    };

    let plan =
        build_install_plan(&managed, carried).map_err(|e| CliError::Resolve(e.to_string()))?;
    Ok((plan, pending))
}

/// State read lazily, one scope at a time (§4.6): the system file is opened
/// only when a system-scope package needs it.
struct ScopeCache<'a> {
    env: &'a Env,
    sys: Option<Option<StateDocument>>,
    usr: Option<Option<StateDocument>>,
}

impl<'a> ScopeCache<'a> {
    fn new(env: &'a Env) -> ScopeCache<'a> {
        ScopeCache {
            env,
            sys: None,
            usr: None,
        }
    }
    /// `scope`'s state, or `None` when this run leaves it out
    /// ([`Env::visible_state`]).
    fn get(&mut self, scope: Scope) -> Result<Option<&StateDocument>, CliError> {
        let slot = match scope {
            Scope::System => &mut self.sys,
            Scope::User => &mut self.usr,
        };
        if slot.is_none() {
            *slot = Some(self.env.visible_state(scope)?);
        }
        Ok(slot.as_ref().and_then(Option::as_ref))
    }
}

/// Build the `uninstall` plan for one manifest (§9), from state, so it works
/// after the manifest is edited or deleted.
pub fn uninstall_plan(env: &Env, path: Option<PathBuf>) -> Result<Plan, CliError> {
    let manifest_path = claim_manifest_path(env.user_files().as_ref(), path);
    let state = combined_state(env)?;
    let installers = env.installers();
    build_uninstall_plan(&manifest_path, &state, &installers)
        .map_err(|e| CliError::Resolve(e.to_string()))
}

/// Build the machine-wide `prune` plan (§9).
pub fn prune_plan(env: &Env) -> Result<Plan, CliError> {
    let state = combined_state(env)?;
    let installers = env.installers();
    build_prune_plan(&state, &installers, manifest_present)
        .map_err(|e| CliError::Resolve(e.to_string()))
}

/// Whether a claim's manifest is still there, for `prune`. One that can't be
/// checked (in a directory this user may not search, such as another user's
/// home) counts as there: only a manifest known to be gone loses its claim.
fn manifest_present(path: &Path) -> bool {
    path.try_exists().unwrap_or_else(|e| {
        tracing::debug!(
            message = crate::logmsg::MANIFEST_UNCHECKED,
            path = %path.display(),
            error = %e
        );
        true
    })
}

fn managed_from_resolution<'a>(
    env: &'a Env,
    manifest_path: &Path,
    resolution: &bosun_resolve::Resolution,
    state: &mut ScopeCache,
) -> Result<Resolved<'a>, CliError> {
    let mut managed = Vec::new();
    let mut carried = Vec::new();
    let mut followed = Vec::new();
    let mut scopes = BTreeMap::new();
    for entry in &resolution.outcomes {
        match &entry.outcome {
            Outcome::Resolved {
                target,
                backend,
                version,
            } => {
                let pkg = resolution
                    .lock
                    .packages
                    .iter()
                    .find(|p| p.name == entry.name)
                    .expect("a resolved entry is in the lock");
                let leaf = pkg
                    .targets
                    .get(target)
                    .expect("the resolved target is in the lock");
                let installer = env.installer_for(backend).ok_or_else(|| {
                    CliError::Environment(format!("no installer for {}", backend.as_str()))
                })?;
                let vers_type = env.vers_type(backend).unwrap_or("generic").to_string();
                let purl = leaf.placement.purl.clone();
                scopes.insert(entry.name.clone(), installer.scope());
                // A scope this run leaves out is not planned for (§4.6).
                let Some(doc) = state.get(installer.scope())? else {
                    continue;
                };
                let claims = claims_for(manifest_path, &entry.name, &pkg.meta.canonical(), doc);
                // §4.6 A: a package not in bosun state is checked on the machine
                // (accepted if present), so one installed outside bosun is never
                // reinstalled or held.
                let (current, moved_from) = current_for(doc, &entry.name, installer, &purl)?;
                // §4.6 D: when bosun would act, select over every claim's
                // intersection (refusing an empty one), not just this manifest.
                let resolved = if needs_resolution(&current) {
                    let index = env.index_for(backend).ok_or_else(|| {
                        CliError::Environment(format!("no index for {}", backend.as_str()))
                    })?;
                    // Lockfiles advise, specs decide (§4.6): this project's
                    // locked version, when every claim admits it.
                    select_across_claims(
                        &claims,
                        &vers_type,
                        index,
                        &Ref::new(purl.clone()),
                        target,
                        std::slice::from_ref(version),
                    )
                    .map_err(|e| CliError::Resolve(e.to_string()))?
                } else {
                    accepted_version(&current, &claims, &vers_type)
                        .unwrap_or_else(|| version.clone())
                };
                let resolved = following_the_tool(
                    &entry.name,
                    &current,
                    moved_from.as_ref(),
                    &claims,
                    &vers_type,
                    resolved,
                );
                // The lock follows what the plan ends at: the package manager's
                // move, or another claim's intersection choosing elsewhere.
                if resolved != leaf.resolved {
                    followed.push((entry.name.clone(), *target, resolved.clone()));
                }
                let checksum = (resolved == leaf.resolved)
                    .then(|| leaf.checksum.clone())
                    .flatten();
                managed.push(Managed {
                    name: entry.name.clone(),
                    purl,
                    backend: backend.clone(),
                    resolved,
                    vers_type,
                    claims,
                    current,
                    installer,
                    // The lock's checksum is for its version; another one (a claim's
                    // intersection chose elsewhere) is the backend's to verify.
                    checksum,
                });
            }
            Outcome::NotApplicable(na) => carried.push(not_applicable_diagnostic(&entry.name, na)),
            Outcome::Unsatisfiable { spec, available } => carried.push(Diagnostic::Unsatisfiable {
                name: entry.name.clone(),
                spec: spec.clone(),
                available: available.clone(),
            }),
        }
    }
    Ok(Resolved {
        managed,
        carried,
        followed,
        scopes,
    })
}

/// What [`managed_from_resolution`] decided: the packages to plan for, the
/// diagnostics resolution carried, and each package whose end version follows
/// the package manager's move or every claim's choice rather than the lock,
/// by name and target, and the scope of each package resolved.
struct Resolved<'a> {
    managed: Vec<Managed<'a>>,
    carried: Vec<Diagnostic>,
    followed: Vec<(String, Target, Version)>,
    /// Each resolved package's install scope.
    scopes: BTreeMap<String, Scope>,
}

fn managed_from_lock<'a>(
    env: &'a Env,
    manifest_path: &Path,
    lock: &LockDocument,
    state: &mut ScopeCache,
) -> Result<(Vec<Managed<'a>>, Vec<Diagnostic>), CliError> {
    let mut managed = Vec::new();
    let mut diagnostics = Vec::new();
    for pkg in &lock.packages {
        // The leaf for this machine's platform, or the platform-independent one.
        let leaf = match pkg
            .targets
            .get(&env.target())
            .or_else(|| pkg.targets.get(&Target::Any))
        {
            Some(leaf) => leaf,
            None => continue,
        };
        let backend = &leaf.placement.backend;
        let installer = env.installer_for(backend).ok_or_else(|| {
            CliError::Environment(format!("no installer for {}", backend.as_str()))
        })?;
        let vers_type = env.vers_type(backend).unwrap_or("generic").to_string();
        // A scope this run leaves out is not planned for (§4.6).
        let Some(doc) = state.get(installer.scope())? else {
            continue;
        };
        let purl = leaf.placement.purl.clone();
        let claims = claims_for(manifest_path, &pkg.name, &pkg.meta.canonical(), doc);
        // §7 --frozen: refuse if another claim's spec excludes the locked version.
        frozen_select(&leaf.resolved, &claims, &vers_type)
            .map_err(|e| CliError::Resolve(e.to_string()))?;
        // §4.6 A: present outside bosun → accepted, not reinstalled.
        let (current, _) = current_for(doc, &pkg.name, installer, &purl)?;
        // §4.6/§7 --frozen: a package bosun does not own, installed at a version
        // other than the one the lock pins, is a conflict bosun will not resolve
        // by changing it. Error and skip; the error blocks apply.
        if let Current::Present {
            version: Some(v),
            origin: Origin::Accepted,
            ..
        } = &current
        {
            if v.as_str() != leaf.resolved.as_str() {
                diagnostics.push(Diagnostic::FrozenAcceptedConflict {
                    name: pkg.name.clone(),
                    installed: v.as_str().to_string(),
                    locked: leaf.resolved.as_str().to_string(),
                });
                continue;
            }
        }
        managed.push(Managed {
            name: pkg.name.clone(),
            purl,
            backend: backend.clone(),
            resolved: leaf.resolved.clone(),
            vers_type,
            claims,
            current,
            installer,
            checksum: leaf.checksum.clone(),
        });
    }
    Ok((managed, diagnostics))
}

fn not_applicable_diagnostic(name: &str, na: &NotApplicable) -> Diagnostic {
    match na {
        NotApplicable::NoRegisteredBackend { candidates } => Diagnostic::MissingBackend {
            name: name.to_string(),
            candidates: candidates.clone(),
        },
        NotApplicable::NoSelector | NotApplicable::NoBackend => Diagnostic::NotApplicable {
            name: name.to_string(),
        },
    }
}

/// The current state of a package (§4.6). For a package bosun state records,
/// its version is the one the package manager reports now (its own updates
/// move it under a range), the recorded one where it reports none; the
/// recorded version comes back too when the two differ. A package state does
/// not record is checked on the machine with `verify_installed` and, if
/// present, treated as `accepted` — so a package installed outside bosun is
/// never reinstalled or held, only accepted.
fn current_for(
    doc: &StateDocument,
    name: &str,
    installer: &dyn Installer,
    purl: &bosun_core::Purl,
) -> Result<(Current, Option<Version>), CliError> {
    if let Some(leaf) = doc
        .packages
        .iter()
        .find(|p| p.name == name)
        .and_then(|p| p.targets.values().next())
    {
        let reported = bosun_apply::reported_installed(installer, &leaf.placement.purl)
            .map_err(|e| CliError::Resolve(e.to_string()))?;
        let moved_from = reported
            .as_ref()
            .filter(|now| **now != leaf.installed)
            .map(|_| leaf.installed.clone());
        if let (Some(recorded), Some(now)) = (&moved_from, &reported) {
            info!(
                message = crate::logmsg::INSTALLED_MOVED,
                package = %name,
                recorded = %recorded,
                installed = %now,
            );
        }
        return Ok((
            Current::Present {
                version: Some(reported.unwrap_or_else(|| leaf.installed.clone())),
                origin: leaf.origin,
                claimed_by: leaf.claimed_by.clone(),
                held: leaf.held,
            },
            moved_from,
        ));
    }
    match installer
        .verify_installed(&Ref::new(purl.clone()))
        .map_err(|e| CliError::Resolve(e.to_string()))?
    {
        Installed::Present { version } => Ok((
            Current::Present {
                version,
                origin: Origin::Accepted,
                claimed_by: Vec::new(),
                held: false,
            },
            None,
        )),
        Installed::Absent => Ok((Current::Absent, None)),
    }
}

/// The version to end at when the package manager moved a package bosun
/// recorded: its own version where that is newer than `resolved` and every
/// claim admits it, so a distro's update within the spec is never taken
/// back; else `resolved`. A version it moved outside the spec is said, and
/// planned for as any other.
fn following_the_tool(
    name: &str,
    current: &Current,
    moved_from: Option<&Version>,
    claims: &[Claim],
    vers_type: &str,
    resolved: Version,
) -> Version {
    let (
        Some(_),
        Current::Present {
            version: Some(now), ..
        },
    ) = (moved_from, current)
    else {
        return resolved;
    };
    if !bosun_apply::satisfies_all(now, claims, vers_type) {
        warn!(message = crate::logmsg::INSTALLED_OUTSIDE_SPEC, package = %name, installed = %now);
        return resolved;
    }
    resolved.max(now.clone())
}

/// The version an accepted package ends at: the one on the machine, when it is
/// known and every claim admits it, so the lock pins what is installed (§4.6).
fn accepted_version(current: &Current, claims: &[Claim], vers_type: &str) -> Option<Version> {
    let Current::Present {
        version: Some(now), ..
    } = current
    else {
        return None;
    };
    bosun_apply::satisfies_all(now, claims, vers_type).then(|| now.clone())
}

/// Whether bosun would install/upgrade/downgrade (so it needs a resolved
/// version). A package present but not bosun-owned (`accepted`) is left alone.
fn needs_resolution(current: &Current) -> bool {
    !matches!(
        current,
        Current::Present {
            origin: Origin::Accepted,
            ..
        }
    )
}

/// The claims to record: every other manifest's claim on this package, plus the
/// current project's (canonical spec; the applier stamps the user, §4.6).
fn claims_for(manifest_path: &Path, name: &str, spec: &str, doc: &StateDocument) -> Vec<Claim> {
    let mut claims: Vec<Claim> = doc
        .packages
        .iter()
        .find(|p| p.name == name)
        .and_then(|p| p.targets.values().next())
        .map(|leaf| {
            leaf.claimed_by
                .iter()
                .filter(|c| c.manifest != manifest_path)
                .cloned()
                .collect()
        })
        .unwrap_or_default();
    claims.push(Claim {
        manifest: manifest_path.to_path_buf(),
        spec: spec.to_string(),
        user: None,
    });
    claims
}

/// `scope`'s machine state; empty when this run leaves it out
/// ([`Env::visible_state`]).
fn read_state(env: &Env, scope: Scope) -> Result<StateDocument, CliError> {
    Ok(env.visible_state(scope)?.unwrap_or_else(|| StateDocument {
        state_version: bosun_io::state::STATE_VERSION,
        packages: Vec::new(),
    }))
}

pub(crate) fn combined_state(env: &Env) -> Result<StateDocument, CliError> {
    let mut packages = read_state(env, Scope::System)?.packages;
    packages.extend(read_state(env, Scope::User)?.packages);
    Ok(StateDocument {
        state_version: 1,
        packages,
    })
}

/// This machine's lock for the manifest, for `install --frozen` (§7): it must
/// exist, be for this manifest, and be locked against the manifest as it is
/// now, or there is nothing to install verbatim.
fn frozen_lock(
    env: &Env,
    file: &Path,
    claim_path: &Path,
    manifest: &Manifest,
    pref: &[BackendId],
) -> Result<LockDocument, CliError> {
    let lock = this_lock(env, claim_path)?.ok_or_else(|| {
        CliError::Resolve(format!(
            "--frozen needs this machine's lock for {}; run `bosun lock`",
            file.display()
        ))
    })?;
    if lock.manifest_hash != manifest_hash(manifest) {
        return Err(CliError::Resolve(format!(
            "--frozen: {} changed since it was locked; run `bosun lock`",
            file.display()
        )));
    }
    // A lock `install --scope` wrote first lacks the other scope's entries.
    let missing: Vec<&str> = applicable(manifest, &env.host(), &env.resolvables(), pref)
        .into_iter()
        .filter(|name| !lock.packages.iter().any(|p| p.name == *name))
        .collect();
    if !missing.is_empty() {
        return Err(CliError::Resolve(format!(
            "--frozen: {}: {}; {}",
            crate::errmsg::FROZEN_LOCK_INCOMPLETE,
            missing.join(", "),
            crate::errmsg::RUN_LOCK
        )));
    }
    Ok(lock)
}

/// Resolve the manifest path for a claim without requiring the file to exist
/// (uninstall works after the repo is gone, §9).
fn claim_manifest_path(files: &dyn FileAccess, path: Option<PathBuf>) -> PathBuf {
    let base = path.unwrap_or_else(|| PathBuf::from("."));
    let file = match base.extension().and_then(|e| e.to_str()) {
        Some("yaml") | Some("yml") => base,
        _ => base.join("bosun.yaml"),
    };
    canonical_manifest(files, &file)
}

/// The canonical absolute form of a manifest path for a claim and the lock it
/// keys (§4.5, §4.6): its symlinks resolved through `files`, as the manifest's
/// user, when the file exists; else made absolute without resolving (the repo
/// is gone — uninstall/prune then match the path bosun recorded while it
/// still existed, or prune's "manifest no longer exists" check takes over, §9).
pub(crate) fn canonical_manifest(files: &dyn FileAccess, file: &Path) -> PathBuf {
    files
        .canonicalize(file)
        .or_else(|_| std::path::absolute(file))
        .unwrap_or_else(|_| file.to_path_buf())
}

impl Render for Plan {
    fn text(&self) -> String {
        let mut lines = Vec::new();
        for a in &self.actions {
            let version = match (&a.from, &a.to) {
                (Some(f), Some(t)) => format!("{} → {}", f.as_str(), t.as_str()),
                (None, Some(t)) => t.as_str().to_string(),
                (Some(f), None) => f.as_str().to_string(),
                (None, None) => String::new(),
            };
            lines.push(format!(
                "{:<10} {:<16} {:<8} {}",
                a.kind.label(),
                a.name,
                a.backend.as_str(),
                version
            ));
        }
        for d in &self.diagnostics {
            lines.push(format!("! {d}"));
        }
        lines.push(format!("Plan: {} action(s)", self.actions.len()));
        lines.join("\n")
    }

    fn exit_code(&self) -> i32 {
        Plan::exit_code(self)
    }
}

impl Render for ApplyReport {
    fn text(&self) -> String {
        if self.outcomes.is_empty() {
            return "nothing to apply".to_string();
        }
        self.outcomes
            .iter()
            .map(|o| {
                let result = match &o.result {
                    ApplyResult::Applied => "applied".to_string(),
                    ApplyResult::Failed { error } => format!("FAILED: {error}"),
                    ApplyResult::Skipped => "skipped".to_string(),
                    ApplyResult::Deferred => bosun_apply::errmsg::DEFERRED.to_string(),
                };
                format!("{:<10} {:<16} {}", o.kind.label(), o.name, result)
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn ids(order: &[BackendId]) -> Vec<&str> {
        order.iter().map(BackendId::as_str).collect()
    }

    #[test]
    fn a_manifest_that_is_there_or_gone_is_present_or_not() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = dir.path().join("bosun.yaml");
        assert!(!manifest_present(&manifest));
        fs::write(&manifest, "version: 2\n").unwrap();
        assert!(manifest_present(&manifest));
    }

    /// A manifest in a directory this user may not search (another user's
    /// 0700 home) can't be checked, so its claim stands. Root searches any
    /// directory, so there it is not run.
    #[cfg(unix)]
    #[test]
    fn a_manifest_that_cannot_be_checked_counts_as_present() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        fs::create_dir(&home).unwrap();
        fs::write(home.join("bosun.yaml"), "version: 2\n").unwrap();
        fs::set_permissions(&home, fs::Permissions::from_mode(0o000)).unwrap();
        let searchable = fs::metadata(home.join("bosun.yaml")).is_ok();
        let present = manifest_present(&home.join("bosun.yaml"));
        let gone = manifest_present(&home.join("gone.yaml"));
        fs::set_permissions(&home, fs::Permissions::from_mode(0o700)).unwrap();
        if searchable {
            return;
        }
        assert!(present);
        assert!(gone);
    }

    #[test]
    fn the_libc_line_is_linux_only_and_says_what_is_known() {
        let glibc = Libc {
            family: bosun_core::LibcFamily::Gnu,
            version: Some("2.36".into()),
        };
        let known = libc_info(Some(&glibc));
        assert_eq!(
            libc_line(Os::Linux, known.as_ref()).as_deref(),
            Some("libc: gnu 2.36")
        );
        let versionless = libc_info(Some(&Libc {
            version: None,
            ..glibc
        }));
        assert_eq!(
            libc_line(Os::Linux, versionless.as_ref()).as_deref(),
            Some("libc: gnu")
        );
        assert_eq!(libc_line(Os::Linux, None).as_deref(), Some("libc: unknown"));
        assert_eq!(libc_line(Os::Windows, None), None);
        assert_eq!(libc_line(Os::Macos, known.as_ref()), None);
    }

    fn strings(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    fn registered(v: &[(&str, Scope)]) -> Vec<(BackendId, Scope)> {
        v.iter().map(|(id, s)| (BackendId::new(*id), *s)).collect()
    }

    fn upgrade_output(changes: &[(&str, Option<&str>, &str)], error: bool) -> UpgradeOutput {
        UpgradeOutput {
            changes: changes
                .iter()
                .map(|(name, from, to)| VersionChange {
                    name: name.to_string(),
                    from: from.map(str::to_string),
                    to: to.to_string(),
                })
                .collect(),
            lock: LockOutput {
                error,
                resolved: Vec::new(),
                diagnostics: Vec::new(),
                lock: None,
            },
        }
    }

    #[test]
    fn upgrade_text_lists_each_move_and_says_to_install() {
        let text = upgrade_output(
            &[
                ("ripgrep", Some("14.1.1"), "14.2.0"),
                ("fd", None, "10.2.0"),
            ],
            false,
        )
        .text();
        assert!(text.contains("ripgrep          14.1.1 -> 14.2.0"), "{text}");
        assert!(text.contains("fd               10.2.0 (new)"), "{text}");
        assert!(text.contains("Run `bosun install`"), "{text}");
        assert!(!text.contains("Everything is at"), "{text}");
    }

    #[test]
    fn upgrade_text_with_nothing_to_move_says_so_and_nothing_to_install() {
        let text = upgrade_output(&[], false).text();
        assert!(text.contains("Everything is at the newest"), "{text}");
        assert!(!text.contains("Run `bosun install`"), "{text}");
    }

    #[test]
    fn a_failed_upgrade_neither_claims_newest_nor_says_install_and_exits_1() {
        let out = upgrade_output(&[("ripgrep", Some("14.1.1"), "14.2.0")], true);
        let text = out.text();
        assert!(!text.contains("Run `bosun install`"), "{text}");
        assert_eq!(out.exit_code(), 1);
        let none = upgrade_output(&[], true).text();
        assert!(!none.contains("Everything is at"), "{none}");
        assert_eq!(upgrade_output(&[], false).exit_code(), 0);
    }

    #[test]
    fn the_resolver_stamps_the_lock_version_bosun_io_reads() {
        assert_eq!(
            bosun_resolve::LOCKFILE_VERSION,
            bosun_io::lock::LOCKFILE_VERSION
        );
    }

    #[test]
    fn flag_then_env_then_config_then_the_built_in_order() {
        let order = preference_order(
            &strings(&["brew"]),
            Some("cargo"),
            &strings(&["apt"]),
            &registered(&[("winget", Scope::User)]),
        );
        assert_eq!(ids(&order), vec!["brew", "cargo", "apt", "winget"]);
    }

    #[test]
    fn the_built_in_order_puts_user_scope_before_system_in_registration_order() {
        // So with no preference stated, nothing needs root or Administrator
        // when a user-scope backend can do it.
        let order = preference_order(
            &[],
            None,
            &[],
            &registered(&[
                ("apt", Scope::System),
                ("cargo", Scope::User),
                ("choco", Scope::System),
                ("scoop", Scope::User),
            ]),
        );
        assert_eq!(ids(&order), vec!["cargo", "scoop", "apt", "choco"]);
    }

    #[test]
    fn a_stated_preference_wins_over_the_scope_rule() {
        let order = preference_order(
            &[],
            None,
            &strings(&["apt"]),
            &registered(&[("cargo", Scope::User), ("apt", Scope::System)]),
        );
        assert_eq!(ids(&order), vec!["apt", "cargo"]);
    }

    #[test]
    fn env_is_comma_separated_and_trimmed() {
        let order = preference_order(&[], Some(" scoop , winget "), &[], &[]);
        assert_eq!(ids(&order), vec!["scoop", "winget"]);
    }

    #[test]
    fn repeats_appear_once_and_empty_names_never() {
        let order = preference_order(
            &strings(&["apt", "", "apt"]),
            Some("apt,,cargo"),
            &strings(&["cargo"]),
            &registered(&[("apt", Scope::System)]),
        );
        assert_eq!(ids(&order), vec!["apt", "cargo"]);
    }

    #[test]
    fn a_missing_config_file_states_no_preference() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            config_installer(&bosun_io::DirectFiles, &dir.path().join("config.yaml")).unwrap(),
            Vec::<String>::new()
        );
    }

    #[test]
    fn a_config_that_exists_but_cannot_be_read_is_an_error() {
        // A directory where the file should be: present, but unreadable.
        let dir = tempfile::tempdir().unwrap();
        let err = config_installer(&bosun_io::DirectFiles, dir.path()).unwrap_err();
        assert!(matches!(err, CliError::Io { .. }), "{err:?}");
    }

    #[test]
    fn a_config_file_that_does_not_load_is_an_error_naming_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.yaml");
        fs::write(&path, "version: 1\ninstallers: [cargo]\n").unwrap();
        let err = config_installer(&bosun_io::DirectFiles, &path).unwrap_err();
        assert!(matches!(err, CliError::Parse { .. }), "{err:?}");
        assert!(err.to_string().contains("installers"), "{err}");
    }

    #[test]
    fn a_config_file_gives_its_installer_order() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.yaml");
        fs::write(&path, "installer: [scoop, winget]\n").unwrap();
        assert_eq!(
            config_installer(&bosun_io::DirectFiles, &path).unwrap(),
            vec!["scoop", "winget"]
        );
    }

    fn deb(raw: &str) -> Version {
        Version::parse(raw, bosun_core::Scheme::Debian).unwrap()
    }

    fn at(raw: &str) -> Current {
        Current::Present {
            version: Some(deb(raw)),
            origin: Origin::Installed,
            claimed_by: Vec::new(),
            held: false,
        }
    }

    fn wanting(spec: &str) -> Vec<Claim> {
        vec![Claim {
            manifest: PathBuf::from("/w/a/bosun.yaml"),
            spec: format!("vers:deb/{spec}"),
            user: None,
        }]
    }

    fn follow(current: &Current, moved_from: Option<&str>, spec: &str, resolved: &str) -> String {
        let moved_from = moved_from.map(deb);
        following_the_tool(
            "jq",
            current,
            moved_from.as_ref(),
            &wanting(spec),
            "deb",
            deb(resolved),
        )
        .as_str()
        .to_string()
    }

    #[test]
    fn an_accepted_package_ends_at_the_version_on_the_machine_when_every_claim_admits_it() {
        let accept = |v: Option<&str>, spec: &str| {
            let current = Current::Present {
                version: v.map(deb),
                origin: Origin::Accepted,
                claimed_by: Vec::new(),
                held: false,
            };
            accepted_version(&current, &wanting(spec), "deb").map(|v| v.as_str().to_string())
        };
        assert_eq!(accept(Some("1.1-1"), ">=1.0").as_deref(), Some("1.1-1"));
        assert_eq!(accept(Some("1.1-1"), ">=1.2"), None);
        assert_eq!(accept(None, ">=1.0"), None);
        assert_eq!(
            accepted_version(&Current::Absent, &wanting(">=1.0"), "deb"),
            None
        );
    }

    #[test]
    fn a_package_the_package_manager_moved_up_within_the_spec_stays_there() {
        assert_eq!(
            follow(&at("1.2-1"), Some("1.1-1"), ">=1.0", "1.1-1"),
            "1.2-1"
        );
    }

    #[test]
    fn the_resolved_version_stands_unless_the_package_manager_moved_past_it() {
        // Not moved: the lock's choice stands, whatever is installed.
        assert_eq!(follow(&at("1.2-1"), None, ">=1.0", "1.1-1"), "1.1-1");
        // Moved, but the lock already asks for more: upgrade to it.
        assert_eq!(
            follow(&at("1.2-1"), Some("1.1-1"), ">=1.0", "1.3-1"),
            "1.3-1"
        );
        // Moved to exactly what is resolved.
        assert_eq!(
            follow(&at("1.2-1"), Some("1.1-1"), ">=1.0", "1.2-1"),
            "1.2-1"
        );
        // Version unknown, or absent.
        let unknown = Current::Present {
            version: None,
            origin: Origin::Installed,
            claimed_by: Vec::new(),
            held: false,
        };
        assert_eq!(follow(&unknown, Some("1.1-1"), ">=1.0", "1.1-1"), "1.1-1");
        assert_eq!(
            follow(&Current::Absent, Some("1.1-1"), ">=1.0", "1.1-1"),
            "1.1-1"
        );
    }

    #[test]
    fn a_package_the_package_manager_moved_outside_the_spec_is_planned_back() {
        assert_eq!(
            follow(&at("2.0-1"), Some("1.1-1"), "<2.0", "1.1-1"),
            "1.1-1"
        );
    }

    fn locked(pins: &[(&str, &str)]) -> LockDocument {
        let pin = |(name, version): &(&str, &str)| {
            let leaf = LockLeaf {
                placement: bosun_core::record::Placement {
                    backend: BackendId::new("apt"),
                    purl: format!("pkg:deb/debian/{name}").parse().unwrap(),
                },
                spec: None,
                resolved: deb(version),
                index_snapshot: None,
                checksum: None,
            };
            Package {
                name: name.to_string(),
                meta: Spec::new("*", "deb").unwrap(),
                targets: BTreeMap::from([(Target::Any, leaf)]),
            }
        };
        LockDocument {
            lockfile_version: bosun_resolve::LOCKFILE_VERSION,
            manifest: "/w/bosun.yaml".to_string(),
            manifest_hash: "sha256:0".to_string(),
            packages: pins.iter().map(pin).collect(),
        }
    }

    fn pins(doc: &LockDocument) -> Vec<(String, String)> {
        doc.packages
            .iter()
            .map(|p| {
                let leaf = p.targets.values().next().unwrap();
                (p.name.clone(), leaf.resolved.as_str().to_string())
            })
            .collect()
    }

    fn owned(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(n, v)| (n.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn a_missing_lock_is_none_and_one_that_cannot_be_read_is_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        let schemes = bosun_io::BackendSchemes::new();
        let files = bosun_io::DirectFiles;
        let missing = tmp.path().join("none.json");
        assert!(read_lock(&files, &missing, &schemes).unwrap().is_none());
        let err = read_lock(&files, tmp.path(), &schemes).unwrap_err();
        assert!(matches!(err, CliError::Io { .. }), "{err}");
        let not_text = tmp.path().join("bad.json");
        fs::write(&not_text, [0xff, 0xfe]).unwrap();
        let err = read_lock(&files, &not_text, &schemes).unwrap_err();
        assert!(matches!(err, CliError::Parse { .. }), "{err}");
    }

    /// A pending lock moving jq (system), rg (user) and fd (no scope
    /// recorded) to 2.0-1, from a previous lock pinning jq and rg at 1.0-1.
    fn pending() -> PendingLock {
        PendingLock {
            path: PathBuf::from("/l.json"),
            doc: locked(&[("jq", "2.0-1"), ("rg", "2.0-1"), ("fd", "2.0-1")]),
            previous: Some(locked(&[("jq", "1.0-1"), ("rg", "1.0-1")])),
            scopes: BTreeMap::from([
                ("jq".to_string(), Scope::System),
                ("rg".to_string(), Scope::User),
            ]),
            files: std::rc::Rc::new(bosun_io::DirectFiles),
        }
    }

    #[test]
    fn without_scope_the_lock_takes_every_resolved_entry() {
        let mut lock = pending();
        lock.only_requested_scope(None);
        assert_eq!(
            pins(&lock.doc),
            owned(&[("jq", "2.0-1"), ("rg", "2.0-1"), ("fd", "2.0-1")])
        );
    }

    #[test]
    fn a_scope_the_install_was_narrowed_away_from_keeps_its_pins() {
        let mut user = pending();
        user.only_requested_scope(Some(Scope::User));
        assert_eq!(
            pins(&user.doc),
            owned(&[("jq", "1.0-1"), ("rg", "2.0-1"), ("fd", "2.0-1")])
        );
        let mut system = pending();
        system.only_requested_scope(Some(Scope::System));
        assert_eq!(
            pins(&system.doc),
            owned(&[("jq", "2.0-1"), ("rg", "1.0-1"), ("fd", "2.0-1")])
        );
    }

    #[test]
    fn with_no_previous_lock_a_scope_left_out_is_absent() {
        let mut lock = pending();
        lock.previous = None;
        lock.only_requested_scope(Some(Scope::User));
        assert_eq!(pins(&lock.doc), owned(&[("rg", "2.0-1"), ("fd", "2.0-1")]));
    }
}
