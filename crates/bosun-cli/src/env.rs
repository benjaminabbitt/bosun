//! Assembling the run environment: the machine, the state store, and the
//! backends (query + install sides) available this run — by probing the real
//! host (§5.3), or, when the test seam is active (`BOSUN_FIXTURE` +
//! `BOSUN_STATE_DIR`, both required), from a fixture ([`crate::fixture`]).

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;

use bosun_apply::{ApplyReport, Driver, HostApplier, Plan};
use bosun_backends::{
    AptFactory, CargoFactory, ChocoFactory, GithubInstallerFactory, GithubPackageFactory,
    GithubReleaseFactory, PackageKind, ScoopFactory, WingetFactory,
};
use bosun_core::vers::scheme_for_type;
use bosun_core::version::Scheme;
use bosun_core::{Arch, BackendId, Distro, Os, Scope, Target, Version};
#[cfg(unix)]
use bosun_host::LinuxMachine;
use bosun_host::{
    build_registry, detect_arch, FakeMachine, ProcessCommandRunner, SkipReason, StepExecutor,
    UreqHttpFetcher,
};
use bosun_io::FileStateStore;
use bosun_ports::backend::{Index, Installer};
use bosun_ports::{
    Backend, BackendFactory, CommandRunner, Elevation, HostInfo, HttpFetcher, Machine, PortError,
    Privilege, StepRunner, User,
};
use bosun_resolve::{Host, ResolvableBackend};

