//! The apt backend (Debian/Ubuntu, §5.2 versioned-OS tier). Mutations are
//! described as step lists (`apt-get install pkg=ver`, `apt-mark hold`, …) a
//! host executor runs; `verify_installed`/`query_available` run directly via
//! the [`CommandRunner`]. apt is Linux-only and system-scope.
//!
//! Non-interactive per Appendix B: `-y` plus `DEBIAN_FRONTEND=noninteractive`.
//! A downgrade passes `--allow-downgrades` only on the explicit downgrade path.

use std::sync::Arc;

use bosun_core::{
    BackendId, Capabilities, Installed, Os, Ref, Scheme, Scope, Source, Target, Version,
};
use bosun_ports::{
    Backend, BackendFactory, Command, CommandRunner, HostInfo, Index, Installer, PortError, Probe,
    Step, Steps,
};

const BACKEND: &str = "apt";

/// Builds the apt backend when `apt-get` is present (§5.3).
pub struct AptFactory {
    runner: Arc<dyn CommandRunner>,
}

impl AptFactory {
    /// Construct with the command runner the built backend will use.
    pub fn new(runner: Arc<dyn CommandRunner>) -> AptFactory {
        AptFactory { runner }
    }
}

impl BackendFactory for AptFactory {
    fn id(&self) -> BackendId {
        BackendId::new(BACKEND)
    }

    fn probe(&self, host: &HostInfo) -> Result<Option<Probe>, PortError> {
        if host.os != Os::Linux {
            return Ok(None); // apt is Linux-only
        }
        let out = host
            .runner
            .run(&Command::new("apt-get", ["--version".to_string()]))?;
        if !out.success() {
            return Ok(None); // apt-get not on PATH / not usable
        }
        Ok(Some(Probe {
            backend: BackendId::new(BACKEND),
            version: parse_apt_version(&out.stdout_utf8())?,
        }))
    }

    fn build(&self, probe: Probe) -> Backend {
        let backend = AptBackend {
            runner: self.runner.clone(),
            _version: probe.version,
        };
        Backend {
            index: Box::new(backend.clone()),
            installer: Box::new(backend),
        }
    }
}

/// `apt 2.4.11 (amd64)` → `2.4.11`.
fn parse_apt_version(stdout: &str) -> Result<Version, PortError> {
    stdout
        .split_whitespace()
        .nth(1)
        .ok_or_else(|| PortError::CommandOutput {
            detail: format!("could not read apt version from {stdout:?}"),
        })
        .and_then(|v| {
            Version::parse(v, Scheme::Debian).map_err(|e| PortError::CommandOutput {
                detail: format!("apt version {v:?}: {e}"),
            })
        })
}

#[derive(Clone)]
struct AptBackend {
    runner: Arc<dyn CommandRunner>,
    // Recorded for future per-version flag gating (Appendix B #224).
    _version: Version,
}

/// `DEBIAN_FRONTEND=noninteractive`, so apt never prompts (Appendix B).
fn noninteractive() -> Vec<(String, String)> {
    vec![("DEBIAN_FRONTEND".to_string(), "noninteractive".to_string())]
}

fn argv(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|s| s.to_string()).collect()
}

/// `<package>=<version>`, how apt names an exact install.
fn pinned(pkg: &Ref, version: &Version) -> String {
    format!("{}={}", pkg.purl.native_id(), version.as_str())
}

impl Index for AptBackend {
    fn refresh(&self) -> Result<(), PortError> {
        let out = self
            .runner
            .run(&Command::new("apt-get", argv(&["update"])).with_env(noninteractive()))?;
        if out.success() {
            Ok(())
        } else {
            Err(PortError::CommandFailed {
                program: "apt-get".to_string(),
                code: out.code,
                stderr: out.stdout_utf8().into_owned(),
            })
        }
    }

    fn query_available(&self, pkg: &Ref, _target: &Target) -> Result<Vec<Version>, PortError> {
        // `apt-cache madison <pkg>` lists `pkg | version | source` per line.
        let out = self.runner.run(&Command::new(
            "apt-cache",
            argv(&["madison", &pkg.purl.native_id()]),
        ))?;
        if !out.success() {
            return Ok(vec![]); // unknown package → no versions
        }
        let mut versions = Vec::new();
        for line in out.stdout_utf8().lines() {
            if let Some(field) = line.split('|').nth(1) {
                if let Ok(v) = Version::parse(field.trim(), Scheme::Debian) {
                    versions.push(v);
                }
            }
        }
        Ok(versions)
    }
}

impl Installer for AptBackend {
    fn capabilities(&self) -> Capabilities {
        // apt installs exact repo versions, holds (`apt-mark hold`) and
        // downgrades (`--allow-downgrades`) — all supported.
        Capabilities {
            supports_exact_version: true,
            supports_hold: true,
            supports_downgrade: true,
        }
    }

