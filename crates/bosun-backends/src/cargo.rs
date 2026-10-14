//! The cargo backend (§5.2 language tier): platform-orthogonal, user-scope,
//! installs any version from crates.io. Mutations are step lists
//! (`cargo install --locked name@ver`); `verify_installed` reads
//! `cargo install --list`; `query_available` reads the crates.io sparse index
//! over HTTP (the query side may run anywhere, §4.7).
//!
//! Language backends have no durable hold — a re-run just re-asserts the exact
//! version — so `hold` is a no-op and `supports_hold` is false (§5.2).

use std::sync::Arc;

use bosun_core::{BackendId, Capabilities, Ref, Scheme, Scope, Source, Target, Version};
use bosun_ports::{
    Backend, BackendFactory, Command, CommandRunner, HostInfo, HttpFetcher, HttpRequest, Index,
    Installer, PortError, Probe, Step, Steps,
};

const BACKEND: &str = "cargo";

/// Builds the cargo backend when `cargo` is present.
pub struct CargoFactory {
    runner: Arc<dyn CommandRunner>,
    fetcher: Arc<dyn HttpFetcher>,
}

impl CargoFactory {
    /// Construct with the command runner and HTTP fetcher the backend uses.
    pub fn new(runner: Arc<dyn CommandRunner>, fetcher: Arc<dyn HttpFetcher>) -> CargoFactory {
        CargoFactory { runner, fetcher }
    }
}

impl BackendFactory for CargoFactory {
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
            _version: probe.version,
        };
        Backend {
            index: Box::new(backend.clone()),
            installer: Box::new(backend),
        }
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
    _version: Version,
}

fn argv(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|s| s.to_string()).collect()
}

/// `name@version`, how cargo names an exact install.
fn at(pkg: &Ref, version: &Version) -> String {
    format!("{}@{}", pkg.package.id, version.as_str())
}

impl Index for CargoBackend {
    fn refresh(&self) -> Result<(), PortError> {
        // The sparse index is fetched fresh per query; nothing to refresh.
        Ok(())
    }

    fn query_available(&self, pkg: &Ref, _target: &Target) -> Result<Vec<Version>, PortError> {
        let url = format!("https://index.crates.io/{}", sparse_path(&pkg.package.id));
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
            supports_hold: false,     // language tier: no durable hold (§5.2)
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
        }])
    }

    fn verify_installed(&self, pkg: &Ref) -> Result<Option<Version>, PortError> {
        // `cargo install --list` prints `name vX.Y.Z:` then its binaries.
        let out = self
            .runner
            .run(&Command::new("cargo", argv(&["install", "--list"])))?;
        if !out.success() {
            return Ok(None);
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
            if name == pkg.package.id {
                let v = vers.trim_end_matches(':').trim_start_matches('v');
                return Version::parse(v, Scheme::Semver).map(Some).map_err(|e| {
                    PortError::CommandOutput {
                        detail: format!("cargo list version {v:?}: {e}"),
                    }
                });
            }
        }
        Ok(None)
    }

    fn hold(&self, _pkg: &Ref, _version: &Version) -> Result<Steps, PortError> {
        Ok(vec![]) // no durable hold; bosun re-asserts the exact version each run
    }

    fn downgrade(&self, pkg: &Ref, version: &Version) -> Result<Steps, PortError> {
        // `cargo install name@lower` overwrites with the lower version.
        self.install_exact(pkg, version)
    }

    fn uninstall(&self, pkg: &Ref) -> Result<Steps, PortError> {
        Ok(vec![Step::Run {
            argv: argv(&["cargo", "uninstall", &pkg.package.id]),
            env: vec![],
            scope: Scope::User,
        }])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bosun_core::{Os, PackageId};
    use bosun_ports::testing::{FakeCommandRunner, FakeHttpFetcher};
    use bosun_ports::Output;

    fn sem(v: &str) -> Version {
        Version::parse(v, Scheme::Semver).unwrap()
    }

    fn reff(name: &str) -> Ref {
        Ref::new(PackageId::new(name))
    }

    fn backend(runner: Arc<dyn CommandRunner>, fetcher: Arc<dyn HttpFetcher>) -> CargoBackend {
        CargoBackend {
            runner,
            fetcher,
            _version: sem("1.96.1"),
        }
    }

    fn user_run(argv: &[&str]) -> Step {
        Step::Run {
            argv: argv.iter().map(|s| s.to_string()).collect(),
            env: vec![],
            scope: Scope::User,
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
        assert!(!c.supports_hold);
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
            Some(sem("14.1.1"))
        );
        assert_eq!(b.verify_installed(&reff("absent")).unwrap(), None);
        assert_eq!(fake.calls()[0].args, vec!["install", "--list"]);
    }

    #[test]
    fn verify_installed_not_installed_is_none() {
        let fake = Arc::new(FakeCommandRunner::new().with_output(Output {
            code: Some(101),
            stdout: vec![],
            stderr: b"error".to_vec(),
        }));
        assert_eq!(
            backend(fake, Arc::new(FakeHttpFetcher::new()))
                .verify_installed(&reff("x"))
                .unwrap(),
            None
        );
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
}
