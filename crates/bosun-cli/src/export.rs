//! `bosun export` (§4.2, §9): seed a manifest from what this machine has
//! installed by hand. Each present backend reports what the user installed
//! through it, by its own record (`apt-mark showmanual` less the base system,
//! `cargo install --list`; bosun's state for the GitHub release backends); each package
//! becomes an entry as `add` writes it, then the manifest is locked. Entries
//! the manifest already has stay as they are. It never installs.

use std::collections::BTreeMap;
use std::path::PathBuf;

use bosun_core::manifest::{InstallBlock, ManifestEntry, Options, PACKAGE_OPTION};
use bosun_core::record::StateDocument;
use bosun_core::{default_spec, BackendId, Manifest, Target, Version};
use bosun_io::manifest_edit::add_entry;
use bosun_io::{check_options, load_manifest, manifest_hash};
use bosun_ports::UserInstalls;
use bosun_resolve::{resolve, Outcome};
use serde::Serialize;
use tracing::info;

use crate::commands::{self, LockOutput};
use crate::edit::{default_selector, io_error, read, write};
use crate::env::Env;
use crate::error::CliError;
use crate::logmsg;
use crate::output::Render;

/// The manifest `export` starts from when there is none.
const SEED: &str = "version: 2\n";

/// Why a package was left out: the manifest names it already.
const IN_MANIFEST: &str = "already in the manifest";

/// Why a package was left out: an earlier backend's package has its name.
const EXPORTED_FROM: &str = "already exported from";

/// Why a package was left out: nothing the backend offers is in the range
/// from its installed version.
const UNSATISFIABLE: &str = "unsatisfiable";

/// Why a package was left out: its entry does not apply on this machine.
const NOT_APPLICABLE: &str = "does not apply on this machine";

/// One package a backend says the user installed, before it is an entry.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Found {
    backend: BackendId,
    /// bosun's name for it: the backend's id, or the name state records.
    name: String,
    /// The backend options its entry carries (`package`, github-release's
    /// `asset`, …); empty when the name is the package.
    options: Options,
    version: Version,
}

/// A package `export` adds.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Added {
    name: String,
    backend: String,
    spec: String,
}

/// A package `export` leaves out, and why.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct LeftOut {
    name: String,
    backend: String,
    reason: String,
}

/// What `bosun export` added (or, planned, would add), what it left out, the
/// backends that keep no record of what the user installed, and the lock.
#[derive(Serialize)]
pub struct ExportOutput {
    manifest: String,
    /// Whether this is only the plan: nothing was written.
    planned: bool,
    added: Vec<Added>,
    left_out: Vec<LeftOut>,
    /// Backends with no record of what the user installed: nothing exported.
    no_record: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    lock: Option<LockOutput>,
}

impl Render for ExportOutput {
    fn text(&self) -> String {
        let mut lines = Vec::new();
        if self.added.is_empty() {
            lines.push(format!("Nothing to export to {}", self.manifest));
        } else if self.planned {
            lines.push(format!("Would add to {}:", self.manifest));
        } else {
            let noun = if self.added.len() == 1 {
                "package"
            } else {
                "packages"
            };
            lines.push(format!(
                "Added {} {noun} to {}:",
                self.added.len(),
                self.manifest
            ));
        }
        for a in &self.added {
            lines.push(format!("  {:<16} {:<16} {}", a.name, a.backend, a.spec));
        }
        if !self.left_out.is_empty() {
            lines.push("Left out:".to_string());
            for l in &self.left_out {
                lines.push(format!("  {} ({}): {}", l.name, l.backend, l.reason));
            }
        }
        if !self.no_record.is_empty() {
            lines.push(format!(
                "No record of what was installed by hand: {}",
                self.no_record.join(", ")
            ));
        }
        if let Some(lock) = &self.lock {
            lines.push(lock.text());
        }
        lines.join("\n")
    }

    fn exit_code(&self) -> i32 {
        self.lock.as_ref().map_or(0, Render::exit_code)
    }
}

/// A `bosun export` worked out before anything is written: the manifest
/// text with the new entries, and what to report.
pub struct Export {
    file: PathBuf,
    edited: String,
    added: Vec<Added>,
    left_out: Vec<LeftOut>,
    no_record: Vec<String>,
}

impl Export {
    /// Whether there is nothing to add.
    pub fn is_empty(&self) -> bool {
        self.added.is_empty()
    }

    /// The output before (or instead of) writing: nothing changed.
    pub fn planned(&self) -> ExportOutput {
        ExportOutput {
            manifest: self.file.display().to_string(),
            planned: true,
            added: self.added.clone(),
            left_out: self.left_out.clone(),
            no_record: self.no_record.clone(),
            lock: None,
        }
    }

