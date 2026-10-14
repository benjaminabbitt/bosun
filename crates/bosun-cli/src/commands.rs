//! The M1 read/resolve commands (§4.2): `backends`, `status`, `lock [--check]`.
//! Each returns one typed value; the mutating commands are slice 10-2.

use std::fs;
use std::path::{Path, PathBuf};

use bosun_core::record::{LockDocument, StateDocument};
use bosun_core::{BackendId, Manifest, Scope};
use bosun_io::{load_manifest, lock, manifest_hash};
use bosun_ports::StateStore;
use bosun_resolve::{
    check_targets, merge_lock, resolve, Freshness, NotApplicable, Outcome, Resolution,
};
use serde::Serialize;

use crate::env::Env;
use crate::error::CliError;
use crate::output::Render;

/// The installer preference order (§5.3): command-line flag, then
/// `BOSUN_INSTALLER`, then user config; the manifest's own order is applied
/// per entry inside the resolver.
pub fn preference(flag: &[String]) -> Vec<BackendId> {
    let mut order: Vec<String> = Vec::new();
    let mut add = |items: Vec<String>| {
        for item in items {
            if !item.is_empty() && !order.contains(&item) {
                order.push(item);
            }
        }
    };
    add(flag.to_vec());
    if let Ok(env) = std::env::var("BOSUN_INSTALLER") {
        add(env.split(',').map(|s| s.trim().to_string()).collect());
    }
    if let Ok(path) = bosun_io::paths::config_path() {
        if let Ok(text) = fs::read_to_string(&path) {
            if let Ok(cfg) = bosun_io::config::from_yaml(&text, &path) {
                add(cfg.installer);
            }
        }
    }
    order.into_iter().map(BackendId::new).collect()
}

fn manifest_file(path: Option<PathBuf>) -> Result<PathBuf, CliError> {
    let base = path.unwrap_or_else(|| PathBuf::from("."));
    let file = if base.is_dir() {
        base.join("bosun.yaml")
    } else {
        base
    };
    if file.exists() {
        Ok(file)
    } else {
        Err(CliError::ManifestNotFound(file))
    }
}

fn load(file: &Path) -> Result<Manifest, CliError> {
    let text = fs::read_to_string(file).map_err(|e| CliError::Io {
        path: file.to_path_buf(),
        detail: e.to_string(),
    })?;
    load_manifest(&text, file).map_err(|e| CliError::Parse {
        what: "manifest".to_string(),
        detail: e.to_string(),
    })
}

// --- backends --------------------------------------------------------------

/// The `backends` output (§5.3).
#[derive(Serialize)]
pub struct BackendsOutput {
    registered: Vec<BackendInfo>,
    skipped: Vec<SkippedInfo>,
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
            "no backends registered".to_string()
        } else {
            lines.join("\n")
        }
    }
}

// --- status ----------------------------------------------------------------

/// The `status` output (§4.6).
#[derive(Serialize)]
pub struct StatusOutput {
    packages: Vec<PackageStatus>,
    diagnostics: Vec<String>,
}

#[derive(Serialize)]
struct PackageStatus {
    name: String,
    backend: String,
    installed: Option<String>,
    wanted: String,
    up_to_date: bool,
}

