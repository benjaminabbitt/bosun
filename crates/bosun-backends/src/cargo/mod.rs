//! The cargo backend (§5.2 language tier): platform-orthogonal, user-scope,
//! installs any version from crates.io. Mutations are step lists
//! (`cargo install --locked name@ver`); `verify_installed` reads
//! `cargo install --list`; `query_available` reads the crates.io sparse index
//! over HTTP (the query side may run anywhere, §4.7). On this machine that
//! is the index cargo's configuration replaces crates.io with, if any
//! ([`registry`]); the remote index reads crates.io's.
//!
//! Language backends have no durable hold — a re-run just re-asserts the exact
//! version — so `hold` is a no-op and `supports_hold` is false (§5.2).

use std::path::PathBuf;
use std::sync::Arc;

use bosun_core::{BackendId, Capabilities, Installed, Ref, Scheme, Scope, Source, Target, Version};
use bosun_ports::{
    Backend, BackendFactory, Command, CommandRunner, HostInfo, HttpFetcher, HttpRequest, Index,
    Installer, PortError, Probe, Step, Steps, UserInstall, UserInstalls,
};
use tracing::debug;

use crate::logmsg;

mod registry;

const BACKEND: &str = "cargo";

/// Builds the cargo backend when `cargo` is present.
pub struct CargoFactory {
    runner: Arc<dyn CommandRunner>,
    fetcher: Arc<dyn HttpFetcher>,
    config: Option<(PathBuf, PathBuf)>,
}

impl CargoFactory {
    /// Construct with the command runner and HTTP fetcher the backend uses.
    pub fn new(runner: Arc<dyn CommandRunner>, fetcher: Arc<dyn HttpFetcher>) -> CargoFactory {
        CargoFactory {
            runner,
            fetcher,
            config: None,
        }
    }

    /// Read cargo's configuration as `cargo install` run in `cwd` would,
    /// with `cargo_home` its `$CARGO_HOME`, for the index the built backend
    /// lists from (the remote index keeps crates.io's).
    pub fn with_config(mut self, cwd: PathBuf, cargo_home: PathBuf) -> CargoFactory {
        self.config = Some((cwd, cargo_home));
        self
    }

    /// The index cargo's configuration names, or why bosun cannot read it.
    fn configured_index(&self) -> Result<String, PortError> {
        let Some((cwd, cargo_home)) = &self.config else {
            return Ok(registry::CRATES_IO.to_string());
        };
        let files = registry::config_files(cwd, cargo_home);
        let found = registry::read(&files).and_then(|tables| registry::crates_io_index(&tables));
        match &found {
            Ok(index) => debug!(message = logmsg::CARGO_INDEX, index = %index),
            Err(why) => debug!(message = logmsg::CARGO_INDEX_UNREADABLE, why = %why),
        }
        found.map_err(|why| PortError::Backend {
            detail: format!("{}: {why}", crate::errmsg::CARGO_INDEX_UNREADABLE),
        })
    }
}

impl BackendFactory for CargoFactory {
    fn meta(&self) -> bosun_ports::BackendMeta {
        bosun_ports::BackendMeta {
            purl_type: "cargo",
            vers_type: "cargo",
            platform_independent: true,
            option_keys: &[],
        }
    }

    fn id(&self) -> BackendId {
        BackendId::new(BACKEND)
    }

    fn probe(&self, host: &HostInfo) -> Result<Option<Probe>, PortError> {
        // cargo is platform-orthogonal; present on any OS where it is on PATH.
        let out = host
            .runner
            .run(&Command::new("cargo", ["--version".to_string()]))?;
        if !out.success() {
            return Ok(None);
        }
        Ok(Some(Probe {
            backend: BackendId::new(BACKEND),
            version: parse_cargo_version(&out.stdout_utf8())?,
        }))
    }

    fn build(&self, probe: Probe) -> Backend {
        let backend = CargoBackend {
            runner: self.runner.clone(),
            fetcher: self.fetcher.clone(),
            _version: Some(probe.version),
            index: self.configured_index(),
        };
        Backend {
            index: Box::new(backend.clone()),
            installer: Box::new(backend),
        }
    }