    /// Write the manifest, then lock it.
    pub fn finish(self, env: &Env, pref: &[BackendId]) -> Result<ExportOutput, CliError> {
        write(&self.file, &self.edited)?;
        let lock = commands::lock(env, Some(self.file.clone()), pref)?;
        Ok(ExportOutput {
            planned: false,
            lock: Some(lock),
            ..self.planned()
        })
    }
}

/// The manifest file `export` writes: `path` when it names a file, else
/// `bosun.yaml` in the directory (default: the current one). It need not
/// exist yet.
fn export_file(path: Option<PathBuf>) -> PathBuf {
    let base = path.unwrap_or_else(|| PathBuf::from("."));
    if base.is_dir() {
        base.join("bosun.yaml")
    } else {
        base
    }
}

/// Work out `bosun export [path]`: what each present backend says the user
/// installed, as entries for the manifest at `path`, each checked to resolve
/// on this machine. Changes nothing.
pub fn prepare(env: &Env, path: Option<PathBuf>, pref: &[BackendId]) -> Result<Export, CliError> {
    let file = export_file(path);
    let (text, manifest) = if file.exists() {
        (read(&file)?, commands::load(env, &file)?)
    } else {
        let manifest = load_manifest(SEED, &file).map_err(io_error)?;
        (SEED.to_string(), manifest)
    };

    let (found, no_record) = found(env)?;
    let (candidates, mut left_out) = sift(&manifest, found);
    let entries: Vec<ManifestEntry> = candidates
        .iter()
        .map(|f| entry(f, env_selector(env, &f.backend)))
        .collect();

    // Resolve the new entries together, on this machine, so what is written
    // locks: one the backend no longer offers a version of is left out.
    let probe = Manifest {
        packages: entries.clone(),
        ..manifest.clone()
    };
    let resolution = resolve(
        &probe,
        &manifest_hash(&probe),
        &env.host(),
        &env.resolvables(),
        pref,
    )
    .map_err(|e| CliError::Resolve(e.to_string()))?;

    let mut edited = text;
    let mut added = Vec::new();
    for ((f, e), outcome) in candidates.iter().zip(&entries).zip(resolution.outcomes) {
        let spec = e.spec.clone().unwrap_or_default();
        let reason = match outcome.outcome {
            Outcome::Resolved { .. } => None,
            Outcome::Unsatisfiable { .. } => Some(format!("{UNSATISFIABLE} {spec}")),
            Outcome::NotApplicable(_) => Some(NOT_APPLICABLE.to_string()),
        };
        if let Some(reason) = reason {
            left_out.push(left(f, reason));
            continue;
        }
        edited = add_entry(&edited, &file, e).map_err(io_error)?;
        added.push(Added {
            name: f.name.clone(),
            backend: f.backend.as_str().to_string(),
            spec,
        });
    }
    let reloaded = load_manifest(&edited, &file).map_err(io_error)?;
    check_options(&reloaded, env.option_keys()).map_err(|e| CliError::Parse {
        what: "manifest".to_string(),
        detail: format!("{}: {e}", file.display()),
    })?;
    Ok(Export {
        file,
        edited,
        added,
        left_out,
        no_record,
    })
}

/// The selector `add` would write `backend` under.
fn env_selector(env: &Env, backend: &BackendId) -> bosun_core::platform::Selector {
    default_selector(env, &[backend.as_str().to_string()])
}

/// What every present backend says the user installed, in registry order,
/// and the backends that keep no such record.
fn found(env: &Env) -> Result<(Vec<Found>, Vec<String>), CliError> {
    let mut found = Vec::new();
    let mut no_record = Vec::new();
    let mut state = None;
    for b in &env.backends {
        let installs = b
            .installer
            .user_installs()
            .map_err(|e| CliError::Resolve(format!("{}: {e}", b.id.as_str())))?;
        match installs {
            UserInstalls::Unsupported => {
                info!(message = logmsg::EXPORT_NO_RECORD, backend = b.id.as_str());
                no_record.push(b.id.as_str().to_string());
            }
            UserInstalls::Listed(list) => found.extend(list.into_iter().map(|u| Found {
                backend: b.id.clone(),
                name: u.package,
                options: Options::new(),
                version: u.version,
            })),
            UserInstalls::BosunState => {
                if state.is_none() {
                    state = Some(commands::combined_state(env)?);
                }
                let keys = env.option_keys().get(b.id.as_str());
                found.extend(recorded(
                    state.as_ref().expect("state was just read"),
                    &env.target(),
                    &b.id,
                    keys.map_or(&[][..], Vec::as_slice),
                ));
            }
        }
    }
    Ok((found, no_record))
}