/// Report installed-vs-wanted for each applicable package.
pub fn status(
    env: &Env,
    path: Option<PathBuf>,
    pref: &[BackendId],
) -> Result<StatusOutput, CliError> {
    let file = manifest_file(path)?;
    let manifest = load(&file)?;
    let hash = manifest_hash(&manifest);
    let resolution = resolve(&manifest, &hash, &env.host(), &env.resolvables(), pref)
        .map_err(|e| CliError::Resolve(e.to_string()))?;

    let system = env
        .state
        .read(Scope::System)
        .map_err(|e| CliError::Environment(e.to_string()))?;
    let user = env
        .state
        .read(Scope::User)
        .map_err(|e| CliError::Environment(e.to_string()))?;

    let mut packages = Vec::new();
    let mut diagnostics = Vec::new();
    for entry in resolution.outcomes {
        match entry.outcome {
            Outcome::Resolved {
                backend, version, ..
            } => {
                let installed = installed_version(&[&system, &user], &entry.name);
                let up_to_date = installed.as_deref() == Some(version.as_str());
                packages.push(PackageStatus {
                    name: entry.name,
                    backend: backend.as_str().to_string(),
                    installed,
                    wanted: version.as_str().to_string(),
                    up_to_date,
                });
            }
            Outcome::NotApplicable(na) => {
                diagnostics.push(format!("{}: not applicable ({})", entry.name, na_text(&na)))
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
    })
}

impl Render for StatusOutput {
    fn text(&self) -> String {
        let mut lines = Vec::new();
        for p in &self.packages {
            let state = match &p.installed {
                None => "not installed".to_string(),
                Some(v) if p.up_to_date => format!("{v} (up to date)"),
                Some(v) => format!("{v} → {} (outdated)", p.wanted),
            };
            lines.push(format!("{:<16} {:<8} {}", p.name, p.backend, state));
        }
        for d in &self.diagnostics {
            lines.push(d.clone());
        }
        if lines.is_empty() {
            "no packages".to_string()
        } else {
            lines.join("\n")
        }
    }
}

// --- lock ------------------------------------------------------------------

/// The `lock` output: the resolution (when writing) or the freshness report
/// (when `--check`).
#[derive(Serialize)]
pub struct LockOutput {
    check: bool,
    resolved: Vec<ResolvedInfo>,
    diagnostics: Vec<String>,
    targets: Vec<TargetFreshness>,
}

#[derive(Serialize)]
struct ResolvedInfo {
    name: String,
    backend: String,
    target: String,
    version: String,
}

#[derive(Serialize)]
struct TargetFreshness {
    target: String,
    freshness: String,
}

/// Resolve this platform (and `any`) and merge into `bosun.lock`; `--check`
/// instead reports declared targets that are missing or stale (§4.7).
pub fn lock(
    env: &Env,
    path: Option<PathBuf>,
    check: bool,
    pref: &[BackendId],
) -> Result<LockOutput, CliError> {
    let file = manifest_file(path)?;
    let manifest = load(&file)?;
    let hash = manifest_hash(&manifest);
    let lock_path = file
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("bosun.lock");
    let existing = read_lock(&lock_path)?;

    if check {
        let doc = existing.unwrap_or_else(empty_lock);
        let targets = check_targets(&manifest.targets, &hash, &doc)
            .into_iter()
            .map(|t| TargetFreshness {
                target: t.target.to_string(),
                freshness: freshness_text(t.freshness).to_string(),
            })
            .collect();
        return Ok(LockOutput {
            check: true,
            resolved: Vec::new(),
            diagnostics: Vec::new(),
            targets,
        });
    }

    let resolution = resolve(&manifest, &hash, &env.host(), &env.resolvables(), pref)
        .map_err(|e| CliError::Resolve(e.to_string()))?;
    let merged = match existing {
        Some(prev) => merge_lock(&prev, &resolution.lock),
        None => resolution.lock.clone(),
    };
    fs::write(&lock_path, lock::to_json(&merged)).map_err(|e| CliError::Io {
        path: lock_path.clone(),
        detail: e.to_string(),
    })?;
    Ok(lock_output(resolution))
}

fn lock_output(resolution: Resolution) -> LockOutput {
    let mut resolved = Vec::new();
    let mut diagnostics = Vec::new();
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
            Outcome::Unsatisfiable { spec, available } => diagnostics.push(format!(
                "{}: unsatisfiable {spec} (available: {available})",
                entry.name
            )),
        }
    }
    LockOutput {
        check: false,
        resolved,
        diagnostics,
        targets: Vec::new(),
    }
}