use crate::error::CliError;
use crate::fixture::{
    Fixture, FixtureIndex, FixtureInstaller, FixtureStepRunner, HandRecord, PackageDb,
    PackageDbRunner, RealSteps,
};

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
    /// The backend→scheme map (§6) for every compiled-in backend, present or
    /// not, so lock/state versions always read under the right scheme.
    schemes: bosun_io::BackendSchemes,
    /// The options (§5.4) every compiled-in backend declares, present or
    /// not, so a manifest checks the same on every machine.
    option_keys: bosun_io::BackendOptionKeys,
    /// Where this user's locks live (§4.5): beside the user machine state.
    lock_dir: PathBuf,
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

    /// The backend→scheme map (§6) for reading lockfile versions: a version's
    /// scheme comes from its backend's declared vers type, not the purl type,
    /// and covers every compiled-in backend whether or not it is present.
    pub fn schemes(&self) -> bosun_io::BackendSchemes {
        self.schemes.clone()
    }

    /// This machine's lock for the manifest at `manifest` (its canonical path).
    pub fn lock_path(&self, manifest: &Path) -> PathBuf {
        bosun_io::paths::lock_file(&self.lock_dir, manifest)
    }

    /// The options each known backend declares, for checking a manifest.
    pub fn option_keys(&self) -> &bosun_io::BackendOptionKeys {
        &self.option_keys
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

fn state_paths_at(dir: &str) -> (PathBuf, PathBuf) {
    let d = PathBuf::from(dir);
    (
        d.join("system").join("state.json"),
        d.join("user").join("state.json"),
    )
}

/// The backend→scheme map (§6) that lock/state reads use: a version's scheme
/// comes from its backend's declared vers type, never the purl type.
fn backend_schemes(backends: &[BackendEntry]) -> bosun_io::BackendSchemes {
    bosun_io::schemes_from(
        backends
            .iter()
            .map(|e| (e.id.as_str(), e.vers_type.as_str())),
    )
}

/// Where system-scope backends cache downloads (github-deb, github-rpm,
/// github-installer): `/var/cache/bosun`, beside the other package caches;
/// on Windows `%ProgramData%\bosun\cache`.
fn system_cache() -> PathBuf {
    if cfg!(windows) {
        let data = std::env::var_os("ProgramData").unwrap_or_else(|| r"C:\ProgramData".into());
        PathBuf::from(data).join("bosun").join("cache")
    } else {
        PathBuf::from("/var/cache/bosun")
    }
}

/// Every compiled-in backend factory, built (not probed). The schemes map is
/// taken from these so a backend absent this run is still known (§6).
fn compiled_in_factories(
    runner: Arc<dyn CommandRunner>,
    fetcher: Arc<dyn HttpFetcher>,
    bin_dir: PathBuf,
    os: Os,
    arch: Arch,
) -> Vec<Box<dyn BackendFactory>> {
    let package = |kind| {
        Box::new(GithubPackageFactory::new(
            kind,
            runner.clone(),
            fetcher.clone(),
            system_cache(),
            os,
            arch,
        )) as Box<dyn BackendFactory>
    };
    vec![
        Box::new(AptFactory::new(runner.clone())),
        Box::new(CargoFactory::new(runner.clone(), fetcher.clone())),
        Box::new(GithubReleaseFactory::new(
            fetcher.clone(),
            bin_dir,
            os,
            arch,
        )),
        package(PackageKind::Deb),
        package(PackageKind::Rpm),
        Box::new(WingetFactory::new(runner.clone())),
        Box::new(ScoopFactory::new(runner.clone())),
        Box::new(ChocoFactory::new(runner.clone())),
        Box::new(GithubInstallerFactory::new(
            runner.clone(),
            fetcher.clone(),
            system_cache(),
            os,
            arch,
        )),
    ]
}

/// The backend→scheme map (§6) from factory metadata alone, no probe — so it
/// covers every compiled-in backend, present or not.
fn factory_schemes(factories: &[Box<dyn BackendFactory>]) -> bosun_io::BackendSchemes {
    factories
        .iter()
        .map(|f| {
            (
                f.id().as_str().to_string(),
                scheme_for_type(f.meta().vers_type),
            )
        })
        .collect()
}

/// The options each compiled-in backend declares, from factory metadata
/// alone (no probe), like [`factory_schemes`].
fn factory_option_keys(factories: &[Box<dyn BackendFactory>]) -> bosun_io::BackendOptionKeys {
    factories
        .iter()
        .map(|f| {
            let keys = f.meta().option_keys.iter().map(|k| k.to_string()).collect();
            (f.id().as_str().to_string(), keys)
        })
        .collect()
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

/// The OS this build runs on: Windows, else Linux (macOS has no machine
/// adapter yet and runs as Linux).
fn host_os() -> Os {
    if cfg!(windows) {
        Os::Windows
    } else {
        Os::Linux
    }
}

/// The real machine for `os`: privilege, user and distro read once here.
#[cfg(unix)]
fn host_machine(
    arch: Arch,
    distro: Option<Distro>,
    system: PathBuf,
    user: PathBuf,
    _runner: &dyn CommandRunner,
) -> Box<dyn Machine> {
    Box::new(LinuxMachine::new(arch, distro, system, user))
}

/// The real machine for `os`: privilege, user and distro read once here.
#[cfg(windows)]
fn host_machine(
    arch: Arch,
    _distro: Option<Distro>,
    system: PathBuf,
    user: PathBuf,
    runner: &dyn CommandRunner,
) -> Box<dyn Machine> {
    let invoking = User {
        name: std::env::var("USERNAME").unwrap_or_default(),
        uid: None,
    };
    Box::new(bosun_host::WindowsMachine::new(
        arch,
        bosun_host::windows_privilege(runner),
        invoking,
        system,
        user,
    ))
}

fn from_host() -> Result<Env, CliError> {
    let os = host_os();
    let arch = detect_arch().map_err(env_err)?;
    let distro = match os {
        Os::Linux => bosun_host::os_release::detect(),
        _ => None,
    };
    let system = bosun_io::paths::state_path(Scope::System).map_err(env_err)?;
    let user = bosun_io::paths::state_path(Scope::User).map_err(env_err)?;

    let runner: Arc<dyn CommandRunner> = Arc::new(ProcessCommandRunner);
    let fetcher: Arc<dyn HttpFetcher> = Arc::new(UreqHttpFetcher::new());
    let bin_dir = user_bin_dir(&home_dir());
    let factories = compiled_in_factories(runner.clone(), fetcher.clone(), bin_dir, os, arch);
    // §6: the version scheme for a backend comes from its factory metadata and
    // must be known for every compiled-in backend — not only those present this
    // run — so state recorded for an absent backend still reads correctly.
    let schemes = factory_schemes(&factories);
    let option_keys = factory_option_keys(&factories);

    let info = HostInfo {
        os,
        arch,
        distro: distro.as_ref(),
        runner: runner.as_ref(),
    };
    let (registered, skipped) = build_registry(factories, &info).into_parts();

    let backends: Vec<BackendEntry> = registered
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

    let machine = host_machine(arch, distro, system.clone(), user.clone(), runner.as_ref());
    let lock_dir = bosun_io::paths::locks_beside(&user);
    Ok(Env {
        state: FileStateStore::new(system, user, schemes.clone()),
        backends,
        skipped,
        schemes,
        option_keys,
        lock_dir,
        machine,
        exec: Exec::Host { runner, fetcher },
    })
}

/// Where bosun installs release binaries for a user whose home is `home`.
fn user_bin_dir(home: &Path) -> PathBuf {
    home.join(".local").join("bin")
}

/// The user's home: `%USERPROFILE%` on Windows, `$HOME` elsewhere.
fn home_dir() -> PathBuf {
    let var = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    std::env::var_os(var).map(PathBuf::from).unwrap_or_default()
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

    let (system, user) = state_paths_at(state_dir);
    // Canned HTTP and a scratch bin dir for real backends (github-release).
    let mut canned = bosun_ports::testing::FakeHttpFetcher::new();
    for (url, file) in &fx.http {
        let body = fs::read(file).map_err(|e| CliError::Io {
            path: file.clone(),
            detail: e.to_string(),
        })?;
        canned = canned.with_body(url.clone(), body);
    }
    let fetcher: Arc<dyn HttpFetcher> = Arc::new(canned);
    let home = fx
        .home
        .clone()
        .unwrap_or_else(|| PathBuf::from(state_dir).join("home"));
    let bin_dir = user_bin_dir(&home);
    let any_real = fx.backends.iter().any(|b| b.real);
    let packages: PackageDb = Arc::default();
    let cache_dir = PathBuf::from(state_dir).join("cache");
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
            if b.real {
                let places = RealPlaces {
                    fetcher: &fetcher,
                    bin_dir: &bin_dir,
                    cache_dir: &cache_dir,
                    packages: &packages,
                };
                return real_backend(&b.id, version, &places, os, arch);
            }
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
            )
            .with_record(HandRecord {
                backend: b.id.clone(),
                names: b.by_hand,
                priorities: b.priorities,
            });
            Ok(BackendEntry {
                id: BackendId::new(b.id),
                version,
                purl_type: b.purl_type,
                vers_type: b.vers_type.clone(),
                platform_independent: b.platform_independent,
                index: Box::new(FixtureIndex::new(b.vers_type, b.available, b.checksums)),
                installer: Box::new(installer),
            })
        })
        .collect::<Result<Vec<_>, CliError>>()?;

    let schemes = backend_schemes(&backends);
    // Compiled-in backends keep their declared keys under the seam; a fake
    // backend the build doesn't have declares none.
    let fake_runner: Arc<dyn CommandRunner> =
        Arc::new(bosun_ports::testing::FakeCommandRunner::new());
    let mut option_keys = factory_option_keys(&compiled_in_factories(
        fake_runner,
        fetcher.clone(),
        bin_dir.clone(),
        os,
        arch,
    ));
    for b in &backends {
        option_keys.entry(b.id.as_str().to_string()).or_default();
    }
    let skipped = absent_backends(&fx.absent, fetcher.clone(), &bin_dir, os, arch)?;
    let store = FileStateStore::new(system.clone(), user.clone(), schemes.clone());
    let lock_dir = bosun_io::paths::locks_beside(&user);
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
        elevation: match os {
            Os::Windows => Elevation::AdministratorTerminal,
            _ => Elevation::Sudo,
        },
    };
    Ok(Env {
        state: store,
        backends,
        skipped,
        schemes,
        option_keys,
        lock_dir,
        machine: Box::new(machine),
        exec: Exec::Fixture(Box::new(FixtureStepRunner::new(
            fx.fail_step,
            machine_state,
            any_real.then(|| RealSteps { fetcher, packages }),
        ))),
    })
}

