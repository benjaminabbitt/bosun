//! Assembling the run environment: the machine, the state store, and the
//! backends (query + install sides) available this run — by probing the real
//! host (§5.3), or, when the test seam is active (`BOSUN_FIXTURE` +
//! `BOSUN_STATE_DIR`, both required), from a fixture ([`crate::fixture`]).

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;

use bosun_apply::{ApplyReport, Driver, HostApplier, Plan};
use bosun_backends::{AptFactory, CargoFactory, GithubReleaseFactory};
use bosun_core::version::Scheme;
use bosun_core::{Arch, BackendId, Distro, Os, Scope, Target, Version};
use bosun_host::{
    build_registry, detect_arch, FakeMachine, LinuxMachine, ProcessCommandRunner, SkipReason,
    StepExecutor, UreqHttpFetcher,
};
use bosun_io::FileStateStore;
use bosun_ports::backend::{Index, Installer};
use bosun_ports::{
    Backend, BackendFactory, CommandRunner, Elevation, HostInfo, HttpFetcher, Machine, PortError,
    Privilege, StepRunner, User,
};
use bosun_resolve::{Host, ResolvableBackend};

use crate::error::CliError;
use crate::fixture::{Fixture, FixtureIndex, FixtureInstaller, FixtureStepRunner};

/// One backend available this run: the resolver metadata, the query index, and
/// the install side — all owned so they outlive a command.
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
    /// The mutation side.
    pub installer: Box<dyn Installer>,
}

/// A backend that did not register, for `bosun backends`.
pub struct Skipped {
    /// The backend id.
    pub id: String,
    /// Why it was skipped.
    pub reason: String,
}

/// How this run carries out steps: the real host executor, or the fixture's.
enum Exec {
    Host {
        runner: Arc<dyn CommandRunner>,
        fetcher: Arc<dyn HttpFetcher>,
    },
    Fixture(Box<dyn StepRunner>),
}

/// Everything a command needs about the machine.
pub struct Env {
    /// The machine-state store (one file per scope).
    pub state: FileStateStore,
    /// The backends available this run.
    pub backends: Vec<BackendEntry>,
    /// Backends that did not register.
    pub skipped: Vec<Skipped>,
    machine: Box<dyn Machine>,
    exec: Exec,
}

impl Env {
    /// The host facts resolution matches selectors and builds identities against.
    pub fn host(&self) -> Host<'_> {
        Host {
            os: self.machine.os(),
            arch: self.machine.arch(),
            distro: self.machine.distro(),
        }
    }

    /// The machine (privilege, invoking user, elevation, state paths).
    pub fn machine(&self) -> &dyn Machine {
        self.machine.as_ref()
    }

    /// This machine's single state target key (§4.6).
    pub fn target(&self) -> Target {
        Target::Platform {
            os: self.machine.os(),
            arch: self.machine.arch(),
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

    /// The installers by backend id (for the applier).
    pub fn installers(&self) -> BTreeMap<BackendId, &dyn Installer> {
        self.backends
            .iter()
            .map(|e| (e.id.clone(), e.installer.as_ref()))
            .collect()
    }

    /// The installer for a backend id, borrowing the owned entry.
    pub fn installer_for(&self, backend: &BackendId) -> Option<&dyn Installer> {
        self.backends
            .iter()
            .find(|e| &e.id == backend)
            .map(|e| e.installer.as_ref())
    }

    /// The query index for a backend id, borrowing the owned entry.
    pub fn index_for(&self, backend: &BackendId) -> Option<&dyn Index> {
        self.backends
            .iter()
            .find(|e| &e.id == backend)
            .map(|e| e.index.as_ref())
    }

    /// The vers type of a backend, for parsing claim specs (§5.6).
    pub fn vers_type(&self, backend: &BackendId) -> Option<&str> {
        self.backends
            .iter()
            .find(|e| &e.id == backend)
            .map(|e| e.vers_type.as_str())
    }

    /// Apply `plan` in `scope` on this machine (§4.8), inside one state
    /// transaction. Builds the host executor (or uses the fixture's) locally.
    pub fn apply(&self, scope: Scope, plan: &Plan) -> Result<ApplyReport, PortError> {
        let installers = self.installers();
        let target = self.target();
        let user = Some(self.machine.invoking_user().name);
        match &self.exec {
            Exec::Host { runner, fetcher } => {
                let exec = StepExecutor::new(runner.as_ref(), fetcher.as_ref());
                HostApplier::new(scope, target, user, &exec, &installers, &self.state).apply(plan)
            }
            Exec::Fixture(runner) => HostApplier::new(
                scope,
                target,
                user,
                runner.as_ref(),
                &installers,
                &self.state,
            )
            .apply(plan),
        }
    }
}

/// Assemble the environment. The test seam (`BOSUN_FIXTURE` + `BOSUN_STATE_DIR`)
/// requires **both** variables — one alone is an error, never a silent fall
/// back to real host state — and prints a one-line stderr warning while active.
/// With neither set, it probes the real host.
pub fn assemble() -> Result<Env, CliError> {
    let fixture = std::env::var("BOSUN_FIXTURE").ok();
    let state_dir = std::env::var("BOSUN_STATE_DIR").ok();
    match (fixture, state_dir) {
        (Some(fixture), Some(state_dir)) => {
            eprintln!(
                "bosun: test fixture active (BOSUN_FIXTURE={fixture}), not touching the real host"
            );
            from_fixture(&fixture, &state_dir)
        }
        (Some(_), None) | (None, Some(_)) => Err(CliError::Environment(
            "the test seam needs both BOSUN_FIXTURE and BOSUN_STATE_DIR; set both or neither"
                .to_string(),
        )),
        (None, None) => from_host(),
    }
}

fn state_store_at(dir: &str) -> (FileStateStore, PathBuf, PathBuf) {
    let d = PathBuf::from(dir);
    let system = d.join("system").join("state.json");
    let user = d.join("user").join("state.json");
    (
        FileStateStore::new(system.clone(), user.clone()),
        system,
        user,
    )
}

fn reason_text(reason: &SkipReason) -> String {
    match reason {
        SkipReason::NotPresent => "not present".to_string(),
        SkipReason::ProbeFailed(detail) => format!("probe failed: {detail}"),
        SkipReason::Panicked => "probe panicked".to_string(),
    }
}

fn env_err(e: impl std::fmt::Display) -> CliError {
    CliError::Environment(e.to_string())
}

fn from_host() -> Result<Env, CliError> {
    let os = Os::Linux; // M1 targets Linux; the Machine trait leaves room for more.
    let arch = detect_arch().map_err(env_err)?;
    let distro = bosun_host::os_release::detect();
    let system = bosun_io::paths::state_path(Scope::System).map_err(env_err)?;
    let user = bosun_io::paths::state_path(Scope::User).map_err(env_err)?;

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
        .map(|rb| {
            // Metadata comes from the backend's factory (§5.1).
            let Backend { index, installer } = rb.backend;
            BackendEntry {
                id: rb.id,
                version: rb.version,
                purl_type: rb.meta.purl_type.to_string(),
                vers_type: rb.meta.vers_type.to_string(),
                platform_independent: rb.meta.platform_independent,
                index,
                installer,
            }
        })
        .collect();
    let skipped = skipped
        .into_iter()
        .map(|s| Skipped {
            id: s.id.as_str().to_string(),
            reason: reason_text(&s.reason),
        })
        .collect();

    let machine = LinuxMachine::new(arch, distro, system.clone(), user.clone());
    Ok(Env {
        state: FileStateStore::new(system, user),
        backends,
        skipped,
        machine: Box::new(machine),
        exec: Exec::Host { runner, fetcher },
    })
}

fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default()
}