impl Render for LockOutput {
    fn text(&self) -> String {
        if self.check {
            let mut lines: Vec<String> = self
                .targets
                .iter()
                .map(|t| format!("{:<16} {}", t.target, t.freshness))
                .collect();
            if lines.is_empty() {
                lines.push("no targets declared".to_string());
            }
            return lines.join("\n");
        }
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
        lines.push(format!("Locked: {} resolved", self.resolved.len()));
        lines.join("\n")
    }

    fn exit_code(&self) -> i32 {
        // lock --check is a CI gate: any missing or stale target is pending (§4.7).
        if self.check && self.targets.iter().any(|t| t.freshness != "fresh") {
            2
        } else {
            0
        }
    }
}

// --- helpers ---------------------------------------------------------------

fn installed_version(docs: &[&StateDocument], name: &str) -> Option<String> {
    for doc in docs {
        if let Some(pkg) = doc.packages.iter().find(|p| p.name == name) {
            if let Some(leaf) = pkg.targets.values().next() {
                return Some(leaf.installed.as_str().to_string());
            }
        }
    }
    None
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

fn freshness_text(f: Freshness) -> &'static str {
    match f {
        Freshness::Fresh => "fresh",
        Freshness::Stale => "stale",
        Freshness::Missing => "missing",
    }
}

fn read_lock(path: &Path) -> Result<Option<LockDocument>, CliError> {
    if !path.exists() {
        return Ok(None);
    }
    let text = fs::read_to_string(path).map_err(|e| CliError::Io {
        path: path.to_path_buf(),
        detail: e.to_string(),
    })?;
    let doc = lock::from_json(&text, path).map_err(|e| CliError::Parse {
        what: "lockfile".to_string(),
        detail: e.to_string(),
    })?;
    Ok(Some(doc))
}

fn empty_lock() -> LockDocument {
    LockDocument {
        lockfile_version: 1,
        targets: std::collections::BTreeMap::new(),
        packages: Vec::new(),
    }
}

// --- install / uninstall / prune (§4.2, §4.8) ------------------------------

use bosun_apply::{
    build_install_plan, build_prune_plan, build_uninstall_plan, ApplyReport, ApplyResult, Current,
    Diagnostic, Managed, Plan,
};
use bosun_core::{Claim, Target};
use bosun_resolve::frozen_select;

/// Options for `install`.
pub struct InstallOpts {
    /// The manifest directory or file.
    pub path: Option<PathBuf>,
    /// Apply the lockfile verbatim, no re-resolve (§7).
    pub frozen: bool,
}

/// Build the `install` plan (§4.8): resolve the manifest (or read the lock with
/// `--frozen`), compare to machine state, and plan installs/upgrades/holds.
pub fn install_plan(env: &Env, opts: InstallOpts, pref: &[BackendId]) -> Result<Plan, CliError> {
    let file = manifest_file(opts.path)?;
    let manifest = load(&file)?;
    let hash = manifest_hash(&manifest);
    let sys = read_state(env, Scope::System)?;
    let usr = read_state(env, Scope::User)?;

    let (managed, carried) = if opts.frozen {
        let lock_path = lock_path(&file);
        let lock = read_lock(&lock_path)?.ok_or_else(|| {
            CliError::Resolve(format!(
                "--frozen needs a lockfile at {}",
                lock_path.display()
            ))
        })?;
        (
            managed_from_lock(env, &file, &lock, &sys, &usr)?,
            Vec::new(),
        )
    } else {
        let resolution = resolve(&manifest, &hash, &env.host(), &env.resolvables(), pref)
            .map_err(|e| CliError::Resolve(e.to_string()))?;
        managed_from_resolution(env, &file, &resolution, &sys, &usr)?
    };

    build_install_plan(&managed, carried).map_err(|e| CliError::Resolve(e.to_string()))
}

/// Build the `uninstall` plan for one manifest (§9), from state, so it works
/// after the manifest is edited or deleted.
pub fn uninstall_plan(env: &Env, path: Option<PathBuf>) -> Result<Plan, CliError> {
    let manifest_path = claim_manifest_path(path);
    let state = combined_state(env)?;
    let installers = env.installers();
    build_uninstall_plan(&manifest_path, &state, &installers)
        .map_err(|e| CliError::Resolve(e.to_string()))
}

