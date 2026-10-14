//! Assembling the run environment: host facts, state store, and the backends
//! available for resolution — built either by probing the real host (§5.3) or,
//! when `BOSUN_FIXTURE` is set, from a fixture (test-only, see [`crate::fixture`]).

use std::fs;
use std::path::PathBuf;
use std::sync::Arc;

use bosun_backends::{AptFactory, CargoFactory, GithubReleaseFactory};
use bosun_core::version::Scheme;
use bosun_core::{Arch, BackendId, Distro, Os, Version};
use bosun_host::{build_registry, detect_arch, ProcessCommandRunner, SkipReason, UreqHttpFetcher};
use bosun_io::FileStateStore;
use bosun_ports::backend::Index;
use bosun_ports::{Backend, BackendFactory, CommandRunner, HostInfo, HttpFetcher};
use bosun_resolve::{Host, ResolvableBackend};

use crate::error::CliError;
use crate::fixture::{Fixture, FixtureIndex};

/// One backend available for this run: the metadata the resolver needs plus its
/// query index (owned, so it outlives the resolve call).
pub struct BackendEntry {
    /// The backend id.
    pub id: BackendId,
    /// The backend's own version (for `bosun backends`).
    pub version: Version,
    /// The purl type for identity (§5.6).
    pub purl_type: String,
    /// The vers type for comparison (§6).
    pub vers_type: String,
    /// Whether it resolves under `any` (§4.7).
    pub platform_independent: bool,
    /// The query side.
    pub index: Box<dyn Index>,
}

/// A backend that did not register, for `bosun backends`.
pub struct Skipped {
    /// The backend id.
    pub id: String,
    /// Why it was skipped.
    pub reason: String,
}

/// Everything a command needs about the machine.
pub struct Env {
    /// The operating system.
    pub os: Os,
    /// The architecture.
    pub arch: Arch,
    /// Distro facts, when any.
    pub distro: Option<Distro>,
    /// The machine-state store (one file per scope).
    pub state: FileStateStore,
    /// The backends available this run.
    pub backends: Vec<BackendEntry>,
    /// Backends that did not register.
    pub skipped: Vec<Skipped>,
}

impl Env {
    /// The host facts resolution matches selectors and builds identities against.
    pub fn host(&self) -> Host<'_> {
        Host {
            os: self.os,
            arch: self.arch,
            distro: self.distro.as_ref(),
        }
    }

    /// The backends as [`ResolvableBackend`]s borrowing the owned indexes.
    pub fn resolvables(&self) -> Vec<ResolvableBackend<'_>> {
        self.backends
            .iter()
            .map(|e| ResolvableBackend {
                id: e.id.clone(),
                purl_type: e.purl_type.clone(),
                vers_type: e.vers_type.clone(),
                platform_independent: e.platform_independent,
                index: e.index.as_ref(),
            })
            .collect()
    }
}

/// Assemble the environment: from a fixture when `BOSUN_FIXTURE` is set, else by
/// probing the real host.
pub fn assemble() -> Result<Env, CliError> {
    match std::env::var("BOSUN_FIXTURE") {
        Ok(path) => from_fixture(&path),
        Err(_) => from_host(),
    }
}

/// The state store: a `BOSUN_STATE_DIR` override (tests) or the §4.6 defaults.
fn state_store() -> Result<FileStateStore, CliError> {
    match std::env::var("BOSUN_STATE_DIR") {
        Ok(dir) => {
            let d = PathBuf::from(dir);
            Ok(FileStateStore::new(
                d.join("system").join("state.json"),
                d.join("user").join("state.json"),
            ))
        }
        Err(_) => {
            FileStateStore::with_default_paths().map_err(|e| CliError::Environment(e.to_string()))
        }
    }
}

/// The purl type, vers type, and platform-independence for an M1 backend
/// (§5.6). The wiring the resolver's `ResolvableBackend` needs; a backend the
/// resolver cannot place is left out.
fn backend_meta(id: &str) -> Option<(&'static str, &'static str, bool)> {
    match id {
        "apt" => Some(("deb", "deb", false)),
        "cargo" => Some(("cargo", "cargo", true)),
        "github-release" => Some(("github", "semver", false)),
        _ => None,
    }
}