    fn remote_index(&self) -> Option<Box<dyn Index>> {
        // crates.io's sparse index is the query side wherever it runs.
        Some(Box::new(CargoBackend {
            runner: self.runner.clone(),
            fetcher: self.fetcher.clone(),
            _version: None,
            index: Ok(registry::CRATES_IO.to_string()),
        }))
    }
}

/// `cargo 1.96.1 (…)` → `1.96.1`.
fn parse_cargo_version(stdout: &str) -> Result<Version, PortError> {
    stdout
        .split_whitespace()
        .nth(1)
        .ok_or_else(|| PortError::CommandOutput {
            detail: format!("could not read cargo version from {stdout:?}"),
        })
        .and_then(|v| {
            Version::parse(v, Scheme::Semver).map_err(|e| PortError::CommandOutput {
                detail: format!("cargo version {v:?}: {e}"),
            })
        })
}

/// The crates.io sparse-index path for a crate name (lowercased), e.g.
/// `ripgrep` → `ri/pg/ripgrep`, `fd` → `2/fd`, `a` → `1/a`.
fn sparse_path(name: &str) -> String {
    let n = name.to_ascii_lowercase();
    match n.len() {
        1 => format!("1/{n}"),
        2 => format!("2/{n}"),
        3 => format!("3/{}/{n}", &n[0..1]),
        _ => format!("{}/{}/{n}", &n[0..2], &n[2..4]),
    }
}

#[derive(Clone)]
struct CargoBackend {
    runner: Arc<dyn CommandRunner>,
    fetcher: Arc<dyn HttpFetcher>,
    _version: Option<Version>,
    /// The sparse index crates are listed from, or why it cannot be read.
    index: Result<String, PortError>,
}

fn argv(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|s| s.to_string()).collect()
}

/// `name@version`, how cargo names an exact install.
fn at(pkg: &Ref, version: &Version) -> String {
    format!("{}@{}", pkg.purl.native_id(), version.as_str())
}

impl Index for CargoBackend {
    fn refresh(&self) -> Result<(), PortError> {
        // The sparse index is fetched fresh per query; nothing to refresh.
        Ok(())
    }

    fn query_available(&self, pkg: &Ref, _target: &Target) -> Result<Vec<Version>, PortError> {
        let index = self.index.as_ref().map_err(Clone::clone)?;
        let url = format!("{index}/{}", sparse_path(&pkg.purl.native_id()));
        let resp = self.fetcher.fetch(&HttpRequest::get(&url))?;
        if resp.status == 404 {
            return Ok(vec![]); // unknown crate
        }
        if !(200..300).contains(&resp.status) {
            return Err(PortError::HttpStatus {
                url,
                status: resp.status,
            });
        }
        let body = String::from_utf8_lossy(&resp.body);
        let mut versions = Vec::new();
        for line in body.lines().filter(|l| !l.trim().is_empty()) {
            let entry: SparseEntry =
                serde_json::from_str(line).map_err(|e| PortError::Backend {
                    detail: format!("crates.io index line {line:?}: {e}"),
                })?;
            if entry.yanked {
                continue;
            }
            if let Ok(v) = Version::parse(&entry.vers, Scheme::Semver) {
                versions.push(v);
            }
        }
        Ok(versions)
    }
}