/// Build the machine-wide `prune` plan (§9).
pub fn prune_plan(env: &Env) -> Result<Plan, CliError> {
    let state = combined_state(env)?;
    let installers = env.installers();
    build_prune_plan(&state, &installers, |p| p.exists())
        .map_err(|e| CliError::Resolve(e.to_string()))
}

fn managed_from_resolution<'a>(
    env: &'a Env,
    manifest_path: &Path,
    resolution: &bosun_resolve::Resolution,
    sys: &StateDocument,
    usr: &StateDocument,
) -> Result<(Vec<Managed<'a>>, Vec<Diagnostic>), CliError> {
    let mut managed = Vec::new();
    let mut carried = Vec::new();
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
                let scope = installer.scope();
                let doc = scope_state(scope, sys, usr);
                managed.push(Managed {
                    name: entry.name.clone(),
                    purl: leaf.placement.purl.clone(),
                    backend: backend.clone(),
                    resolved: version.clone(),
                    vers_type: env.vers_type(backend).unwrap_or("generic").to_string(),
                    claims: claims_for(manifest_path, &entry.name, &pkg.meta.canonical(), doc),
                    current: current_of(doc, &entry.name),
                    installer,
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
    Ok((managed, carried))
}

fn managed_from_lock<'a>(
    env: &'a Env,
    manifest_path: &Path,
    lock: &LockDocument,
    sys: &StateDocument,
    usr: &StateDocument,
) -> Result<Vec<Managed<'a>>, CliError> {
    let mut managed = Vec::new();
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
        let scope = installer.scope();
        let doc = scope_state(scope, sys, usr);
        let claims = claims_for(manifest_path, &pkg.name, &pkg.meta.canonical(), doc);
        // §7 --frozen: refuse if another claim's spec excludes the locked version.
        frozen_select(&leaf.resolved, &claims, &vers_type)
            .map_err(|e| CliError::Resolve(e.to_string()))?;
        managed.push(Managed {
            name: pkg.name.clone(),
            purl: leaf.placement.purl.clone(),
            backend: backend.clone(),
            resolved: leaf.resolved.clone(),
            vers_type,
            claims,
            current: current_of(doc, &pkg.name),
            installer,
        });
    }
    Ok(managed)
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

fn scope_state<'a>(
    scope: Scope,
    sys: &'a StateDocument,
    usr: &'a StateDocument,
) -> &'a StateDocument {
    match scope {
        Scope::System => sys,
        Scope::User => usr,
    }
}

fn current_of(doc: &StateDocument, name: &str) -> Current {
    match doc
        .packages
        .iter()
        .find(|p| p.name == name)
        .and_then(|p| p.targets.values().next())
    {
        Some(leaf) => Current::Present {
            version: Some(leaf.installed.clone()),
            origin: leaf.origin,
        },
        None => Current::Absent,
    }
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

fn read_state(env: &Env, scope: Scope) -> Result<StateDocument, CliError> {
    env.state
        .read(scope)
        .map_err(|e| CliError::Environment(e.to_string()))
}

fn combined_state(env: &Env) -> Result<StateDocument, CliError> {
    let mut packages = read_state(env, Scope::System)?.packages;
    packages.extend(read_state(env, Scope::User)?.packages);
    Ok(StateDocument {
        state_version: 1,
        packages,
    })
}

fn lock_path(manifest_file: &Path) -> PathBuf {
    manifest_file
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("bosun.lock")
}

/// Resolve the manifest path for a claim without requiring the file to exist
/// (uninstall works after the repo is gone, §9).
fn claim_manifest_path(path: Option<PathBuf>) -> PathBuf {
    let base = path.unwrap_or_else(|| PathBuf::from("."));
    match base.extension().and_then(|e| e.to_str()) {
        Some("yaml") | Some("yml") => base,
        _ => base.join("bosun.yaml"),
    }
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
                };
                format!("{:<10} {:<16} {}", o.kind.label(), o.name, result)
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}