/// What bosun's state records as installed through `backend` on `target`:
/// each under its recorded name, with the package and the backend options
/// (`keys`) its identity carries.
fn recorded(
    state: &StateDocument,
    target: &Target,
    backend: &BackendId,
    keys: &[String],
) -> Vec<Found> {
    state
        .packages
        .iter()
        .filter_map(|p| {
            let leaf = p.targets.get(target)?;
            if &leaf.placement.backend != backend {
                return None;
            }
            let purl = &leaf.placement.purl;
            let mut options: Options = purl
                .qualifiers()
                .iter()
                .filter(|(k, _)| keys.contains(k))
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            let package = purl.native_id();
            if package != p.name {
                options.insert(PACKAGE_OPTION.to_string(), package);
            }
            Some(Found {
                backend: backend.clone(),
                name: p.name.clone(),
                options,
                version: leaf.installed.clone(),
            })
        })
        .collect()
}

/// `found` split into what to add and what to leave out: a name the
/// manifest has, or one an earlier backend's package already took (the
/// manifest keeps one entry per name).
fn sift(manifest: &Manifest, found: Vec<Found>) -> (Vec<Found>, Vec<LeftOut>) {
    let mut taken: BTreeMap<String, String> = BTreeMap::new();
    let mut keep = Vec::new();
    let mut left_out = Vec::new();
    for f in found {
        if manifest.packages.iter().any(|e| e.name == f.name) {
            left_out.push(left(&f, IN_MANIFEST.to_string()));
        } else if let Some(first) = taken.get(&f.name) {
            left_out.push(left(&f, format!("{EXPORTED_FROM} {first}")));
        } else {
            taken.insert(f.name.clone(), f.backend.as_str().to_string());
            keep.push(f);
        }
    }
    (keep, left_out)
}

fn left(f: &Found, reason: String) -> LeftOut {
    LeftOut {
        name: f.name.clone(),
        backend: f.backend.as_str().to_string(),
        reason,
    }
}

/// The entry for `f`, as `add` writes one: the default spec for its
/// installed version (an OS package's `*`, a language package's range up to
/// the next breaking version), under `selector`.
fn entry(f: &Found, selector: bosun_core::platform::Selector) -> ManifestEntry {
    ManifestEntry {
        name: f.name.clone(),
        spec: Some(default_spec(&f.version)),
        formerly: Vec::new(),
        var: None,
        install: BTreeMap::from([(
            selector,
            InstallBlock {
                backends: BTreeMap::from([(f.backend.as_str().to_string(), f.options.clone())]),
                ..InstallBlock::default()
            },
        )]),
    }
}

#[cfg(test)]
mod tests {
    use bosun_core::{Origin, Os, Placement, Scheme, StateLeaf, StatePackage};

    use super::*;

    fn linux() -> Target {
        "linux/amd64".parse().unwrap()
    }

    fn sem(v: &str) -> Version {
        Version::parse(v, Scheme::Semver).unwrap()
    }

    fn recorded_package(name: &str, backend: &str, purl: &str, target: Target) -> StatePackage {
        StatePackage {
            name: name.to_string(),
            meta: (),
            targets: BTreeMap::from([(
                target,
                StateLeaf {
                    placement: Placement {
                        backend: BackendId::new(backend),
                        purl: purl.parse().unwrap(),
                    },
                    installed: sem("0.8.1"),
                    origin: Origin::Installed,
                    claimed_by: Vec::new(),
                },
            )]),
        }
    }

    fn state(packages: Vec<StatePackage>) -> StateDocument {
        StateDocument {
            state_version: 1,
            packages,
        }
    }

    fn keys() -> Vec<String> {
        vec!["asset".to_string(), "tag".to_string()]
    }

    fn options(pairs: &[(&str, &str)]) -> Options {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    fn found(backend: &str, name: &str) -> Found {
        Found {
            backend: BackendId::new(backend),
            name: name.to_string(),
            options: Options::new(),
            version: sem("1.0.0"),
        }
    }

    #[test]
    fn a_recorded_release_keeps_its_repo_and_the_options_it_was_installed_with() {
        let duf = recorded_package(
            "duf",
            "github-release",
            "pkg:github/muesli/duf?asset=duf.tar.gz&binary=duf&tag=v%7B%7B%20version%20%7D%7D",
            linux(),
        );
        let got = recorded(
            &state(vec![duf]),
            &linux(),
            &BackendId::new("github-release"),
            &keys(),
        );
        assert_eq!(
            got,
            vec![Found {
                backend: BackendId::new("github-release"),
                name: "duf".to_string(),
                options: options(&[
                    ("asset", "duf.tar.gz"),
                    ("package", "muesli/duf"),
                    ("tag", "v{{ version }}"),
                ]),
                version: sem("0.8.1"),
            }]
        );
    }

    #[test]
    fn a_recorded_package_named_as_its_id_needs_no_package_option() {
        let rg = recorded_package("ripgrep", "cargo", "pkg:cargo/ripgrep", linux());
        let got = recorded(&state(vec![rg]), &linux(), &BackendId::new("cargo"), &[]);
        assert_eq!(got.len(), 1);
        assert!(got[0].options.is_empty(), "{:?}", got[0].options);
    }

    #[test]
    fn only_this_backend_on_this_machine_is_taken_from_state() {
        let packages = vec![
            recorded_package("rg", "cargo", "pkg:cargo/ripgrep", linux()),
            recorded_package(
                "fd",
                "github-release",
                "pkg:github/sharkdp/fd",
                "windows/amd64".parse().unwrap(),
            ),
            recorded_package("duf", "github-release", "pkg:github/muesli/duf", linux()),
        ];
        let got = recorded(
            &state(packages),
            &linux(),
            &BackendId::new("github-release"),
            &keys(),
        );
        let names: Vec<&str> = got.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, ["duf"]);
    }

