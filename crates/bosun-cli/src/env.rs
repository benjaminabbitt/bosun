//! Assembling the run environment: the machine, the state store, and the
//! backends (query + install sides) available this run — by probing the real
//! host (§5.3), or, when the test seam is active (`BOSUN_FIXTURE` +
//! `BOSUN_STATE_DIR`, both required), from a fixture ([`crate::fixture`]).

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;

use bosun_apply::{ApplyReport, Driver, HostApplier, Plan};
use bosun_backends::apt::remote::TargetRoot;
use bosun_backends::github_release::BinFiles;
use bosun_backends::{
    AptFactory, CargoFactory, ChocoFactory, DnfFactory, GithubInstallerFactory,
    GithubPackageFactory, GithubReleaseFactory, NpmFactory, PackageKind, ScoopFactory, UvFactory,
    WingetFactory,
};
use bosun_core::record::StateDocument;
use bosun_core::vers::scheme_for_type;
use bosun_core::version::Scheme;
use bosun_core::{Arch, BackendId, Distro, Libc, LibcFamily, Os, Scope, Target, Version};
#[cfg(unix)]
use bosun_host::LinuxMachine;
use bosun_host::{
    build_registry, detect_arch, Account, FakeMachine, Invoker, ProcessCommandRunner, SkipReason,
    StepExecutor, UreqHttpFetcher,
};
use bosun_io::{DirectFiles, FileStateStore};
use bosun_ports::backend::{Index, Installer};
use bosun_ports::StateStore;
use bosun_ports::{
    Backend, BackendFactory, BackendMeta, CommandRunner, Elevation, FileAccess, HostInfo,
    HttpFetcher, Machine, PortError, Privilege, Probe, StepRunner, User,
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
    /// Where this run's locks live (§4.5).
    lock_dir: PathBuf,
    /// How the user's files (manifest, config, lock) are read and written:
    /// as this account, or as the user who elevated the run (§4.5).
    user_files: Rc<dyn FileAccess>,
    /// The user config (§5.3), when there is a home to find it in.
    config: Option<PathBuf>,
    machine: Box<dyn Machine>,
    exec: Exec,
    /// Whether this run found the system state unreadable and left it out.
    system_hidden: Cell<bool>,
}

impl Env {
    /// `scope`'s machine state, or `None` when this run leaves it out: a run
    /// without elevation that may not read the system state (root's, left by
    /// an elevated run) skips it and its packages, said once, and never
    /// touches it (§4.6). An elevated run, or the user scope, fails instead.
    pub fn visible_state(&self, scope: Scope) -> Result<Option<StateDocument>, CliError> {
        match self.state.read(scope) {
            Ok(doc) => Ok(Some(doc)),
            Err(PortError::StateDenied { path })
                if scope == Scope::System && self.machine.privilege() != Privilege::Elevated =>
            {
                if !self.system_hidden.replace(true) {
                    tracing::warn!(message = crate::logmsg::SYSTEM_STATE_HIDDEN, path = %path);
                }
                Ok(None)
            }
            Err(e) => Err(CliError::Environment(e.to_string())),
        }
    }

    /// Whether this run left the system state out (see [`Env::visible_state`]).
    pub fn system_hidden(&self) -> bool {
        self.system_hidden.get()
    }