/// What the user installed with cargo (§9 state export): every crates.io
/// crate in `cargo install --list` (`name vX.Y.Z:`). One installed from a
/// path or git (`name vX.Y.Z (source):`) is left out: bosun installs from
/// crates.io, which may not have it.
pub fn user_installs(runner: &dyn CommandRunner) -> Result<UserInstalls, PortError> {
    let out = runner.run(&Command::new("cargo", argv(&["install", "--list"])))?;
    if !out.success() {
        return Err(PortError::CommandFailed {
            program: "cargo".to_string(),
            code: out.code,
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        });
    }
    let mut found = Vec::new();
    // Package lines have no leading whitespace; binary lines do.
    for line in out
        .stdout_utf8()
        .lines()
        .filter(|l| !l.starts_with(char::is_whitespace))
    {
        let mut words = line.trim_end_matches(':').split(' ');
        let (Some(name), Some(raw), source) = (words.next(), words.next(), words.next()) else {
            return Err(PortError::CommandOutput {
                detail: format!("cargo install --list line {line:?}"),
            });
        };
        if source.is_some() {
            debug!(message = logmsg::NOT_FROM_CRATES_IO, line);
            continue;
        }
        let version = raw.strip_prefix('v').unwrap_or(raw);
        let version =
            Version::parse(version, Scheme::Semver).map_err(|e| PortError::CommandOutput {
                detail: format!("cargo install --list version {raw:?} of {name}: {e}"),
            })?;
        found.push(UserInstall {
            package: name.to_string(),
            version,
        });
    }
    Ok(UserInstalls::Listed(found))
}

#[derive(serde::Deserialize)]
struct SparseEntry {
    vers: String,
    #[serde(default)]
    yanked: bool,
}

