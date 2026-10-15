//! `bosun add` and `bosun remove` (§4.2): edit one manifest entry, keeping the
//! rest of the file as written, then run `lock` on the edited manifest. Two
//! operations, so only `lock` ever writes `bosun.lock` (§4.4).

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use bosun_apply::{build_release_plan, ApplyReport, Plan};
use bosun_core::manifest::{Ext, InstallBlock, ManifestEntry};
use bosun_core::platform::Selector;
use bosun_core::{default_spec, BackendId, Manifest};
use bosun_io::manifest_edit::{add_entry, remove_entry};
use bosun_io::{check_ext, load_manifest, manifest_hash};
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
    /// Candidate backends, in preference order.
    pub backends: Vec<String>,
    /// The backend-native package id, where it differs from the name.
    pub package: Option<String>,
    /// The spec; when absent, one is derived from the newest version.
    pub spec: Option<String>,
    /// The selector the install block goes under; default this machine's OS.
    pub selector: Option<String>,
    /// `key=value` or `backend.key=value` settings for `ext`.
    pub ext: Vec<String>,
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
        None => env
            .host()
            .os
            .to_string()
            .parse()
            .expect("an OS is a selector"),
    };
    let block = InstallBlock {
        backends: opts.backends.clone(),
        package: opts.package.clone(),
        spec: None,
        ext: parse_ext(&opts.ext, &opts.backends)?,
    };
    let mut entry = ManifestEntry {
        name: opts.name.clone(),
        spec: opts.spec.clone(),
        formerly: Vec::new(),
        install: BTreeMap::from([(selector, block)]),
    };
    // The ext settings are checked before any lookup, so a typo costs nothing.
    let alone = with_only(&manifest, entry.clone());
    check_ext(&alone, env.ext_keys()).map_err(|e| CliError::Parse {
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
    check_ext(&reloaded, env.ext_keys()).map_err(|e| CliError::Parse {
        what: "manifest".to_string(),
        detail: format!("{}: {e}", file.display()),
    })?;
    write(&file, &edited)?;

    let lock = commands::lock(env, Some(file.clone()), false, pref)?;
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

/// `--ext` values as an `ext` map. `backend.key=value` names its backend;
/// a bare `key=value` belongs to the one backend given, and is an error when
/// there are none or several.
fn parse_ext(values: &[String], backends: &[String]) -> Result<Ext, CliError> {
    let mut ext = Ext::new();
    for raw in values {
        let (path, value) = raw
            .split_once('=')
            .ok_or_else(|| CliError::Usage(format!("--ext {raw}: expected key=value")))?;
        let (backend, key) = match path.split_once('.') {
            Some((backend, key)) => (backend.to_string(), key),
            None => match backends {
                [one] => (one.clone(), path),
                _ => {
                    return Err(CliError::Usage(format!(
                        "--ext {raw}: name the backend (backend.{path}=…) unless exactly one \
                         --backend is given"
                    )))
                }
            },
        };
        ext.entry(backend)
            .or_default()
            .insert(key.to_string(), value.to_string());
    }
    Ok(ext)
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
        let lock = commands::lock(env, Some(self.file.clone()), false, pref)?;
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

fn read(file: &Path) -> Result<String, CliError> {
    fs::read_to_string(file).map_err(|e| CliError::Io {
        path: file.to_path_buf(),
        detail: e.to_string(),
    })
}

fn write(file: &Path, text: &str) -> Result<(), CliError> {
    fs::write(file, text).map_err(|e| CliError::Io {
        path: file.to_path_buf(),
        detail: e.to_string(),
    })
}

fn io_error(e: bosun_io::IoError) -> CliError {
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

    #[test]
    fn a_bare_ext_key_belongs_to_the_one_backend() {
        let ext = parse_ext(&strings(&["asset=a.tar.gz"]), &strings(&["github-release"])).unwrap();
        assert_eq!(ext["github-release"]["asset"], "a.tar.gz");
    }

    #[test]
    fn a_qualified_ext_key_names_its_backend() {
        let ext = parse_ext(
            &strings(&["github-release.asset=x=y.tar.gz"]),
            &strings(&["cargo", "github-release"]),
        )
        .unwrap();
        assert_eq!(ext["github-release"]["asset"], "x=y.tar.gz");
    }

    #[test]
    fn a_bare_ext_key_with_several_or_no_backends_is_a_usage_error() {
        for backends in [strings(&["cargo", "apt"]), Vec::new()] {
            let err = parse_ext(&strings(&["asset=a"]), &backends).unwrap_err();
            assert!(matches!(err, CliError::Usage(_)), "{err:?}");
        }
    }

    #[test]
    fn an_ext_value_without_equals_is_a_usage_error() {
        let err = parse_ext(&strings(&["asset"]), &strings(&["cargo"])).unwrap_err();
        assert!(matches!(err, CliError::Usage(_)), "{err:?}");
    }
}