/// The compiled-in backends named `absent`, probed through the registry on a
/// machine where no program exists — the skips a real host without them
/// reports. One that registers anyway (it needs no program) is a fixture error.
fn absent_backends(
    absent: &[String],
    fetcher: Arc<dyn HttpFetcher>,
    bin_dir: &Path,
    os: Os,
    arch: Arch,
) -> Result<Vec<Skipped>, CliError> {
    let no_programs: Arc<dyn CommandRunner> =
        Arc::new(bosun_ports::testing::FakeCommandRunner::new());
    let factories: Vec<_> = compiled_in_factories(
        no_programs.clone(),
        fetcher,
        bin_dir.to_path_buf(),
        os,
        arch,
    )
    .into_iter()
    .filter(|f| absent.iter().any(|id| id == f.id().as_str()))
    .collect();
    if let Some(unknown) = absent
        .iter()
        .find(|id| !factories.iter().any(|f| f.id().as_str() == id.as_str()))
    {
        return Err(env_err(format!(
            "fixture backend {unknown} cannot be absent; bosun has no such backend"
        )));
    }
    let host = HostInfo {
        os,
        arch,
        distro: None,
        runner: no_programs.as_ref(),
    };
    let (registered, skipped) = build_registry(factories, &host).into_parts();
    if let Some(present) = registered.first() {
        return Err(env_err(format!(
            "fixture backend {} cannot be absent; it needs no program",
            present.id.as_str()
        )));
    }
    Ok(skipped
        .into_iter()
        .map(|s| Skipped {
            id: s.id.as_str().to_string(),
            reason: reason_text(&s.reason),
        })
        .collect())
}