impl Installer for CargoBackend {
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            supports_exact_version: true,
            // Reinstalls the exact version each run, so a hold is unnecessary (§5.2).
            hold: bosun_core::HoldSupport::NotNeeded,
            supports_downgrade: true, // installing a lower version overwrites
        }
    }

    fn scope(&self) -> Scope {
        Scope::User
    }

    fn ensure_source(&self, _source: &Source) -> Result<Steps, PortError> {
        // crates.io is the default registry; alternate registries are future.
        Ok(vec![])
    }

    fn install_exact(&self, pkg: &Ref, version: &Version) -> Result<Steps, PortError> {
        Ok(vec![Step::Run {
            argv: argv(&["cargo", "install", "--locked", &at(pkg, version)]),
            env: vec![],
            scope: Scope::User,
            ok_exits: Vec::new(),
        }])
    }

    fn verify_installed(&self, pkg: &Ref) -> Result<Installed, PortError> {
        // `cargo install --list` prints `name vX.Y.Z:` then its binaries.
        let out = self
            .runner
            .run(&Command::new("cargo", argv(&["install", "--list"])))?;
        if !out.success() {
            return Ok(Installed::Absent);
        }
        for line in out.stdout_utf8().lines() {
            // Package lines have no leading whitespace; binary lines do.
            if line.starts_with(char::is_whitespace) {
                continue;
            }
            let mut toks = line.split_whitespace();
            let (Some(name), Some(vers)) = (toks.next(), toks.next()) else {
                continue;
            };
            if name == pkg.purl.native_id() {
                let v = vers.trim_end_matches(':').trim_start_matches('v');
                return Version::parse(v, Scheme::Semver)
                    .map(|v| Installed::Present { version: Some(v) })
                    .map_err(|e| PortError::CommandOutput {
                        detail: format!("cargo list version {v:?}: {e}"),
                    });
            }
        }
        Ok(Installed::Absent)
    }

    fn hold(&self, _pkg: &Ref, _version: &Version) -> Result<Steps, PortError> {
        Ok(vec![]) // no durable hold; bosun re-asserts the exact version each run
    }
    fn unhold(&self, _pkg: &Ref) -> Result<Steps, PortError> {
        Ok(Vec::new())
    }

    fn downgrade(&self, pkg: &Ref, version: &Version) -> Result<Steps, PortError> {
        // `cargo install name@lower` overwrites with the lower version.
        self.install_exact(pkg, version)
    }

    fn user_installs(&self) -> Result<UserInstalls, PortError> {
        user_installs(self.runner.as_ref())
    }

    fn uninstall(&self, pkg: &Ref) -> Result<Steps, PortError> {
        Ok(vec![Step::Run {
            argv: argv(&["cargo", "uninstall", &pkg.purl.native_id()]),
            env: vec![],
            scope: Scope::User,
            ok_exits: Vec::new(),
        }])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bosun_core::Os;
    use bosun_ports::testing::{FakeCommandRunner, FakeHttpFetcher};
    use bosun_ports::Output;

    fn sem(v: &str) -> Version {
        Version::parse(v, Scheme::Semver).unwrap()
    }

    fn reff(name: &str) -> Ref {
        Ref::new(format!("pkg:cargo/{name}").parse().unwrap())
    }

    fn backend(runner: Arc<dyn CommandRunner>, fetcher: Arc<dyn HttpFetcher>) -> CargoBackend {
        CargoBackend {
            runner,
            fetcher,
            _version: Some(sem("1.96.1")),
            index: Ok(registry::CRATES_IO.to_string()),
        }
    }

    fn user_run(argv: &[&str]) -> Step {
        Step::Run {
            argv: argv.iter().map(|s| s.to_string()).collect(),
            env: vec![],
            scope: Scope::User,
            ok_exits: Vec::new(),
        }
    }

    #[test]
    fn install_and_downgrade_use_locked_at_version() {
        let b = backend(
            Arc::new(FakeCommandRunner::new()),
            Arc::new(FakeHttpFetcher::new()),
        );
        let expected = vec![user_run(&["cargo", "install", "--locked", "eza@0.20.0"])];
        assert_eq!(
            b.install_exact(&reff("eza"), &sem("0.20.0")).unwrap(),
            expected
        );
        assert_eq!(b.downgrade(&reff("eza"), &sem("0.20.0")).unwrap(), expected);
    }

    #[test]
    fn uninstall_argv_and_hold_is_a_noop() {
        let b = backend(
            Arc::new(FakeCommandRunner::new()),
            Arc::new(FakeHttpFetcher::new()),
        );
        assert_eq!(
            b.uninstall(&reff("eza")).unwrap(),
            vec![user_run(&["cargo", "uninstall", "eza"])]
        );
        assert_eq!(
            b.hold(&reff("eza"), &sem("0.20.0")).unwrap(),
            Vec::<Step>::new()
        );
    }

    #[test]
    fn capabilities_language_tier() {
        let b = backend(
            Arc::new(FakeCommandRunner::new()),
            Arc::new(FakeHttpFetcher::new()),
        );
        let c = b.capabilities();
        assert!(c.supports_exact_version);
        assert_eq!(c.hold, bosun_core::HoldSupport::NotNeeded);
        assert!(c.supports_downgrade);
        assert_eq!(b.scope(), Scope::User);
    }

    #[test]
    fn verify_installed_parses_cargo_list() {
        let list = "eza v0.20.0:\n    eza\nripgrep v14.1.1:\n    rg\n";
        // Two calls → the replay fake needs the output queued twice.
        let fake = Arc::new(FakeCommandRunner::new().with_stdout(list).with_stdout(list));
        let b = backend(fake.clone(), Arc::new(FakeHttpFetcher::new()));
        assert_eq!(
            b.verify_installed(&reff("ripgrep")).unwrap(),
            Installed::Present {
                version: Some(sem("14.1.1"))
            }
        );
        assert_eq!(
            b.verify_installed(&reff("absent")).unwrap(),
            Installed::Absent
        );
        assert_eq!(fake.calls()[0].args, vec!["install", "--list"]);
    }

    #[test]
    fn verify_installed_not_installed_is_absent() {
        let fake = Arc::new(FakeCommandRunner::new().with_output(Output {
            code: Some(101),
            stdout: vec![],
            stderr: b"error".to_vec(),
        }));
        assert_eq!(
            backend(fake, Arc::new(FakeHttpFetcher::new()))
                .verify_installed(&reff("x"))
                .unwrap(),
            Installed::Absent
        );
    }

    const INSTALL_LIST: &str = include_str!("../../tests/fixtures/cargo/install-list.txt");

    fn listed(found: UserInstalls) -> Vec<(String, String)> {
        let UserInstalls::Listed(found) = found else {
            panic!("want a listing, got {found:?}");
        };
        found
            .into_iter()
            .map(|u| (u.package, u.version.as_str().to_string()))
            .collect()
    }

    #[test]
    fn user_installs_are_the_crates_io_installs() {
        // Path installs (`name v0.1.0 (/home/…):`) are not on crates.io.
        let fake = Arc::new(FakeCommandRunner::new().with_stdout(INSTALL_LIST));
        let b = backend(fake.clone(), Arc::new(FakeHttpFetcher::new()));
        let want: Vec<(String, String)> = [
            ("ast-grep", "0.40.4"),
            ("bacon", "3.22.0"),
            ("cargo-deny", "0.20.2"),
            ("cargo-llvm-cov", "0.8.4"),
            ("cargo-machete", "0.9.1"),
        ]
        .iter()
        .map(|(p, v)| (p.to_string(), v.to_string()))
        .collect();
        assert_eq!(listed(b.user_installs().unwrap()), want);
        assert_eq!(fake.calls()[0].program, "cargo");
        assert_eq!(fake.calls()[0].args, vec!["install", "--list"]);
    }

    #[test]
    fn a_git_install_is_left_out() {
        let list = "tool v0.1.0 (https://github.com/o/tool#1a2b3c4d):\n    tool\n";
        let fake = FakeCommandRunner::new().with_stdout(list);
        assert_eq!(listed(user_installs(&fake).unwrap()), vec![]);
    }

    #[test]
    fn cargo_install_list_failing_is_an_error() {
        let fake = FakeCommandRunner::new().with_output(Output {
            code: Some(101),
            stdout: vec![],
            stderr: b"error".to_vec(),
        });
        let err = user_installs(&fake).unwrap_err();
        assert!(
            matches!(err, PortError::CommandFailed { ref program, code: Some(101), .. } if program == "cargo"),
            "{err:?}"
        );
    }

    #[test]
    fn a_package_line_cargo_should_not_print_is_an_error() {
        for list in ["ripgrep:\n", "ripgrep vnope:\n"] {
            let fake = FakeCommandRunner::new().with_stdout(list);
            let err = user_installs(&fake).unwrap_err();
            assert!(
                matches!(err, PortError::CommandOutput { .. }),
                "{list:?}: {err:?}"
            );
        }
    }

    #[test]
    fn query_available_reads_sparse_index() {
        let ndjson = "{\"name\":\"ripgrep\",\"vers\":\"14.0.0\",\"yanked\":false}\n\
                      {\"name\":\"ripgrep\",\"vers\":\"14.1.0\",\"yanked\":true}\n\
                      {\"name\":\"ripgrep\",\"vers\":\"14.1.1\",\"yanked\":false}\n";
        let fetcher = Arc::new(
            FakeHttpFetcher::new().with_body("https://index.crates.io/ri/pg/ripgrep", ndjson),
        );
        let got = backend(Arc::new(FakeCommandRunner::new()), fetcher)
            .query_available(&reff("ripgrep"), &"linux/amd64".parse().unwrap())
            .unwrap();
        // Yanked 14.1.0 is skipped.
        assert_eq!(got, vec![sem("14.0.0"), sem("14.1.1")]);
    }

    #[test]
    fn sparse_path_by_name_length() {
        assert_eq!(sparse_path("a"), "1/a");
        assert_eq!(sparse_path("fd"), "2/fd");
        assert_eq!(sparse_path("rip"), "3/r/rip");
        assert_eq!(sparse_path("ripgrep"), "ri/pg/ripgrep");
        assert_eq!(sparse_path("Serde"), "se/rd/serde"); // lowercased
    }

    #[test]
    fn probe_reads_version_on_any_os() {
        let factory = CargoFactory::new(
            Arc::new(FakeCommandRunner::new()),
            Arc::new(FakeHttpFetcher::new()),
        );
        let runner = FakeCommandRunner::new().with_stdout("cargo 1.96.1 (abc 2026-06-26)\n");
        let host = HostInfo {
            os: Os::Macos, // platform-orthogonal: still present
            arch: bosun_core::Arch::Arm64,
            distro: None,
            runner: &runner,
        };
        let probe = factory.probe(&host).unwrap().unwrap();
        assert_eq!(probe.version, sem("1.96.1"));
    }

    const RG: &str = r#"{"name":"ripgrep","vers":"14.1.1","yanked":false}"#;

    /// The cargo backend built on a machine whose cargo config in `cwd`
    /// (and an empty `$CARGO_HOME`) says `config`.
    fn built_with_config(config: &str, fetcher: Arc<dyn HttpFetcher>) -> Backend {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().join("project");
        std::fs::create_dir_all(cwd.join(".cargo")).unwrap();
        std::fs::write(cwd.join(".cargo/config.toml"), config).unwrap();
        CargoFactory::new(Arc::new(FakeCommandRunner::new()), fetcher)
            .with_config(cwd, dir.path().join("cargo-home"))
            .build(Probe {
                backend: BackendId::new("cargo"),
                version: sem("1.96.1"),
            })
    }

    #[test]
    fn the_index_is_the_one_cargos_config_replaces_crates_io_with() {
        let fetcher = Arc::new(
            FakeHttpFetcher::new().with_body("https://mirror.example/index/ri/pg/ripgrep", RG),
        );
        let built = built_with_config(
            "[source.crates-io]\nreplace-with = \"m\"\n[source.m]\nregistry = \"sparse+https://mirror.example/index/\"\n",
            fetcher,
        );
        let rg = Ref::new("pkg:cargo/ripgrep".parse().unwrap());
        assert_eq!(
            built.index.query_available(&rg, &Target::Any).unwrap(),
            vec![sem("14.1.1")]
        );
    }

    #[test]
    fn an_index_bosun_cannot_read_fails_the_query_naming_it() {
        let built = built_with_config(
            "[source.crates-io]\nreplace-with = \"v\"\n[source.v]\ndirectory = \"vendor\"\n",
            Arc::new(FakeHttpFetcher::new()),
        );
        let rg = Ref::new("pkg:cargo/ripgrep".parse().unwrap());
        let err = built.index.query_available(&rg, &Target::Any).unwrap_err();
        assert!(
            err.to_string()
                .contains(crate::errmsg::CARGO_INDEX_UNREADABLE)
                && err.to_string().contains("directory"),
            "{err}"
        );
        let unparsable = built_with_config("[source\n", Arc::new(FakeHttpFetcher::new()));
        assert!(unparsable.index.query_available(&rg, &Target::Any).is_err());
    }

    #[test]
    fn with_no_config_read_the_built_backend_lists_crates_io() {
        let fetcher =
            Arc::new(FakeHttpFetcher::new().with_body("https://index.crates.io/ri/pg/ripgrep", RG));
        let built = CargoFactory::new(Arc::new(FakeCommandRunner::new()), fetcher).build(Probe {
            backend: BackendId::new("cargo"),
            version: sem("1.96.1"),
        });
        let rg = Ref::new("pkg:cargo/ripgrep".parse().unwrap());
        assert_eq!(
            built.index.query_available(&rg, &Target::Any).unwrap(),
            vec![sem("14.1.1")]
        );
    }

    #[test]
    fn a_remote_index_reads_crates_io_and_runs_nothing() {
        let runner = Arc::new(FakeCommandRunner::new());
        let fetcher = Arc::new(FakeHttpFetcher::new().with_body(
            "https://index.crates.io/ri/pg/ripgrep",
            r#"{"vers":"14.1.1","yanked":false}"#,
        ));
        let index = CargoFactory::new(runner.clone(), fetcher)
            .remote_index()
            .expect("a remote index");
        let rg = Ref::new("pkg:cargo/ripgrep".parse().unwrap());
        let versions = index.query_available(&rg, &Target::Any).unwrap();
        assert_eq!(versions, vec![sem("14.1.1")]);
        assert!(runner.calls().is_empty());
    }
}