    fn scope(&self) -> Scope {
        Scope::System
    }

    fn ensure_source(&self, source: &Source) -> Result<Steps, PortError> {
        // Add a PPA/repo, then refresh its index. (M1 dogfood uses the default
        // repos and needs none; this covers an explicit source.)
        Ok(vec![
            Step::Run {
                argv: argv(&["add-apt-repository", "-y", source.id.as_str()]),
                env: noninteractive(),
                scope: Scope::System,
            },
            Step::Run {
                argv: argv(&["apt-get", "update"]),
                env: noninteractive(),
                scope: Scope::System,
            },
        ])
    }

    fn install_exact(&self, pkg: &Ref, version: &Version) -> Result<Steps, PortError> {
        Ok(vec![Step::Run {
            argv: argv(&[
                "apt-get",
                "install",
                "-y",
                "--no-install-recommends",
                &pinned(pkg, version),
            ]),
            env: noninteractive(),
            scope: Scope::System,
        }])
    }

    fn verify_installed(&self, pkg: &Ref) -> Result<Installed, PortError> {
        // `dpkg-query -W -f=${Version} <pkg>`: a non-zero exit (not installed)
        // is data, not an error.
        let out = self.runner.run(&Command::new(
            "dpkg-query",
            argv(&["-W", "-f=${Version}", &pkg.purl.native_id()]),
        ))?;
        if !out.success() {
            return Ok(Installed::Absent);
        }
        let raw = out.stdout_utf8();
        let raw = raw.trim();
        if raw.is_empty() {
            return Ok(Installed::Absent);
        }
        Version::parse(raw, Scheme::Debian)
            .map(|v| Installed::Present { version: Some(v) })
            .map_err(|e| PortError::CommandOutput {
                detail: format!("dpkg version {raw:?}: {e}"),
            })
    }

    fn hold(&self, pkg: &Ref, _version: &Version) -> Result<Steps, PortError> {
        Ok(vec![Step::Run {
            argv: argv(&["apt-mark", "hold", &pkg.purl.native_id()]),
            env: noninteractive(),
            scope: Scope::System,
        }])
    }

    fn downgrade(&self, pkg: &Ref, version: &Version) -> Result<Steps, PortError> {
        Ok(vec![Step::Run {
            argv: argv(&[
                "apt-get",
                "install",
                "-y",
                "--allow-downgrades",
                &pinned(pkg, version),
            ]),
            env: noninteractive(),
            scope: Scope::System,
        }])
    }

