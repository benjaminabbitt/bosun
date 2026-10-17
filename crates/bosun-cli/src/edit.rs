//! `bosun add` and `bosun remove` (§4.2): edit one manifest entry, keeping the
//! rest of the file as written, then run `lock` on the edited manifest. Two
//! operations, so only `lock` ever writes `bosun.lock` (§4.4).

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use bosun_apply::{build_release_plan, ApplyReport, Plan};
use bosun_core::manifest::{Backends, InstallBlock, ManifestEntry, PACKAGE_OPTION};
use bosun_core::platform::Selector;
use bosun_core::{default_spec, BackendId, Manifest};
use bosun_io::manifest_edit::{add_entry, remove_entry};
use bosun_io::{check_options, load_manifest, manifest_hash};
use bosun_resolve::{resolve, Outcome};
use serde::Serialize;

use crate::commands::{self, LockOutput};
use crate::env::Env;
use crate::error::CliError;
use crate::output::Render;

/// What `bosun add` was asked to write.
pub struct AddOpts {
    /// bosun's logical name for the package.
    pub name: String,
    /// The acceptable backends (a set; preference is the machine's).
    pub backends: Vec<String>,
    /// The backend-native package id, where it differs from the name: for
    /// every `--backend` that doesn't name its own with `--opt`.
    pub package: Option<String>,
    /// The spec; when absent, one is derived from the newest version.
    pub spec: Option<String>,
    /// The selector the install block goes under; default `any` when every
    /// backend is platform-independent, else this machine's OS.
    pub selector: Option<String>,
    /// `key=value` or `backend.key=value` backend options.
    pub opts: Vec<String>,
    /// The manifest directory or file.
    pub manifest: Option<PathBuf>,
}

/// The `add` output: the entry written, then the lock that followed.
#[derive(Serialize)]
pub struct AddOutput {
    name: String,
    spec: String,
    manifest: String,
    lock: LockOutput,
}

impl Render for AddOutput {
    fn text(&self) -> String {
        format!(
            "Added {} {} to {}\n{}\nRun `bosun install` to install it.",
            self.name,
            self.spec,
            self.manifest,
            self.lock.text()
        )
    }

    fn exit_code(&self) -> i32 {
        self.lock.exit_code()
    }
}

/// Add an entry to the manifest, then lock (§4.2). The entry is checked first,
/// by loading the edited manifest and resolving the entry on this machine;
/// on any error nothing is written. `add` never installs.
pub fn add(env: &Env, opts: AddOpts, pref: &[BackendId]) -> Result<AddOutput, CliError> {
    let file = commands::manifest_file(opts.manifest.clone())?;
    let manifest = commands::load(env, &file)?;
    if manifest.packages.iter().any(|e| e.name == opts.name) {
        return Err(io_error(bosun_io::IoError::DuplicateEntry {
            path: file.clone(),
            name: opts.name.clone(),
        }));
    }

    let selector = match &opts.selector {
        Some(s) => s
            .parse::<Selector>()
            .map_err(|_| CliError::Usage(format!("--on {s}: not a selector")))?,
        None => default_selector(env, &opts.backends),
    };
    let block = install_block(&opts)?;
    let mut entry = ManifestEntry {
        name: opts.name.clone(),
        spec: opts.spec.clone(),
        formerly: Vec::new(),
        var: None,
        install: BTreeMap::from([(selector, block)]),
    };
    // The backend options are checked before any lookup, so a typo costs nothing.
    let alone = with_only(&manifest, entry.clone());
    check_options(&alone, env.option_keys()).map_err(|e| CliError::Parse {
        what: "manifest".to_string(),
        detail: format!("{}: {e}", file.display()),
    })?;

    // Resolve the entry alone on this machine: for the default spec, with
    // every version allowed; with a given spec, to refuse one nothing meets.
    let probe = match &opts.spec {
        Some(_) => entry.clone(),
        None => ManifestEntry {
            spec: Some("*".to_string()),
            ..entry.clone()
        },
    };
    let resolved = resolve_alone(env, &manifest, probe, pref)?;
    if opts.spec.is_none() {
        let latest = resolved.ok_or_else(|| {
            CliError::Usage(format!(
                "{} does not resolve on this machine, so there is no newest version to \
                 derive a spec from; pass --spec",
                opts.name
            ))
        })?;
        entry.spec = Some(default_spec(&latest));
    }

    let text = read(&file)?;
    let edited = add_entry(&text, &file, &entry).map_err(io_error)?;
    let reloaded = load_manifest(&edited, &file).map_err(io_error)?;
    check_options(&reloaded, env.option_keys()).map_err(|e| CliError::Parse {
        what: "manifest".to_string(),
        detail: format!("{}: {e}", file.display()),
    })?;
    write(&file, &edited)?;

    let lock = commands::lock(env, Some(file.clone()), pref)?;
    Ok(AddOutput {
        name: entry.name,
        spec: entry.spec.unwrap_or_default(),
        manifest: file.display().to_string(),
        lock,
    })
}