    /// The host facts resolution matches selectors and builds identities against.
    pub fn host(&self) -> Host<'_> {
        Host {
            os: self.machine.os(),
            arch: self.machine.arch(),
            distro: self.machine.distro(),
            libc: self.machine.libc(),
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

    /// How this run reads and writes the user's files, the manifest and its
    /// canonical path, and the lock: as the user who elevated it, whose files
    /// they are (§4.5), else as this account.
    pub fn user_files(&self) -> Rc<dyn FileAccess> {
        self.user_files.clone()
    }

    /// The user's own files this run reads: through whom, and where the user
    /// config is (§5.3).
    pub fn user_side(&self) -> UserSide {
        UserSide {
            files: self.user_files.clone(),
            config: self.config.clone(),
        }
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

/// The user's own files a run reads, without the rest of the environment:
/// through whom (§4.5), and where the user config is (§5.3).
#[derive(Clone)]
pub struct UserSide {
    /// As the user who elevated the run, else as this account.
    pub files: Rc<dyn FileAccess>,
    /// The user config, when there is a home to find it in.
    pub config: Option<PathBuf>,
}

/// The user-scope state file (§4.6) a run reads and writes, its locks beside
/// it (§4.5), the user config (§5.3), and whose they are: the user who
/// elevated the run, as whom they are read and written, or this account.
#[derive(Debug, PartialEq, Eq)]
struct UserPlace {
    state: PathBuf,
    config: Option<PathBuf>,
    invoker: Option<Invoker>,
}

/// This account's own user state and config, where it has them.
struct Own {
    state: PathBuf,
    config: Option<PathBuf>,
}

impl UserPlace {
    /// Where the locks are: beside the user state.
    fn lock_dir(&self) -> PathBuf {
        bosun_io::paths::locks_beside(&self.state)
    }

    /// How the user state and the lock are read and written: as their user
    /// when the run was elevated by one, else as this account.
    fn files(&self) -> Rc<dyn FileAccess> {
        files_as(self.invoker.as_ref())
    }

    /// The user's files: through whom, and the config.
    fn side(&self) -> UserSide {
        UserSide {
            files: self.files(),
            config: self.config.clone(),
        }
    }

    /// The human the run is for (§5.5): the user who elevated it, if any.
    fn invoking_user(&self) -> Option<User> {
        self.invoker.as_ref().map(|u| User {
            name: u.name.clone(),
            uid: Some(u.uid),
        })
    }
}

/// File access as `user`, or as this account when there is none (§4.5).
#[cfg(unix)]
fn files_as(user: Option<&Invoker>) -> Rc<dyn FileAccess> {
    match user {
        Some(user) => Rc::new(bosun_host::AsUser::invoker(user)),
        None => Rc::new(DirectFiles),
    }
}

/// Windows elevation keeps the user's own account, so its own files.
#[cfg(not(unix))]
fn files_as(_user: Option<&Invoker>) -> Rc<dyn FileAccess> {
    Rc::new(DirectFiles)
}

/// Whose user state, lock and config a run uses: `own`, this account's,
/// unless the run is elevated by a user the elevating tool names (`env`,
/// found by `lookup`; §5.5). Then they are that user's, in the default state
/// and config dirs of their home, read and written as them; their
/// `XDG_STATE_HOME` and `XDG_CONFIG_HOME`, if set, are each said once not to
/// be read. A user the passwd database lacks is an error. Elevation on
/// Windows keeps the user's own profile, so its own state and config.
fn user_place(
    os: Os,
    privilege: Privilege,
    own: Own,
    env: impl Fn(&str) -> Option<String>,
    lookup: impl Fn(&str) -> Option<Account>,
) -> Result<UserPlace, CliError> {
    let own = UserPlace {
        state: own.state,
        config: own.config,
        invoker: None,
    };
    if privilege != Privilege::Elevated || os == Os::Windows {
        return Ok(own);
    }
    match bosun_host::invoker(&env, lookup) {
        Ok(Some(user)) => {
            let state = bosun_io::paths::state_under_home(&user.home);
            let config = bosun_io::paths::config_under_home(&user.home);
            if let Some(xdg) = env("XDG_STATE_HOME") {
                tracing::warn!(
                    message = crate::logmsg::XDG_STATE_HOME_IGNORED,
                    xdg_state_home = %xdg,
                    state = %state.display()
                );
            }
            if let Some(xdg) = env("XDG_CONFIG_HOME") {
                tracing::warn!(
                    message = crate::logmsg::XDG_CONFIG_HOME_IGNORED,
                    xdg_config_home = %xdg,
                    config = %config.display()
                );
            }
            tracing::debug!(
                message = crate::logmsg::INVOKER_STATE,
                user = %user.name,
                state = %state.display()
            );
            tracing::debug!(
                message = crate::logmsg::INVOKER_CONFIG,
                user = %user.name,
                config = %config.display()
            );
            Ok(UserPlace {
                state,
                config: Some(config),
                invoker: Some(user),
            })
        }
        Ok(None) => Ok(own),
        Err(unknown) => Err(CliError::Environment(format!(
            "{}, {}, {}; {}",
            crate::errmsg::INVOKER_UNKNOWN,
            unknown.name,
            crate::errmsg::INVOKER_NOT_IN_PASSWD,
            crate::errmsg::INVOKER_UNKNOWN_REMEDY
        ))),
    }
}

/// Assemble the environment. The test seam (`BOSUN_FIXTURE` + `BOSUN_STATE_DIR`)
/// requires **both** variables — one alone is an error, never a silent fall
/// back to real host state — and prints a one-line stderr warning while active.
/// With neither set, it probes the real host.
pub fn assemble() -> Result<Env, CliError> {
    match seam()? {
        Some((fixture, state_dir)) => from_fixture(&fixture, &state_dir),
        None => from_host(),
    }
}

/// The user's own files this run reads (§4.5, §5.3), without probing the
/// rest of the host, for `render` of another machine: through the user who
/// elevated the run, else as this account, and where the user config is.
pub fn user_side() -> Result<UserSide, CliError> {
    let place = match seam()? {
        Some((fixture, state_dir)) => fixture_place(&read_fixture(&fixture)?, &state_dir)?,
        None => host_place()?,
    };
    Ok(place.side())
}

/// This account's user config, where it has a home to find it in: none is
/// no config to read.
fn own_config() -> Option<PathBuf> {
    bosun_io::paths::config_path().ok()
}

/// The test seam's fixture and state dir when it is active, said on stderr;
/// one variable without the other is an error.
fn seam() -> Result<Option<(String, String)>, CliError> {
    let fixture = std::env::var("BOSUN_FIXTURE").ok();
    let state_dir = std::env::var("BOSUN_STATE_DIR").ok();
    match (fixture, state_dir) {
        (Some(fixture), Some(state_dir)) => {
            eprintln!(
                "bosun: test fixture active (BOSUN_FIXTURE={fixture}), not touching the real host"
            );
            Ok(Some((fixture, state_dir)))
        }
        (Some(_), None) | (None, Some(_)) => Err(CliError::Environment(
            "the test seam needs both BOSUN_FIXTURE and BOSUN_STATE_DIR; set both or neither"
                .to_string(),
        )),
        (None, None) => Ok(None),
    }
}

/// This machine's OS and architecture, without probing anything else.
pub fn this_platform() -> Result<(Os, Arch), CliError> {
    match seam()? {
        Some((fixture, _)) => {
            let fx = read_fixture(&fixture)?;
            fixture_platform(&fx)
        }
        None => Ok((
            host_os(),
            detect_arch(&ProcessCommandRunner).map_err(env_err)?,
        )),
    }
}

/// One backend's query side for a machine bosun isn't running on (§4.7).
pub struct RemoteBackend {
    /// The backend id.
    pub id: BackendId,
    /// The purl type for identity (§5.6).
    pub purl_type: String,
    /// The vers type for comparison (§6).
    pub vers_type: String,
    /// Whether it resolves under `any` (§4.7).
    pub platform_independent: bool,
    /// The remote index.
    pub index: Box<dyn Index>,
}

/// What resolving for a machine bosun isn't running on has (§4.7, §8.3): each
/// compiled-in backend's remote index, where it has one, built for that
/// machine's platform. Nothing here reads or changes this machine's package
/// state: the only program run is apt's, in a scratch root of its own under
/// bosun's cache (§4.7).
pub struct RemoteEnv {
    /// The backends with a remote index, in registration order.
    pub backends: Vec<RemoteBackend>,
    option_keys: bosun_io::BackendOptionKeys,
}

impl RemoteEnv {
    /// The backends as [`ResolvableBackend`]s borrowing the owned indexes.
    pub fn resolvables(&self) -> Vec<ResolvableBackend<'_>> {
        self.backends
            .iter()
            .map(|b| ResolvableBackend {
                id: b.id.clone(),
                purl_type: b.purl_type.clone(),
                vers_type: b.vers_type.clone(),
                platform_independent: b.platform_independent,
                index: b.index.as_ref(),
            })
            .collect()
    }

    /// The options each known backend declares, for checking a manifest.
    pub fn option_keys(&self) -> &bosun_io::BackendOptionKeys {
        &self.option_keys
    }
}

/// A runner that runs nothing: the remote side never asks this machine.
struct NoPrograms;

impl CommandRunner for NoPrograms {
    fn run(&self, command: &bosun_ports::Command) -> Result<bosun_ports::Output, PortError> {
        Err(PortError::CommandNotFound {
            program: command.program.clone(),
        })
    }
}

/// The remote indexes for a machine of `os`, `arch` and `libc` (§4.7): over
/// HTTPS on a real host, over the fixture's canned HTTP under the test seam.
/// apt's reads the sources of `root`'s release from that root.
pub fn remote(
    os: Os,
    arch: Arch,
    libc: Option<LibcFamily>,
    root: Option<TargetRoot>,
) -> Result<RemoteEnv, CliError> {
    match seam()? {
        Some((fixture, state_dir)) => {
            remote_from_fixture(&fixture, &state_dir, os, arch, libc, root)
        }
        None => {
            let fetcher: Arc<dyn HttpFetcher> = Arc::new(UreqHttpFetcher::new());
            let factories = factories_for(
                Arc::new(NoPrograms),
                &UserTools::own(home_dir(), Arc::new(NoPrograms)),
                fetcher,
                (os, arch, libc),
                &host_cache()?,
                RemoteApt {
                    root,
                    local: Some(Arc::new(ProcessCommandRunner)),
                },
            );
            Ok(remote_env(&factories))
        }
    }
}

/// Each factory's remote index, where it has one.
fn remote_env(factories: &[Box<dyn BackendFactory>]) -> RemoteEnv {
    let backends = factories
        .iter()
        .filter_map(|f| {
            let meta = f.meta();
            f.remote_index().map(|index| RemoteBackend {
                id: f.id(),
                purl_type: meta.purl_type.to_string(),
                vers_type: meta.vers_type.to_string(),
                platform_independent: meta.platform_independent,
                index,
            })
        })
        .collect();
    RemoteEnv {
        backends,
        option_keys: factory_option_keys(factories),
    }
}

/// The remote indexes under the test seam: the compiled-in ones over the
/// fixture's canned HTTP, with each fake platform-independent backend's
/// index (a registry, not this machine's package manager) in place of the
/// compiled-in one of its id.
fn remote_from_fixture(
    path: &str,
    state_dir: &str,
    os: Os,
    arch: Arch,
    libc: Option<LibcFamily>,
    root: Option<TargetRoot>,
) -> Result<RemoteEnv, CliError> {
    let fx = read_fixture(path)?;
    let factories = factories_for(
        Arc::new(NoPrograms),
        &UserTools::own(PathBuf::from(state_dir).join("home"), Arc::new(NoPrograms)),
        canned_fetcher(&fx.http)?,
        (os, arch, libc),
        &PathBuf::from(state_dir).join("cache"),
        RemoteApt { root, local: None },
    );
    let mut env = remote_env(&factories);
    for b in fx
        .backends
        .into_iter()
        .filter(|b| b.platform_independent && !b.real)
    {
        env.option_keys.entry(b.id.clone()).or_default();
        let fake = RemoteBackend {
            id: BackendId::new(b.id),
            purl_type: b.purl_type,
            vers_type: b.vers_type.clone(),
            platform_independent: true,
            index: Box::new(FixtureIndex::new(b.vers_type, b.available, b.checksums)),
        };
        match env.backends.iter_mut().find(|r| r.id == fake.id) {
            Some(slot) => *slot = fake,
            None => env.backends.push(fake),
        }
    }
    Ok(env)
}

/// The fixture at `path`.
fn read_fixture(path: &str) -> Result<Fixture, CliError> {
    let text = fs::read_to_string(path).map_err(|e| CliError::Io {
        path: PathBuf::from(path),
        detail: e.to_string(),
    })?;
    serde_json::from_str(&text).map_err(|e| CliError::Parse {
        what: "fixture".to_string(),
        detail: e.to_string(),
    })
}

/// The fixture machine's OS and architecture.
fn fixture_platform(fx: &Fixture) -> Result<(Os, Arch), CliError> {
    let os = fx
        .os
        .parse::<Os>()
        .map_err(|e| env_err(format!("fixture os: {e}")))?;
    let arch = fx
        .arch
        .parse::<Arch>()
        .map_err(|e| env_err(format!("fixture arch: {e}")))?;
    Ok((os, arch))
}

/// The fixture's canned HTTP: each URL answers 200 with its file's bytes; any
/// other URL fails.
fn canned_fetcher(http: &BTreeMap<String, PathBuf>) -> Result<Arc<dyn HttpFetcher>, CliError> {
    let mut canned = bosun_ports::testing::FakeHttpFetcher::new();
    for (url, file) in http {
        let body = fs::read(file).map_err(|e| CliError::Io {
            path: file.clone(),
            detail: e.to_string(),
        })?;
        canned = canned.with_body(url.clone(), body);
    }
    Ok(Arc::new(canned))
}

/// bosun's cache directory on this host (§4.7).
fn host_cache() -> Result<PathBuf, CliError> {
    bosun_io::paths::cache_dir().map_err(env_err)
}

/// The seam's system state file under its state dir; the user state is under
/// the user's home, as on a real host.
fn system_state_at(dir: &str) -> PathBuf {
    PathBuf::from(dir).join("system").join("state.json")
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

/// Every compiled-in backend factory, built (not probed): system-scope ones
/// running their programs through `runner`, user-scope ones for `user`. The
/// schemes map is taken from these so a backend absent this run is still
/// known (§6). `cache` is where remote indexes keep what they fetch (§4.7).
fn compiled_in_factories(
    runner: Arc<dyn CommandRunner>,
    user: &UserTools,
    fetcher: Arc<dyn HttpFetcher>,
    (os, arch, libc): (Os, Arch, Option<LibcFamily>),
    cache: &Path,
) -> Vec<Box<dyn BackendFactory>> {
    factories_for(
        runner,
        user,
        fetcher,
        (os, arch, libc),
        cache,
        RemoteApt::default(),
    )
}

/// Whom a run's user-scope backends (cargo, npm, uv, github-release) are
/// for (§5.3): the home they look in and its cargo home, how their programs
/// run, to query and to probe, and how the release bin dir is looked at.
struct UserTools {
    home: PathBuf,
    cargo_home: PathBuf,
    runner: Arc<dyn CommandRunner>,
    /// As the bin dir's user, when not this process.
    bin_files: Option<BinFiles>,
    /// The `PATH` the user's programs run with, when not this process's.
    path_var: Option<String>,
}

impl UserTools {
    /// This account's own: its home and `CARGO_HOME`, its programs run by
    /// `runner`, its files its own.
    fn own(home: PathBuf, runner: Arc<dyn CommandRunner>) -> UserTools {
        UserTools {
            cargo_home: cargo_home(&home, std::env::var_os("CARGO_HOME")),
            home,
            runner,
            bin_files: None,
            path_var: None,
        }
    }

    /// The user's who elevated the run, as them (§5.3): their passwd home
    /// and its `~/.cargo` (root's `CARGO_HOME` is not theirs), programs run
    /// as them with their own environment ([`bosun_host::user_env`]), and
    /// their bin dir looked at as them.
    #[cfg(unix)]
    fn invoker(user: &Invoker) -> UserTools {
        let runner =
            bosun_host::AsUserRunner::invoker(user, bosun_io::paths::env_var("PATH").as_deref());
        UserTools {
            home: user.home.clone(),
            cargo_home: user.home.join(".cargo"),
            path_var: Some(runner.path_var()),
            runner: Arc::new(runner),
            bin_files: Some(Arc::new(bosun_host::AsUser::invoker(user))),
        }
    }

    /// For `place`'s user: the invoking user's under elevation, else this
    /// account's, whose programs `runner` runs.
    #[cfg(unix)]
    fn for_place(place: &UserPlace, runner: Arc<dyn CommandRunner>) -> UserTools {
        match &place.invoker {
            Some(user) => UserTools::invoker(user),
            None => UserTools::own(home_dir(), runner),
        }
    }

    /// Windows elevation keeps the user's own account, so its own tools.
    #[cfg(not(unix))]
    fn for_place(_place: &UserPlace, runner: Arc<dyn CommandRunner>) -> UserTools {
        UserTools::own(home_dir(), runner)
    }

    /// Where bosun installs release binaries for this user.
    fn bin_dir(&self) -> PathBuf {
        user_bin_dir(&self.home)
    }

    /// The release backend for this user: their bin dir, looked at as them.
    fn release(
        &self,
        fetcher: Arc<dyn HttpFetcher>,
        (os, arch, libc): (Os, Arch, Option<LibcFamily>),
    ) -> GithubReleaseFactory {
        let mut factory =
            GithubReleaseFactory::new(fetcher, self.bin_dir(), os, arch).with_libc(libc);
        if let Some(files) = &self.bin_files {
            factory = factory.with_files(files.clone());
        }
        if let Some(path_var) = &self.path_var {
            factory = factory.with_path_var(path_var.clone());
        }
        factory
    }

    /// `factory`, probed through this user's runner, as its queries run.
    fn probed(&self, factory: impl BackendFactory + 'static) -> Box<dyn BackendFactory> {
        Box::new(ProbedBy {
            factory: Box::new(factory),
            runner: self.runner.clone(),
        })
    }
}

/// A user-scope backend's factory whose probe runs its program through
/// `runner`, as its queries do (§5.3): under elevation, as the invoking
/// user, who has the tool root may not.
struct ProbedBy {
    factory: Box<dyn BackendFactory>,
    runner: Arc<dyn CommandRunner>,
}

impl BackendFactory for ProbedBy {
    fn id(&self) -> BackendId {
        self.factory.id()
    }

    fn meta(&self) -> BackendMeta {
        self.factory.meta()
    }

    fn probe(&self, host: &HostInfo) -> Result<Option<Probe>, PortError> {
        self.factory.probe(&HostInfo {
            os: host.os,
            arch: host.arch,
            distro: host.distro,
            runner: self.runner.as_ref(),
        })
    }

    fn build(&self, probe: Probe) -> Backend {
        self.factory.build(probe)
    }

    fn remote_index(&self) -> Option<Box<dyn Index>> {
        self.factory.remote_index()
    }
}

/// What apt's remote index is given beyond the platform (§4.7).
#[derive(Default)]
struct RemoteApt {
    /// The target's root filesystem, when bosun was given it.
    root: Option<TargetRoot>,
    /// Runs this machine's apt in a scratch root, when it is to be asked.
    local: Option<Arc<dyn CommandRunner>>,
}

/// [`compiled_in_factories`] for a machine of that OS, arch and libc, with
/// what apt's remote index is given of it.
fn factories_for(
    runner: Arc<dyn CommandRunner>,
    user: &UserTools,
    fetcher: Arc<dyn HttpFetcher>,
    (os, arch, libc): (Os, Arch, Option<LibcFamily>),
    cache: &Path,
    apt: RemoteApt,
) -> Vec<Box<dyn BackendFactory>> {
    let package = |kind| {
        Box::new(
            GithubPackageFactory::new(
                kind,
                runner.clone(),
                fetcher.clone(),
                system_cache(),
                os,
                arch,
            )
            .with_libc(libc),
        ) as Box<dyn BackendFactory>
    };
    let mut apt_factory = AptFactory::new(runner.clone())
        .with_remote_index(fetcher.clone(), cache.join("apt"))
        .with_target_root(apt.root);
    if let Some(local) = apt.local {
        apt_factory = apt_factory.with_local_apt(local);
    }
    vec![
        Box::new(apt_factory),
        Box::new(DnfFactory::new(runner.clone())),
        Box::new(bosun_backends::ApkFactory::new(runner.clone())),
        user.probed(
            CargoFactory::new(user.runner.clone(), fetcher.clone()).with_config(
                std::env::current_dir().unwrap_or_default(),
                user.cargo_home.clone(),
            ),
        ),
        user.probed(NpmFactory::new(user.runner.clone(), fetcher.clone(), os)),
        user.probed(UvFactory::new(user.runner.clone(), fetcher.clone())),
        Box::new(user.release(fetcher.clone(), (os, arch, libc))),
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

/// The real machine for `os`: privilege, user and distro read once here;
/// and whose user state it uses, `user` unless a user elevated the run
/// ([`user_place`]), when it is theirs and runs for them.
#[cfg(unix)]
fn host_machine(
    arch: Arch,
    distro: Option<Distro>,
    libc: Option<Libc>,
    system: PathBuf,
    place: &UserPlace,
    _runner: &dyn CommandRunner,
) -> Box<dyn Machine> {
    let machine = LinuxMachine::new(arch, distro, libc, system, place.state.clone());
    match place.invoking_user() {
        Some(invoker) => Box::new(machine.invoked_by(invoker, place.state.clone())),
        None => Box::new(machine),
    }
}

/// The real machine for `os`: privilege, user and distro read once here.
/// An elevated terminal runs as the same user, so the user state is theirs.
#[cfg(windows)]
fn host_machine(
    arch: Arch,
    _distro: Option<Distro>,
    _libc: Option<Libc>,
    system: PathBuf,
    place: &UserPlace,
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
        place.state.clone(),
    ))
}

/// Whose user state, lock and files this host's run uses ([`user_place`]),
/// from this process's privilege alone.
#[cfg(unix)]
fn host_place() -> Result<UserPlace, CliError> {
    user_place(
        Os::Linux,
        bosun_host::unix_privilege(),
        Own {
            state: bosun_io::paths::state_path(Scope::User).map_err(env_err)?,
            config: own_config(),
        },
        bosun_io::paths::env_var,
        bosun_host::passwd_account,
    )
}

/// Windows elevation keeps the user's own account, so its own files.
#[cfg(not(unix))]
fn host_place() -> Result<UserPlace, CliError> {
    Ok(UserPlace {
        state: bosun_io::paths::state_path(Scope::User).map_err(env_err)?,
        config: own_config(),
        invoker: None,
    })
}

fn from_host() -> Result<Env, CliError> {
    let os = host_os();
    let runner: Arc<dyn CommandRunner> = Arc::new(ProcessCommandRunner);
    let arch = detect_arch(runner.as_ref()).map_err(env_err)?;
    let system = bosun_io::paths::state_path(Scope::System).map_err(env_err)?;
    let place = host_place()?;

    let (distro, libc) = match os {
        Os::Linux => (
            bosun_host::os_release::detect(),
            bosun_host::libc::detect(runner.as_ref()),
        ),
        _ => (None, None),
    };
    let fetcher: Arc<dyn HttpFetcher> = Arc::new(UreqHttpFetcher::new());
    let factories = compiled_in_factories(
        runner.clone(),
        &UserTools::for_place(&place, runner.clone()),
        fetcher.clone(),
        (os, arch, libc.as_ref().map(|l| l.family)),
        &host_cache()?,
    );
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

    let machine = host_machine(arch, distro, libc, system.clone(), &place, runner.as_ref());
    let files = place.files();
    Ok(Env {
        state: FileStateStore::new(system, place.state.clone(), schemes.clone())
            .with_user_files(files.clone()),
        backends,
        skipped,
        schemes,
        option_keys,
        lock_dir: place.lock_dir(),
        user_files: files,
        config: place.config.clone(),
        machine,
        exec: Exec::Host { runner, fetcher },
        system_hidden: Cell::new(false),
    })
}

/// Where bosun installs release binaries for a user whose home is `home`.
fn user_bin_dir(home: &Path) -> PathBuf {
    home.join(".local").join("bin")
}

/// The user's home: `%USERPROFILE%` on Windows, `$HOME` elsewhere.
/// cargo's home, where its own config lives: `$CARGO_HOME` (`var`), else
/// `~/.cargo`.
fn cargo_home(home: &Path, var: Option<std::ffi::OsString>) -> PathBuf {
    var.filter(|v| !v.is_empty())
        .map_or_else(|| home.join(".cargo"), PathBuf::from)
}

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

/// The fixture machine's privilege.
fn fixture_privilege(fx: &Fixture) -> Privilege {
    match fx.privilege.as_deref() {
        Some("elevated") => Privilege::Elevated,
        _ => Privilege::Unelevated,
    }
}

/// The scratch home of the seam's user: the fixture's, else `home` in the
/// state dir.
fn fixture_home(fx: &Fixture, state_dir: &str) -> PathBuf {
    fx.home
        .clone()
        .unwrap_or_else(|| PathBuf::from(state_dir).join("home"))
}

/// The home of the account the seam's run is: the seam's user's, or, when
/// elevated, root's own, a home of its own in the state dir.
fn fixture_own_home(fx: &Fixture, state_dir: &str) -> PathBuf {
    match fixture_privilege(fx) {
        Privilege::Elevated => PathBuf::from(state_dir).join("root"),
        Privilege::Unelevated => fixture_home(fx, state_dir),
    }
}

/// The seam's user-scope tools for `place`'s user: under elevation the
/// invoking user's home and bin dir, looked at as them; else the home of
/// the account the run is. The seam runs no programs of theirs.
fn fixture_tools(place: &UserPlace, own_home: PathBuf) -> UserTools {
    let runner: Arc<dyn CommandRunner> = Arc::new(bosun_ports::testing::FakeCommandRunner::new());
    match &place.invoker {
        Some(user) => UserTools {
            home: user.home.clone(),
            cargo_home: user.home.join(".cargo"),
            runner,
            bin_files: bin_files_as(user),
            path_var: None,
        },
        None => UserTools::own(own_home, runner),
    }
}

/// The bin dir's files as `user` (§4.5).
#[cfg(unix)]
fn bin_files_as(user: &Invoker) -> Option<BinFiles> {
    Some(Arc::new(bosun_host::AsUser::invoker(user)))
}

/// Windows elevation keeps the user's own account, so its own files.
#[cfg(not(unix))]
fn bin_files_as(_user: &Invoker) -> Option<BinFiles> {
    None
}

/// Whose user state, lock, config and files the seam's run uses
/// ([`user_place`]). The seam's user keeps state under the default state dir
/// of their home; an elevated run's own is root's, a home of its own in the
/// seam. The config is where the environment says, as on a real host. Under
/// sudo they are the invoking user's, by the fixture's passwd, read and
/// written as them.
fn fixture_place(fx: &Fixture, state_dir: &str) -> Result<UserPlace, CliError> {
    let (os, _) = fixture_platform(fx)?;
    let privilege = fixture_privilege(fx);
    let own_home = fixture_own_home(fx, state_dir);
    user_place(
        os,
        privilege,
        Own {
            state: bosun_io::paths::state_under_home(&own_home),
            config: own_config(),
        },
        bosun_io::paths::env_var,
        |name| {
            fx.passwd.get(name).map(|a| Account {
                uid: a.uid,
                gid: a.gid,
                home: a.home.clone(),
            })
        },
    )
}

fn from_fixture(path: &str, state_dir: &str) -> Result<Env, CliError> {
    let fx = read_fixture(path)?;
    let (os, arch) = fixture_platform(&fx)?;
    let place = fixture_place(&fx, state_dir)?;
    let privilege = fixture_privilege(&fx);
    let home = fixture_home(&fx, state_dir);
    let own_home = fixture_own_home(&fx, state_dir);
    let distro = fx.distro.map(|d| Distro {
        id: d.id,
        id_like: d.id_like,
        version_id: d.version_id,
        version_codename: d.codename,
        base_codename: d.base_codename,
    });
    let libc = fx
        .libc
        .map(|l| -> Result<Libc, CliError> {
            Ok(Libc {
                family: l
                    .family
                    .parse()
                    .map_err(|e| env_err(format!("fixture libc: {e}")))?,
                version: l.version,
            })
        })
        .transpose()?;
    let family = libc.as_ref().map(|l| l.family);
    let user_name = fx.user.unwrap_or_else(|| "tester".to_string());

    let system = system_state_at(state_dir);
    // Canned HTTP and a scratch bin dir for real backends (github-release),
    // the user's under elevation, as on a real host.
    let fetcher = canned_fetcher(&fx.http)?;
    let tools = fixture_tools(&place, own_home);
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
                    user: &tools,
                    cache_dir: &cache_dir,
                    packages: &packages,
                };
                return real_backend(&b.id, version, &places, os, arch, family);
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
        fake_runner.clone(),
        &UserTools::own(home.clone(), fake_runner),
        fetcher.clone(),
        (os, arch, family),
        &cache_dir,
    ));
    for b in &backends {
        option_keys.entry(b.id.as_str().to_string()).or_default();
    }
    let skipped = absent_backends(&fx.absent, fetcher.clone(), &home, &cache_dir, os, arch)?;
    let files = place.files();
    let store = FileStateStore::new(system.clone(), place.state.clone(), schemes.clone())
        .with_user_files(files.clone());
    let machine = FakeMachine {
        os,
        arch,
        distro,
        libc,
        privilege,
        invoking_user: place.invoking_user().unwrap_or(User {
            name: user_name,
            uid: None,
        }),
        system_state: system,
        user_state: place.state.clone(),
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
        lock_dir: place.lock_dir(),
        user_files: files,
        config: place.config.clone(),
        machine: Box::new(machine),
        exec: Exec::Fixture(Box::new(FixtureStepRunner::new(
            fx.fail_step,
            machine_state,
            any_real.then(|| RealSteps { fetcher, packages }),
        ))),
        system_hidden: Cell::new(false),
    })
}