fn scope_of(raw: Option<&str>) -> Scope {
    match raw {
        Some("system") => Scope::System,
        _ => Scope::User,
    }
}

fn from_fixture(path: &str, state_dir: &str) -> Result<Env, CliError> {
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
        .map_err(|e| env_err(format!("fixture os: {e}")))?;
    let arch = fx
        .arch
        .parse::<Arch>()
        .map_err(|e| env_err(format!("fixture arch: {e}")))?;
    let distro = fx.distro.map(|d| Distro {
        id: d.id,
        id_like: d.id_like,
        version_id: None,
        version_codename: d.codename,
    });
    let privilege = match fx.privilege.as_deref() {
        Some("elevated") => Privilege::Elevated,
        _ => Privilege::Unelevated,
    };
    let user_name = fx.user.unwrap_or_else(|| "tester".to_string());

    let (store, system, user) = state_store_at(state_dir);
    // One in-memory machine shared by every installer and the step runner,
    // seeded with the packages each backend reports already present.
    let machine_state: crate::fixture::Machine = Rc::new(RefCell::new(BTreeMap::new()));
    let backends = fx
        .backends
        .into_iter()
        .map(|b| {
            let version =
                Version::parse(&b.version, Scheme::Generic).map_err(|e| CliError::Parse {
                    what: format!("fixture backend {} version", b.id),
                    detail: e.to_string(),
                })?;
            for (native, ver) in &b.installed {
                machine_state
                    .borrow_mut()
                    .insert(native.clone(), ver.clone());
            }
            let hold = if b.supports_hold {
                bosun_core::HoldSupport::Supported
            } else {
                bosun_core::HoldSupport::NotNeeded
            };
            let installer = FixtureInstaller::new(
                scope_of(b.scope.as_deref()),
                hold,
                b.supports_downgrade,
                b.vers_type.clone(),
                machine_state.clone(),
            );
            Ok(BackendEntry {
                id: BackendId::new(b.id),
                version,
                purl_type: b.purl_type,
                vers_type: b.vers_type.clone(),
                platform_independent: b.platform_independent,
                index: Box::new(FixtureIndex::new(b.vers_type, b.available)),
                installer: Box::new(installer),
            })
        })
        .collect::<Result<Vec<_>, CliError>>()?;

    let machine = FakeMachine {
        os,
        arch,
        distro,
        privilege,
        invoking_user: User {
            name: user_name,
            uid: None,
        },
        system_state: system,
        user_state: user,
        elevation: Elevation::Sudo,
    };
    Ok(Env {
        state: store,
        backends,
        skipped: Vec::new(),
        machine: Box::new(machine),
        exec: Exec::Fixture(Box::new(FixtureStepRunner::new(
            fx.fail_step,
            machine_state,
        ))),
    })
}