/// `manifest` with `entry` as its only package: same targets and defaults, so
/// the entry resolves and checks as it would in the file.
fn with_only(manifest: &Manifest, entry: ManifestEntry) -> Manifest {
    Manifest {
        packages: vec![entry],
        ..manifest.clone()
    }
}

/// Resolve `entry` alone on this machine: the version chosen, `None` when it
/// does not apply here, an error when nothing meets its spec.
fn resolve_alone(
    env: &Env,
    manifest: &Manifest,
    entry: ManifestEntry,
    pref: &[BackendId],
) -> Result<Option<bosun_core::Version>, CliError> {
    let name = entry.name.clone();
    let alone = with_only(manifest, entry);
    let resolution = resolve(
        &alone,
        &manifest_hash(&alone),
        &env.host(),
        &env.resolvables(),
        pref,
    )
    .map_err(|e| CliError::Resolve(e.to_string()))?;
    match resolution.outcomes.into_iter().next().map(|o| o.outcome) {
        Some(Outcome::Resolved { version, .. }) => Ok(Some(version)),
        Some(Outcome::Unsatisfiable { spec, available }) => Err(CliError::Resolve(format!(
            "{name}: unsatisfiable {spec} (available: {available})"
        ))),
        Some(Outcome::NotApplicable(_)) | None => Ok(None),
    }
}

/// The selector `add` writes under without `--on`: `any` when every backend
/// is a registered platform-independent one (npm, cargo, …), since nothing
/// ties it to this OS; else this machine's OS.
pub(crate) fn default_selector(env: &Env, backends: &[String]) -> Selector {
    let resolvables = env.resolvables();
    let independent = !backends.is_empty()
        && backends.iter().all(|b| {
            resolvables
                .iter()
                .any(|r| r.id.as_str() == b && r.platform_independent)
        });
    if independent {
        Selector::Any
    } else {
        env.host()
            .os
            .to_string()
            .parse()
            .expect("an OS is a selector")
    }
}

/// The install block `add` writes: each `--backend` with its `--opt`s, and
/// `--package` for those that don't name their own. With no `--backend`, the
/// package is for whatever backend the block inherits.
fn install_block(opts: &AddOpts) -> Result<InstallBlock, CliError> {
    let mut backends: Backends = opts
        .backends
        .iter()
        .map(|b| (b.clone(), Default::default()))
        .collect();
    for raw in &opts.opts {
        let (path, value) = raw
            .split_once('=')
            .ok_or_else(|| CliError::Usage(format!("--opt {raw}: expected key=value")))?;
        let (backend, key) = match path.split_once('.') {
            Some((backend, key)) => (backend.to_string(), key),
            None => match opts.backends.as_slice() {
                [one] => (one.clone(), path),
                _ => {
                    return Err(CliError::Usage(format!(
                        "--opt {raw}: name the backend (backend.{path}=…) unless exactly one \
                         --backend is given"
                    )))
                }
            },
        };
        let options = backends.get_mut(&backend).ok_or_else(|| {
            CliError::Usage(format!(
                "--opt {raw}: {backend} is not one of the --backend values"
            ))
        })?;
        options.insert(key.to_string(), value.to_string());
    }
    let mut package = opts.package.clone();
    if !backends.is_empty() {
        if let Some(p) = package.take() {
            for options in backends.values_mut() {
                options
                    .entry(PACKAGE_OPTION.to_string())
                    .or_insert_with(|| p.clone());
            }
        }
    }
    Ok(InstallBlock {
        backends,
        package,
        spec: None,
    })
}

// --- remove ------------------------------------------------------------------

/// A `bosun remove` worked out before anything changes: the edited manifest
/// text, and the plan releasing this manifest's claim on the package.
pub struct Removal {
    name: String,
    file: PathBuf,
    edited: String,
    /// The release plan (§9): drop the claim, uninstall if bosun alone wanted it.
    pub plan: Plan,
}

/// Work out `bosun remove <name>`: the manifest without the entry, and the plan
/// to release the package (§4.2). Changes nothing.
pub fn prepare_remove(
    env: &Env,
    name: &str,
    manifest: Option<PathBuf>,
) -> Result<Removal, CliError> {
    let file = commands::manifest_file(manifest)?;
    commands::load(env, &file)?;
    let text = read(&file)?;
    let edited = remove_entry(&text, &file, name).map_err(io_error)?;
    load_manifest(&edited, &file).map_err(io_error)?;

    let state = commands::combined_state(env)?;
    let claim = commands::canonical_manifest(&file);
    let plan = build_release_plan(&claim, name, &state, &env.installers())
        .map_err(|e| CliError::Resolve(e.to_string()))?;
    Ok(Removal {
        name: name.to_string(),
        file,
        edited,
        plan,
    })
}

/// The `remove` output: what was (or, with `--plan`, would be) released, the
/// manifest edit, and the lock that followed.
#[derive(Serialize)]
pub struct RemoveOutput {
    name: String,
    manifest: String,
    /// Whether this is only the plan (`--plan`): nothing was changed.
    planned: bool,
    plan: Plan,
    #[serde(skip_serializing_if = "Option::is_none")]
    report: Option<ApplyReport>,
    #[serde(skip_serializing_if = "Option::is_none")]
    lock: Option<LockOutput>,
}