/// The compiled-in backends named `absent`, probed through the registry on a
/// machine where no program exists — the skips a real host without them
/// reports. One that registers anyway (it needs no program) is a fixture error.
fn absent_backends(
    absent: &[String],
    fetcher: Arc<dyn HttpFetcher>,
    home: &Path,
    cache: &Path,
    os: Os,
    arch: Arch,
) -> Result<Vec<Skipped>, CliError> {
    let no_programs: Arc<dyn CommandRunner> =
        Arc::new(bosun_ports::testing::FakeCommandRunner::new());
    let factories: Vec<_> = compiled_in_factories(
        no_programs.clone(),
        &UserTools::own(home.to_path_buf(), no_programs.clone()),
        fetcher,
        (os, arch, None),
        cache,
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
    /// Whose bin dir github-release installs to and looks in.
    user: &'a UserTools,
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
    libc: Option<LibcFamily>,
) -> Result<BackendEntry, CliError> {
    let runner: Arc<dyn CommandRunner> = Arc::new(PackageDbRunner::new(places.packages.clone()));
    let package = |kind| -> Box<dyn BackendFactory> {
        Box::new(
            GithubPackageFactory::new(
                kind,
                runner.clone(),
                places.fetcher.clone(),
                places.cache_dir.to_path_buf(),
                os,
                arch,
            )
            .with_libc(libc),
        )
    };
    let factory: Box<dyn BackendFactory> = match id {
        "github-release" => Box::new(
            places
                .user
                .release(places.fetcher.clone(), (os, arch, libc)),
        ),
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

    fn sudo_by(name: &'static str) -> impl Fn(&str) -> Option<String> {
        move |var| match var {
            "SUDO_USER" => Some(name.to_string()),
            "SUDO_UID" => Some("1000".to_string()),
            "SUDO_GID" => Some("1001".to_string()),
            _ => None,
        }
    }

    fn ben(name: &str) -> Option<Account> {
        (name == "ben").then(|| Account {
            uid: 1000,
            gid: 1001,
            home: PathBuf::from("/srv/ben"),
        })
    }

    /// The invoking user's files go through a child that is them: another
    /// uid, which this process may not become without root, is refused,
    /// where this account's own files are not.
    #[cfg(unix)]
    #[test]
    fn the_invoking_user_s_files_are_done_as_them() {
        use std::os::unix::fs::MetadataExt;
        let tmp = tempfile::tempdir().unwrap();
        let mine = std::fs::metadata(tmp.path()).unwrap();
        let (uid, gid) = (mine.uid(), mine.gid());
        if uid == 0 {
            eprintln!("skipped: root may become anyone");
            return;
        }
        let other = Invoker {
            name: "bosun-no-such-user".to_string(),
            uid: uid + 1,
            gid,
            home: tmp.path().to_path_buf(),
        };
        let err = files_as(Some(&other)).exists(tmp.path()).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::PermissionDenied);
        assert!(files_as(None).exists(tmp.path()).unwrap());
    }

    fn own() -> UserPlace {
        UserPlace {
            state: PathBuf::from("/root/.local/state/bosun/state.json"),
            config: Some(PathBuf::from("/root/.config/bosun/config.yaml")),
            invoker: None,
        }
    }

    fn own_files() -> Own {
        let place = own();
        Own {
            state: place.state,
            config: place.config,
        }
    }

    #[test]
    fn an_elevated_run_keeps_the_state_and_lock_of_the_user_who_elevated_it_as_them() {
        let place = user_place(
            Os::Linux,
            Privilege::Elevated,
            own_files(),
            sudo_by("ben"),
            ben,
        )
        .unwrap();
        assert_eq!(
            place,
            UserPlace {
                state: PathBuf::from("/srv/ben/.local/state/bosun/state.json"),
                config: Some(PathBuf::from("/srv/ben/.config/bosun/config.yaml")),
                invoker: Some(Invoker {
                    name: "ben".to_string(),
                    uid: 1000,
                    gid: 1001,
                    home: PathBuf::from("/srv/ben"),
                }),
            }
        );
        assert_eq!(
            place.lock_dir(),
            PathBuf::from("/srv/ben/.local/state/bosun/locks")
        );
        assert_eq!(
            place.invoking_user(),
            Some(User {
                name: "ben".to_string(),
                uid: Some(1000),
            })
        );
        assert_eq!(own().invoking_user(), None);
    }

    #[test]
    fn a_run_not_elevated_on_windows_or_named_by_no_tool_keeps_its_own_state() {
        let place = |os, privilege, env: &dyn Fn(&str) -> Option<String>| {
            user_place(os, privilege, own_files(), env, ben).unwrap()
        };
        assert_eq!(
            place(Os::Linux, Privilege::Unelevated, &sudo_by("ben")),
            own()
        );
        assert_eq!(
            place(Os::Windows, Privilege::Elevated, &sudo_by("ben")),
            own()
        );
        assert_eq!(
            place(Os::Windows, Privilege::Elevated, &sudo_by("ghost")),
            own()
        );
        assert_eq!(place(Os::Linux, Privilege::Elevated, &|_| None), own());
    }

    #[test]
    fn an_elevating_user_the_passwd_database_lacks_is_an_error_with_the_remedy() {
        let err = user_place(
            Os::Linux,
            Privilege::Elevated,
            own_files(),
            sudo_by("ghost"),
            ben,
        )
        .unwrap_err()
        .to_string();
        assert!(
            err.starts_with(&format!(
                "{}, ghost, {}",
                crate::errmsg::INVOKER_UNKNOWN,
                crate::errmsg::INVOKER_NOT_IN_PASSWD
            )),
            "{err}"
        );
        assert!(
            err.ends_with(crate::errmsg::INVOKER_UNKNOWN_REMEDY),
            "{err}"
        );
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
            runner.clone(),
            &UserTools::own(PathBuf::from("/home/u"), runner),
            fetcher,
            (Os::Linux, Arch::Amd64, None),
            Path::new("/tmp/cache"),
        );
        let schemes = factory_schemes(&factories);
        assert_eq!(schemes.get("apt"), Some(&Scheme::Debian));
        assert_eq!(schemes.get("dnf"), Some(&Scheme::Rpm));
        assert_eq!(schemes.get("apk"), Some(&Scheme::Alpine));
        assert_eq!(schemes.get("cargo"), Some(&Scheme::Semver));
        assert_eq!(schemes.get("npm"), Some(&Scheme::Semver));
        assert_eq!(schemes.get("uv"), Some(&Scheme::Pep440));
        assert_eq!(schemes.get("github-release"), Some(&Scheme::Semver));
    }

    #[test]
    fn factory_option_keys_cover_every_compiled_in_backend_without_probing() {
        let runner: Arc<dyn CommandRunner> = Arc::new(ProcessCommandRunner);
        let fetcher: Arc<dyn HttpFetcher> = Arc::new(UreqHttpFetcher::new());
        let factories = compiled_in_factories(
            runner.clone(),
            &UserTools::own(PathBuf::from("/home/u"), runner),
            fetcher,
            (Os::Linux, Arch::Amd64, None),
            Path::new("/tmp/cache"),
        );
        let keys = factory_option_keys(&factories);
        assert_eq!(
            keys.get("github-release"),
            Some(&vec!["asset".to_string(), "tag".to_string()])
        );
        assert_eq!(keys.get("apt"), Some(&vec![]));
        assert_eq!(keys.get("apk"), Some(&vec![]));
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
    fn cargo_home_is_its_variable_else_dot_cargo() {
        let home = Path::new("/home/ben");
        assert_eq!(cargo_home(home, None), home.join(".cargo"));
        assert_eq!(cargo_home(home, Some("".into())), home.join(".cargo"));
        assert_eq!(
            cargo_home(home, Some("/opt/cargo".into())),
            PathBuf::from("/opt/cargo")
        );
    }

    /// The programs `runner` was asked to run, each once.
    fn programs(runner: &bosun_ports::testing::FakeCommandRunner) -> Vec<String> {
        let mut seen: Vec<String> = runner.calls().into_iter().map(|c| c.program).collect();
        seen.sort();
        seen.dedup();
        seen
    }

    /// The user-scope backends' probes run through the user's runner, as
    /// their queries do; the system-scope ones through this process's.
    #[test]
    fn user_scope_backends_are_probed_through_the_user_s_runner() {
        use bosun_ports::testing::{FakeCommandRunner, FakeHttpFetcher};
        let root = Arc::new(FakeCommandRunner::new());
        let user = Arc::new(FakeCommandRunner::new());
        let tools = UserTools::own(PathBuf::from("/home/u"), user.clone());
        let factories = compiled_in_factories(
            root.clone(),
            &tools,
            Arc::new(FakeHttpFetcher::new()),
            (Os::Linux, Arch::Amd64, None),
            Path::new("/tmp/cache"),
        );
        let host = HostInfo {
            os: Os::Linux,
            arch: Arch::Amd64,
            distro: None,
            runner: root.as_ref(),
        };
        let registry = build_registry(factories, &host);
        assert_eq!(programs(&user), ["cargo", "npm", "uv"]);
        let by_root = programs(&root);
        assert!(
            by_root
                .iter()
                .any(|p| p == "apt-cache" || p == "apt-get" || p == "dpkg-query"),
            "{by_root:?}"
        );
        assert!(
            !by_root
                .iter()
                .any(|p| ["cargo", "npm", "uv"].contains(&p.as_str())),
            "{by_root:?}"
        );
        let release = registry.get("github-release").expect("always present");
        assert_eq!(release.meta.purl_type, "github");
    }

    #[test]
    fn this_account_s_tools_are_its_own_and_the_invoking_user_s_are_theirs() {
        let runner: Arc<dyn CommandRunner> =
            Arc::new(bosun_ports::testing::FakeCommandRunner::new());
        let mine_own = UserTools::own(PathBuf::from("/home/u"), runner.clone());
        assert_eq!(mine_own.bin_dir(), PathBuf::from("/home/u/.local/bin"));
        assert!(mine_own.bin_files.is_none() && mine_own.path_var.is_none());
        let place = UserPlace {
            state: PathBuf::from("/s/state.json"),
            config: None,
            invoker: Some(Invoker {
                name: "bosun-no-such-user".to_string(),
                uid: 4242,
                gid: 4343,
                home: PathBuf::from("/srv/ben"),
            }),
        };
        let theirs = UserTools::for_place(&place, runner.clone());
        assert_eq!(theirs.home, PathBuf::from("/srv/ben"));
        assert_eq!(theirs.cargo_home, PathBuf::from("/srv/ben/.cargo"));
        assert_eq!(theirs.bin_dir(), PathBuf::from("/srv/ben/.local/bin"));
        assert!(theirs.bin_files.is_some() && theirs.path_var.is_some());
        let mine = UserTools::for_place(&own(), runner);
        assert!(mine.bin_files.is_none() && mine.path_var.is_none());
        assert_eq!(mine.home, home_dir());
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