/// Where a fixture's real backends fetch from and install to.
struct RealPlaces<'a> {
    fetcher: &'a Arc<dyn HttpFetcher>,
    bin_dir: &'a std::path::Path,
    cache_dir: &'a std::path::Path,
    packages: &'a PackageDb,
}

/// bosun's own compiled-in backend `id`, built over the fixture's canned HTTP:
/// github-release with a scratch bin dir, and github-deb / github-rpm with a
/// scratch cache and package managers answered from the fixture's package db.
fn real_backend(
    id: &str,
    version: Version,
    places: &RealPlaces,
    os: Os,
    arch: Arch,
) -> Result<BackendEntry, CliError> {
    let runner: Arc<dyn CommandRunner> = Arc::new(PackageDbRunner::new(places.packages.clone()));
    let package = |kind| -> Box<dyn BackendFactory> {
        Box::new(GithubPackageFactory::new(
            kind,
            runner.clone(),
            places.fetcher.clone(),
            places.cache_dir.to_path_buf(),
            os,
            arch,
        ))
    };
    let factory: Box<dyn BackendFactory> = match id {
        "github-release" => Box::new(GithubReleaseFactory::new(
            places.fetcher.clone(),
            places.bin_dir.to_path_buf(),
            os,
            arch,
        )),
        "github-deb" => package(PackageKind::Deb),
        "github-rpm" => package(PackageKind::Rpm),
        other => {
            return Err(env_err(format!(
                "fixture backend {other} cannot be real; only bosun's own release backends can"
            )))
        }
    };
    let host = HostInfo {
        os,
        arch,
        distro: None,
        runner: runner.as_ref(),
    };
    let probe = factory
        .probe(&host)
        .map_err(env_err)?
        .ok_or_else(|| env_err(format!("fixture backend {id} is not present")))?;
    let meta = factory.meta();
    let Backend { index, installer } = factory.build(probe);
    Ok(BackendEntry {
        id: factory.id(),
        version,
        purl_type: meta.purl_type.to_string(),
        vers_type: meta.vers_type.to_string(),
        platform_independent: meta.platform_independent,
        index,
        installer,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_env(dir: &std::path::Path, json: &str) -> Env {
        let fx = dir.join("fixture.json");
        std::fs::write(&fx, json).unwrap();
        from_fixture(fx.to_str().unwrap(), dir.join("state").to_str().unwrap()).unwrap()
    }

    #[test]
    fn vers_type_returns_the_named_backends_type_and_none_for_unknown() {
        let dir = tempfile::tempdir().unwrap();
        let env = fixture_env(
            dir.path(),
            r#"{"os":"linux","arch":"amd64","backends":[
                {"id":"apt","version":"2.0","purl_type":"deb","vers_type":"deb"},
                {"id":"cargo","version":"1.0","purl_type":"cargo","vers_type":"semver"}]}"#,
        );
        assert_eq!(env.vers_type(&BackendId::new("apt")), Some("deb"));
        assert_eq!(env.vers_type(&BackendId::new("cargo")), Some("semver"));
        assert_eq!(env.vers_type(&BackendId::new("nope")), None);
    }

    #[test]
    fn from_fixture_reads_the_privilege() {
        let dir = tempfile::tempdir().unwrap();
        let elevated = fixture_env(
            dir.path(),
            r#"{"os":"linux","arch":"amd64","privilege":"elevated","backends":[]}"#,
        );
        assert_eq!(elevated.machine.privilege(), Privilege::Elevated);

        let dir2 = tempfile::tempdir().unwrap();
        let plain = fixture_env(
            dir2.path(),
            r#"{"os":"linux","arch":"amd64","backends":[]}"#,
        );
        assert_eq!(plain.machine.privilege(), Privilege::Unelevated);
    }

    #[test]
    fn factory_schemes_cover_every_compiled_in_backend_without_probing() {
        // The scheme map is built from factory metadata, not from the probed set,
        // so an absent backend's state/lock entries still read under its scheme.
        let runner: Arc<dyn CommandRunner> = Arc::new(ProcessCommandRunner);
        let fetcher: Arc<dyn HttpFetcher> = Arc::new(UreqHttpFetcher::new());
        let factories = compiled_in_factories(
            runner,
            fetcher,
            PathBuf::from("/tmp/bin"),
            Os::Linux,
            Arch::Amd64,
        );
        let schemes = factory_schemes(&factories);
        assert_eq!(schemes.get("apt"), Some(&Scheme::Debian));
        assert_eq!(schemes.get("cargo"), Some(&Scheme::Semver));
        assert_eq!(schemes.get("github-release"), Some(&Scheme::Semver));
    }

    #[test]
    fn factory_option_keys_cover_every_compiled_in_backend_without_probing() {
        let runner: Arc<dyn CommandRunner> = Arc::new(ProcessCommandRunner);
        let fetcher: Arc<dyn HttpFetcher> = Arc::new(UreqHttpFetcher::new());
        let factories = compiled_in_factories(
            runner,
            fetcher,
            PathBuf::from("/tmp/bin"),
            Os::Linux,
            Arch::Amd64,
        );
        let keys = factory_option_keys(&factories);
        assert_eq!(
            keys.get("github-release"),
            Some(&vec!["asset".to_string(), "tag".to_string()])
        );
        assert_eq!(keys.get("apt"), Some(&vec![]));
        assert_eq!(keys.get("cargo"), Some(&vec![]));
    }

    #[test]
    fn a_fixture_env_knows_compiled_in_option_keys_and_its_own_backends() {
        // Fake fixture backends declare no options; compiled-in ones keep
        // theirs, so manifests check the same way under the test seam.
        let dir = tempfile::tempdir().unwrap();
        let env = fixture_env(
            dir.path(),
            r#"{"os":"linux","arch":"amd64","backends":[
                {"id":"brew","version":"4.0","purl_type":"brew","vers_type":"generic"}]}"#,
        );
        assert_eq!(
            env.option_keys().get("github-release"),
            Some(&vec!["asset".to_string(), "tag".to_string()])
        );
        assert_eq!(env.option_keys().get("brew"), Some(&vec![]));
    }

    fn fixture_error(dir: &std::path::Path, json: &str) -> String {
        let fx = dir.join("fixture.json");
        std::fs::write(&fx, json).unwrap();
        match from_fixture(fx.to_str().unwrap(), dir.join("state").to_str().unwrap()) {
            Ok(_) => panic!("want a fixture error for {json}"),
            Err(e) => e.to_string(),
        }
    }

    #[test]
    fn an_absent_fixture_backend_is_skipped_as_not_present() {
        let dir = tempfile::tempdir().unwrap();
        let env = fixture_env(
            dir.path(),
            r#"{"os":"linux","arch":"amd64","absent":["apt","cargo"],"backends":[]}"#,
        );
        let mut skipped: Vec<(&str, &str)> = env
            .skipped
            .iter()
            .map(|s| (s.id.as_str(), s.reason.as_str()))
            .collect();
        skipped.sort();
        assert_eq!(
            skipped,
            vec![("apt", "not present"), ("cargo", "not present")]
        );
    }

    #[test]
    fn a_fixture_backend_bosun_lacks_or_that_needs_no_program_cannot_be_absent() {
        let dir = tempfile::tempdir().unwrap();
        let unknown = fixture_error(
            dir.path(),
            r#"{"os":"linux","arch":"amd64","absent":["brew"],"backends":[]}"#,
        );
        assert!(unknown.contains("brew cannot be absent"), "{unknown}");
        let builtin = fixture_error(
            dir.path(),
            r#"{"os":"linux","arch":"amd64","absent":["github-release"],"backends":[]}"#,
        );
        assert!(
            builtin.contains("github-release cannot be absent"),
            "{builtin}"
        );
    }

    #[test]
    fn release_binaries_go_to_the_users_local_bin() {
        assert_eq!(
            user_bin_dir(Path::new("/home/ben")),
            Path::new("/home/ben").join(".local").join("bin")
        );
    }

    #[test]
    fn reason_text_names_each_skip_reason() {
        assert_eq!(reason_text(&SkipReason::NotPresent), "not present");
        assert_eq!(
            reason_text(&SkipReason::ProbeFailed("boom".into())),
            "probe failed: boom"
        );
        assert_eq!(reason_text(&SkipReason::Panicked), "probe panicked");
    }
}