    fn uninstall(&self, pkg: &Ref) -> Result<Steps, PortError> {
        Ok(vec![Step::Run {
            argv: argv(&["apt-get", "remove", "-y", &pkg.purl.native_id()]),
            env: noninteractive(),
            scope: Scope::System,
        }])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bosun_core::{Arch, SourceId};
    use bosun_ports::testing::FakeCommandRunner;
    use bosun_ports::Output;

    fn deb(v: &str) -> Version {
        Version::parse(v, Scheme::Debian).unwrap()
    }

    fn reff(name: &str) -> Ref {
        Ref::new(format!("pkg:deb/debian/{name}").parse().unwrap())
    }

    fn backend(runner: Arc<dyn CommandRunner>) -> AptBackend {
        AptBackend {
            runner,
            _version: deb("2.4.11"),
        }
    }

    /// An `apt-get`/`apt-mark` Run step at system scope. The env is a literal,
    /// not `noninteractive()`, so a mutation of that function is caught (an
    /// assertion against `noninteractive()` would be tautological).
    fn run_step(argv: &[&str]) -> Step {
        Step::Run {
            argv: argv.iter().map(|s| s.to_string()).collect(),
            env: vec![("DEBIAN_FRONTEND".to_string(), "noninteractive".to_string())],
            scope: Scope::System,
        }
    }

    #[test]
    fn install_exact_pins_the_version() {
        let b = backend(Arc::new(FakeCommandRunner::new()));
        assert_eq!(
            b.install_exact(&reff("fd-find"), &deb("9.0.0-1")).unwrap(),
            vec![run_step(&[
                "apt-get",
                "install",
                "-y",
                "--no-install-recommends",
                "fd-find=9.0.0-1"
            ])]
        );
    }

    #[test]
    fn hold_downgrade_uninstall_argv() {
        let b = backend(Arc::new(FakeCommandRunner::new()));
        assert_eq!(
            b.hold(&reff("jq"), &deb("1.7.1-3")).unwrap(),
            vec![run_step(&["apt-mark", "hold", "jq"])]
        );
        assert_eq!(
            b.downgrade(&reff("jq"), &deb("1.7.1-3")).unwrap(),
            vec![run_step(&[
                "apt-get",
                "install",
                "-y",
                "--allow-downgrades",
                "jq=1.7.1-3"
            ])]
        );
        assert_eq!(
            b.uninstall(&reff("jq")).unwrap(),
            vec![run_step(&["apt-get", "remove", "-y", "jq"])]
        );
    }

    #[test]
    fn ensure_source_adds_repo_then_updates() {
        let b = backend(Arc::new(FakeCommandRunner::new()));
        let steps = b
            .ensure_source(&Source {
                backend: BackendId::new("apt"),
                id: SourceId::new("ppa:example/ppa"),
            })
            .unwrap();
        assert_eq!(
            steps,
            vec![
                run_step(&["add-apt-repository", "-y", "ppa:example/ppa"]),
                run_step(&["apt-get", "update"]),
            ]
        );
    }

    fn host<'a>(os: Os, runner: &'a FakeCommandRunner) -> HostInfo<'a> {
        HostInfo {
            os,
            arch: Arch::Amd64,
            distro: None,
            runner,
        }
    }

    #[test]
    fn probe_present_reads_version() {
        let factory = AptFactory::new(Arc::new(FakeCommandRunner::new()));
        let runner = FakeCommandRunner::new().with_stdout("apt 2.4.11 (amd64)\n");
        let probe = factory.probe(&host(Os::Linux, &runner)).unwrap().unwrap();
        assert_eq!(probe.backend.as_str(), "apt");
        assert_eq!(probe.version, deb("2.4.11"));
    }

    #[test]
    fn probe_is_none_off_linux() {
        let factory = AptFactory::new(Arc::new(FakeCommandRunner::new()));
        let runner = FakeCommandRunner::new().with_stdout("apt 2.4.11\n");
        assert!(factory.probe(&host(Os::Macos, &runner)).unwrap().is_none());
    }

    #[test]
    fn probe_is_none_when_apt_get_fails() {
        let factory = AptFactory::new(Arc::new(FakeCommandRunner::new()));
        let runner = FakeCommandRunner::new().with_output(Output {
            code: Some(127),
            stdout: vec![],
            stderr: b"not found".to_vec(),
        });
        assert!(factory.probe(&host(Os::Linux, &runner)).unwrap().is_none());
    }

    #[test]
    fn refresh_runs_apt_update() {
        let fake = Arc::new(FakeCommandRunner::new().with_stdout(""));
        backend(fake.clone()).refresh().unwrap();
        let calls = fake.calls();
        assert_eq!(calls[0].program, "apt-get");
        assert_eq!(calls[0].args, vec!["update"]);
    }

    #[test]
    fn refresh_propagates_failure() {
        let fake = Arc::new(FakeCommandRunner::new().with_output(Output {
            code: Some(1),
            stdout: b"E: could not update".to_vec(),
            stderr: vec![],
        }));
        assert!(backend(fake).refresh().is_err());
    }

    #[test]
    fn verify_installed_reads_dpkg_query() {
        let fake = Arc::new(FakeCommandRunner::new().with_stdout("1.7.1-3ubuntu0.1"));
        let b = backend(fake.clone());
        assert_eq!(
            b.verify_installed(&reff("jq")).unwrap(),
            Installed::Present {
                version: Some(deb("1.7.1-3ubuntu0.1"))
            }
        );
        let calls = fake.calls();
        assert_eq!(calls[0].program, "dpkg-query");
        assert_eq!(calls[0].args, vec!["-W", "-f=${Version}", "jq"]);
    }

    #[test]
    fn verify_installed_not_installed_is_absent_not_error() {
        // dpkg-query exits non-zero when the package is unknown.
        let fake = Arc::new(FakeCommandRunner::new().with_output(Output {
            code: Some(1),
            stdout: vec![],
            stderr: b"no packages found".to_vec(),
        }));
        assert_eq!(
            backend(fake).verify_installed(&reff("absent")).unwrap(),
            Installed::Absent
        );
    }

    #[test]
    fn query_available_parses_madison() {
        let madison = " fd-find | 9.0.0-1 | http://deb.debian.org bookworm/main amd64\n \
                        fd-find | 8.7.0-1 | http://deb.debian.org bookworm/main amd64\n";
        let fake = Arc::new(FakeCommandRunner::new().with_stdout(madison));
        let got = backend(fake)
            .query_available(&reff("fd-find"), &"linux/amd64".parse().unwrap())
            .unwrap();
        assert_eq!(got, vec![deb("9.0.0-1"), deb("8.7.0-1")]);
    }

    #[test]
    fn parse_apt_version_reads_second_token() {
        assert_eq!(
            parse_apt_version("apt 2.4.11 (amd64)\n").unwrap(),
            deb("2.4.11")
        );
        assert!(parse_apt_version("").is_err());
    }

    #[test]
    fn capabilities_and_scope() {
        let b = backend(Arc::new(FakeCommandRunner::new()));
        let caps = b.capabilities();
        assert!(caps.supports_exact_version && caps.supports_hold && caps.supports_downgrade);
        assert_eq!(b.scope(), Scope::System);
    }
}