fn reason_text(reason: &SkipReason) -> String {
    match reason {
        SkipReason::NotPresent => "not present".to_string(),
        SkipReason::ProbeFailed(detail) => format!("probe failed: {detail}"),
        SkipReason::Panicked => "probe panicked".to_string(),
    }
}

fn from_host() -> Result<Env, CliError> {
    let os = Os::Linux; // M1 targets Linux; the Machine trait leaves room for more.
    let arch = detect_arch().map_err(|e| CliError::Environment(e.to_string()))?;
    let distro = bosun_host::os_release::detect();

    let runner: Arc<dyn CommandRunner> = Arc::new(ProcessCommandRunner);
    let fetcher: Arc<dyn HttpFetcher> = Arc::new(UreqHttpFetcher::new());
    let bin_dir = home_dir().join(".local").join("bin");
    let factories: Vec<Box<dyn BackendFactory>> = vec![
        Box::new(AptFactory::new(runner.clone())),
        Box::new(CargoFactory::new(runner.clone(), fetcher.clone())),
        Box::new(GithubReleaseFactory::new(
            fetcher.clone(),
            bin_dir,
            os,
            arch,
        )),
    ];
    let info = HostInfo {
        os,
        arch,
        distro: distro.as_ref(),
        runner: runner.as_ref(),
    };
    let (registered, skipped) = build_registry(factories, &info).into_parts();

    let backends = registered
        .into_iter()
        .filter_map(|rb| {
            let (purl_type, vers_type, platform_independent) = backend_meta(rb.id.as_str())?;
            let Backend {
                index,
                installer: _,
            } = rb.backend;
            Some(BackendEntry {
                id: rb.id,
                version: rb.version,
                purl_type: purl_type.to_string(),
                vers_type: vers_type.to_string(),
                platform_independent,
                index,
            })
        })
        .collect();
    let skipped = skipped
        .into_iter()
        .map(|s| Skipped {
            id: s.id.as_str().to_string(),
            reason: reason_text(&s.reason),
        })
        .collect();

    Ok(Env {
        os,
        arch,
        distro,
        state: state_store()?,
        backends,
        skipped,
    })
}

fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default()
}

fn from_fixture(path: &str) -> Result<Env, CliError> {
    let text = fs::read_to_string(path).map_err(|e| CliError::Io {
        path: PathBuf::from(path),
        detail: e.to_string(),
    })?;
    let fx: Fixture = serde_json::from_str(&text).map_err(|e| CliError::Parse {
        what: "fixture".to_string(),
        detail: e.to_string(),
    })?;
    let os = fx
        .os
        .parse::<Os>()
        .map_err(|e| CliError::Environment(format!("fixture os: {e}")))?;
    let arch = fx
        .arch
        .parse::<Arch>()
        .map_err(|e| CliError::Environment(format!("fixture arch: {e}")))?;
    let distro = fx.distro.map(|d| Distro {
        id: d.id,
        id_like: d.id_like,
        version_id: None,
        version_codename: d.codename,
    });

    let backends = fx
        .backends
        .into_iter()
        .map(|b| {
            let version =
                Version::parse(&b.version, Scheme::Generic).map_err(|e| CliError::Parse {
                    what: format!("fixture backend {} version", b.id),
                    detail: e.to_string(),
                })?;
            Ok(BackendEntry {
                id: BackendId::new(b.id),
                version,
                purl_type: b.purl_type,
                vers_type: b.vers_type.clone(),
                platform_independent: b.platform_independent,
                index: Box::new(FixtureIndex::new(b.vers_type, b.available)),
            })
        })
        .collect::<Result<Vec<_>, CliError>>()?;

    Ok(Env {
        os,
        arch,
        distro,
        state: state_store()?,
        backends,
        skipped: Vec::new(),
    })
}