impl Render for RemoveOutput {
    fn text(&self) -> String {
        let mut parts = Vec::new();
        if self.planned {
            parts.push(self.plan.text());
            parts.push(format!("Would remove {} from {}", self.name, self.manifest));
            return parts.join("\n");
        }
        if let Some(report) = &self.report {
            parts.push(report.text());
        }
        parts.push(format!("Removed {} from {}", self.name, self.manifest));
        if let Some(lock) = &self.lock {
            parts.push(lock.text());
        }
        parts.join("\n")
    }

    fn exit_code(&self) -> i32 {
        if self.planned {
            return 0;
        }
        self.lock.as_ref().map_or(0, Render::exit_code)
    }
}

impl Removal {
    /// The `--plan` output: nothing changed.
    pub fn planned(self) -> RemoveOutput {
        RemoveOutput {
            name: self.name,
            manifest: self.file.display().to_string(),
            planned: true,
            plan: self.plan,
            report: None,
            lock: None,
        }
    }

    /// After the release plan was applied (or had nothing to do): write the
    /// edited manifest, then lock it.
    pub fn finish(
        self,
        env: &Env,
        report: Option<ApplyReport>,
        pref: &[BackendId],
    ) -> Result<RemoveOutput, CliError> {
        write(&self.file, &self.edited)?;
        let lock = commands::lock(env, Some(self.file.clone()), pref)?;
        Ok(RemoveOutput {
            name: self.name,
            manifest: self.file.display().to_string(),
            planned: false,
            plan: self.plan,
            report,
            lock: Some(lock),
        })
    }
}

pub(crate) fn read(file: &Path) -> Result<String, CliError> {
    fs::read_to_string(file).map_err(|e| CliError::Io {
        path: file.to_path_buf(),
        detail: e.to_string(),
    })
}

pub(crate) fn write(file: &Path, text: &str) -> Result<(), CliError> {
    fs::write(file, text).map_err(|e| CliError::Io {
        path: file.to_path_buf(),
        detail: e.to_string(),
    })
}

pub(crate) fn io_error(e: bosun_io::IoError) -> CliError {
    CliError::Parse {
        what: "manifest".to_string(),
        detail: e.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    fn opts(backends: &[&str], package: Option<&str>, opts: &[&str]) -> AddOpts {
        AddOpts {
            name: "fd".to_string(),
            backends: strings(backends),
            package: package.map(str::to_string),
            spec: None,
            selector: None,
            opts: strings(opts),
            manifest: None,
        }
    }

    #[test]
    fn each_backend_is_acceptable_with_no_options_by_default() {
        let block = install_block(&opts(&["scoop", "winget"], None, &[])).unwrap();
        assert_eq!(block.backends.len(), 2);
        assert!(block.backends.values().all(|o| o.is_empty()));
    }

    #[test]
    fn a_bare_opt_belongs_to_the_one_backend() {
        let block = install_block(&opts(&["github-release"], None, &["asset=a.tar.gz"])).unwrap();
        assert_eq!(block.backends["github-release"]["asset"], "a.tar.gz");
    }

    #[test]
    fn a_qualified_opt_names_its_backend() {
        let block = install_block(&opts(
            &["cargo", "github-release"],
            None,
            &["github-release.asset=x=y.tar.gz"],
        ))
        .unwrap();
        assert_eq!(block.backends["github-release"]["asset"], "x=y.tar.gz");
        assert!(block.backends["cargo"].is_empty());
    }

    #[test]
    fn package_goes_to_every_backend_that_names_none_of_its_own() {
        let block = install_block(&opts(
            &["scoop", "winget"],
            Some("fd"),
            &["winget.package=sharkdp.fd"],
        ))
        .unwrap();
        assert_eq!(block.package_for("scoop"), Some("fd"));
        assert_eq!(block.package_for("winget"), Some("sharkdp.fd"));
        assert_eq!(block.package, None);
    }

    #[test]
    fn package_with_no_backend_is_for_the_inherited_one() {
        let block = install_block(&opts(&[], Some("sharkdp.fd"), &[])).unwrap();
        assert!(block.backends.is_empty());
        assert_eq!(block.package.as_deref(), Some("sharkdp.fd"));
    }

    #[test]
    fn an_opt_for_a_backend_not_given_is_a_usage_error() {
        let err = install_block(&opts(&["cargo"], None, &["apt.package=fd-find"])).unwrap_err();
        assert!(matches!(err, CliError::Usage(_)), "{err:?}");
    }

    #[test]
    fn a_bare_opt_with_several_or_no_backends_is_a_usage_error() {
        for backends in [&["cargo", "apt"][..], &[]] {
            let err = install_block(&opts(backends, None, &["asset=a"])).unwrap_err();
            assert!(matches!(err, CliError::Usage(_)), "{err:?}");
        }
    }

    #[test]
    fn an_opt_without_equals_is_a_usage_error() {
        let err = install_block(&opts(&["cargo"], None, &["asset"])).unwrap_err();
        assert!(matches!(err, CliError::Usage(_)), "{err:?}");
    }
}