    #[test]
    fn sift_leaves_out_names_the_manifest_or_an_earlier_backend_has() {
        let manifest = load_manifest(
            "version: 2\npackages:\n  - name: jq\n    install: {backend: apt}\n",
            std::path::Path::new("bosun.yaml"),
        )
        .unwrap();
        let (keep, left_out) = sift(
            &manifest,
            vec![
                found("apt", "jq"),
                found("apt", "ripgrep"),
                found("cargo", "ripgrep"),
                found("cargo", "just"),
            ],
        );
        assert_eq!(keep, vec![found("apt", "ripgrep"), found("cargo", "just")]);
        assert_eq!(
            left_out,
            vec![
                LeftOut {
                    name: "jq".to_string(),
                    backend: "apt".to_string(),
                    reason: "already in the manifest".to_string(),
                },
                LeftOut {
                    name: "ripgrep".to_string(),
                    backend: "cargo".to_string(),
                    reason: "already exported from apt".to_string(),
                },
            ]
        );
    }

    #[test]
    fn an_entry_is_the_default_range_under_the_selector() {
        let mut f = found("github-release", "duf");
        f.version = sem("0.8.1");
        f.options = options(&[("package", "muesli/duf")]);
        let selector: bosun_core::Selector = Os::Linux.to_string().parse().unwrap();
        let e = entry(&f, selector.clone());
        assert_eq!(e.name, "duf");
        assert_eq!(e.spec.as_deref(), Some(">=0.8.1|<0.9.0"));
        assert_eq!(
            e.install[&selector].backends["github-release"],
            options(&[("package", "muesli/duf")])
        );
    }

    #[test]
    fn the_file_is_bosun_yaml_in_a_directory_else_the_path_given() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            export_file(Some(dir.path().to_path_buf())),
            dir.path().join("bosun.yaml")
        );
        let file = dir.path().join("tools.yaml");
        assert_eq!(export_file(Some(file.clone())), file);
        assert_eq!(export_file(None), PathBuf::from("./bosun.yaml"));
    }

    fn output(added: usize, planned: bool) -> ExportOutput {
        ExportOutput {
            manifest: "/p/bosun.yaml".to_string(),
            planned,
            added: (0..added)
                .map(|i| Added {
                    name: format!("tool{i}"),
                    backend: "cargo".to_string(),
                    spec: ">=1.0.0|<2.0.0".to_string(),
                })
                .collect(),
            left_out: vec![LeftOut {
                name: "jq".to_string(),
                backend: "apt".to_string(),
                reason: "already in the manifest".to_string(),
            }],
            no_record: vec!["winget".to_string(), "scoop".to_string()],
            lock: None,
        }
    }

    #[test]
    fn the_text_says_what_was_added_left_out_and_not_recorded() {
        let text = output(2, false).text();
        assert!(
            text.starts_with("Added 2 packages to /p/bosun.yaml:\n"),
            "{text}"
        );
        assert!(
            text.contains("  tool0            cargo            >=1.0.0|<2.0.0"),
            "{text}"
        );
        assert!(
            text.contains("Left out:\n  jq (apt): already in the manifest"),
            "{text}"
        );
        assert!(
            text.contains("No record of what was installed by hand: winget, scoop"),
            "{text}"
        );
    }

    #[test]
    fn the_text_counts_one_package_and_says_when_planned_or_empty() {
        assert!(output(1, false).text().starts_with("Added 1 package to"));
        assert!(output(1, true)
            .text()
            .starts_with("Would add to /p/bosun.yaml:"));
        let mut quiet = output(0, true);
        quiet.left_out.clear();
        quiet.no_record.clear();
        assert_eq!(quiet.text(), "Nothing to export to /p/bosun.yaml");
        assert_eq!(quiet.exit_code(), 0);
    }
}
