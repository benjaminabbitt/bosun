//! The Chocolatey backend (Windows, §5.2 versioned-OS tier): `choco install
//! <id> --version <v>`, held with `choco pin add`, downgraded with
//! `--allow-downgrade`. Versions come from `choco search --exact
//! --all-versions` and the installed one from `choco list --exact` (local only
//! since Chocolatey 2), both with `--limit-output`, which prints `id|version`
//! lines.
//!
//! System scope (§4.6): Chocolatey installs machine-wide and needs an
//! Administrator terminal. Unverified on a real Windows host (§11).

use std::sync::Arc;

use bosun_core::{
    BackendId, Capabilities, HoldSupport, Installed, Os, Ref, Scheme, Scope, Source, Target,
    Version,
};
use bosun_ports::{
    Backend, BackendFactory, BackendMeta, Command, CommandRunner, HostInfo, Index, Installer,
    PortError, Probe, Step, Steps,
};

use crate::winget::first_version;

const BACKEND: &str = "choco";

/// Builds the Chocolatey backend when `choco` answers on a Windows host.
pub struct ChocoFactory {
    runner: Arc<dyn CommandRunner>,
}

impl ChocoFactory {
    /// Construct with the command runner the built backend will use.
    pub fn new(runner: Arc<dyn CommandRunner>) -> ChocoFactory {
        ChocoFactory { runner }
    }
}

impl BackendFactory for ChocoFactory {
    fn id(&self) -> BackendId {
        BackendId::new(BACKEND)
    }

    fn meta(&self) -> BackendMeta {
        BackendMeta {
            purl_type: "choco",
            vers_type: "generic",
            platform_independent: false,
            option_keys: &[],
        }
    }

    fn probe(&self, host: &HostInfo) -> Result<Option<Probe>, PortError> {
        if host.os != Os::Windows {
            return Ok(None);
        }
        let out = host
            .runner
            .run(&Command::new("choco", ["--version".to_string()]))?;
        if !out.success() {
            return Ok(None);
        }
        Ok(Some(Probe {
            backend: self.id(),
            version: first_version(&out.stdout_utf8()),
        }))
    }

    fn build(&self, probe: Probe) -> Backend {
        let backend = ChocoBackend {
            runner: self.runner.clone(),
            _version: probe.version,
        };
        Backend {
            index: Box::new(backend.clone()),
            installer: Box::new(backend),
        }
    }
}

#[derive(Clone)]
struct ChocoBackend {
    runner: Arc<dyn CommandRunner>,
    // Recorded for future per-version flag gating (Appendix B #224).
    _version: Version,
}

fn strings(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|s| s.to_string()).collect()
}

fn step(parts: &[&str]) -> Step {
    let mut argv = vec!["choco".to_string()];
    argv.extend(strings(parts));
    Step::Run {
        argv,
        env: vec![],
        scope: Scope::System,
        ok_exits: Vec::new(),
    }
}

/// The versions of `id` in `--limit-output` lines (`id|version`), matched on
/// the id without regard to case, as Chocolatey ids are.
fn versions_of(stdout: &str, id: &str) -> Vec<String> {
    stdout
        .lines()
        .filter_map(|l| l.trim().split_once('|'))
        .filter(|(name, _)| name.eq_ignore_ascii_case(id))
        .map(|(_, version)| version.trim().to_string())
        .filter(|v| !v.is_empty())
        .collect()
}

impl Index for ChocoBackend {
    fn refresh(&self) -> Result<(), PortError> {
        Ok(()) // every search asks the feed
    }

    fn query_available(&self, pkg: &Ref, _target: &Target) -> Result<Vec<Version>, PortError> {
        let id = pkg.purl.native_id();
        let out = self.runner.run(&Command::new(
            "choco",
            strings(&["search", &id, "--exact", "--all-versions", "--limit-output"]),
        ))?;
        if !out.success() {
            return Ok(vec![]);
        }
        Ok(versions_of(&out.stdout_utf8(), &id)
            .into_iter()
            .filter_map(|v| Version::parse(v, Scheme::Generic).ok())
            .collect())
    }
}

impl Installer for ChocoBackend {
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            supports_exact_version: true,
            hold: HoldSupport::Supported,
            supports_downgrade: true,
        }
    }

    fn scope(&self) -> Scope {
        Scope::System
    }

    fn ensure_source(&self, source: &Source) -> Result<Steps, PortError> {
        // A source id is taken as the feed URL, named after itself.
        let id = source.id.as_str();
        Ok(vec![step(&[
            "source",
            "add",
            &format!("--name={id}"),
            &format!("--source={id}"),
        ])])
    }

    fn install_exact(&self, pkg: &Ref, version: &Version) -> Result<Steps, PortError> {
        let id = pkg.purl.native_id();
        Ok(vec![step(&[
            "install",
            &id,
            "--version",
            version.as_str(),
            "-y",
            "--no-progress",
        ])])
    }

    fn verify_installed(&self, pkg: &Ref) -> Result<Installed, PortError> {
        let id = pkg.purl.native_id();
        let out = self.runner.run(&Command::new(
            "choco",
            strings(&["list", &id, "--exact", "--limit-output"]),
        ))?;
        if !out.success() {
            return Ok(Installed::Absent);
        }
        Ok(
            match versions_of(&out.stdout_utf8(), &id).into_iter().next() {
                Some(v) => Installed::Present {
                    version: Version::parse(v, Scheme::Generic).ok(),
                },
                None => Installed::Absent,
            },
        )
    }

    fn hold(&self, pkg: &Ref, version: &Version) -> Result<Steps, PortError> {
        let id = pkg.purl.native_id();
        Ok(vec![step(&[
            "pin",
            "add",
            &format!("--name={id}"),
            &format!("--version={}", version.as_str()),
        ])])
    }

    fn unhold(&self, pkg: &Ref) -> Result<Steps, PortError> {
        let id = pkg.purl.native_id();
        Ok(vec![step(&["pin", "remove", &format!("--name={id}")])])
    }

    fn downgrade(&self, pkg: &Ref, version: &Version) -> Result<Steps, PortError> {
        let id = pkg.purl.native_id();
        Ok(vec![step(&[
            "install",
            &id,
            "--version",
            version.as_str(),
            "--allow-downgrade",
            "-y",
            "--no-progress",
        ])])
    }

    fn uninstall(&self, pkg: &Ref) -> Result<Steps, PortError> {
        let id = pkg.purl.native_id();
        Ok(vec![step(&["uninstall", &id, "-y", "--no-progress"])])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bosun_core::{Arch, SourceId};
    use bosun_ports::testing::FakeCommandRunner;
    use bosun_ports::Output;

    fn reff(id: &str) -> Ref {
        Ref::new(format!("pkg:choco/{id}").parse().unwrap())
    }

    fn backend(runner: Arc<FakeCommandRunner>) -> ChocoBackend {
        ChocoBackend {
            runner,
            _version: Version::parse("2.4.3", Scheme::Generic).unwrap(),
        }
    }

    fn fake(stdout: &str) -> Arc<FakeCommandRunner> {
        Arc::new(FakeCommandRunner::new().with_stdout(stdout))
    }

    fn failing() -> Arc<FakeCommandRunner> {
        Arc::new(FakeCommandRunner::new().with_output(Output {
            code: Some(1),
            stdout: Vec::new(),
            stderr: Vec::new(),
        }))
    }

    /// Owned strings, written out here rather than through the code's own
    /// helper, so a broken helper can't hide behind its test.
    fn owned(parts: &[&str]) -> Vec<String> {
        let mut out = Vec::new();
        for p in parts {
            out.push(String::from(*p));
        }
        out
    }

    fn choco(rest: &[&str]) -> Step {
        let mut argv = vec!["choco".to_string()];
        argv.extend(owned(rest));
        Step::Run {
            argv,
            env: vec![],
            scope: Scope::System,
            ok_exits: Vec::new(),
        }
    }

    fn v(s: &str) -> Version {
        Version::parse(s, Scheme::Generic).unwrap()
    }

    #[test]
    fn versions_come_from_limit_output_lines_for_that_id() {
        let out = "jq|1.8.1\njq|1.7.1\njq-extra|9.9\n\n";
        assert_eq!(versions_of(out, "jq"), vec!["1.8.1", "1.7.1"]);
        assert_eq!(versions_of("JQ|1.8.1", "jq"), vec!["1.8.1"]);
        assert!(versions_of("Chocolatey v2.4.3\n0 packages found.", "jq").is_empty());
        let runner = fake(out);
        let b = backend(runner.clone());
        let got = b
            .query_available(&reff("jq"), &"windows/amd64".parse().unwrap())
            .unwrap();
        assert_eq!(got, vec![v("1.8.1"), v("1.7.1")]);
        assert_eq!(
            runner.calls()[0].args,
            strings(&[
                "search",
                "jq",
                "--exact",
                "--all-versions",
                "--limit-output"
            ])
        );
        assert!(backend(failing())
            .query_available(&reff("jq"), &"windows/amd64".parse().unwrap())
            .unwrap()
            .is_empty());
    }

    #[test]
    fn the_installed_version_is_the_local_list_line() {
        let runner = fake("jq|1.7.1\n");
        let b = backend(runner.clone());
        assert_eq!(
            b.verify_installed(&reff("jq")).unwrap(),
            Installed::Present {
                version: Some(v("1.7.1"))
            }
        );
        assert_eq!(
            runner.calls()[0].args,
            owned(&["list", "jq", "--exact", "--limit-output"])
        );
        assert_eq!(
            backend(fake("")).verify_installed(&reff("jq")).unwrap(),
            Installed::Absent
        );
        assert_eq!(
            backend(failing()).verify_installed(&reff("jq")).unwrap(),
            Installed::Absent
        );
    }

    #[test]
    fn install_hold_downgrade_uninstall_and_source_steps() {
        let b = backend(Arc::new(FakeCommandRunner::new()));
        let jq = reff("jq");
        assert_eq!(
            b.install_exact(&jq, &v("1.8.1")).unwrap(),
            vec![choco(&[
                "install",
                "jq",
                "--version",
                "1.8.1",
                "-y",
                "--no-progress"
            ])]
        );
        assert_eq!(
            b.hold(&jq, &v("1.8.1")).unwrap(),
            vec![choco(&["pin", "add", "--name=jq", "--version=1.8.1"])]
        );
        assert_eq!(
            b.unhold(&jq).unwrap(),
            vec![choco(&["pin", "remove", "--name=jq"])]
        );
        assert_eq!(
            b.downgrade(&jq, &v("1.7.1")).unwrap(),
            vec![choco(&[
                "install",
                "jq",
                "--version",
                "1.7.1",
                "--allow-downgrade",
                "-y",
                "--no-progress"
            ])]
        );
        assert_eq!(
            b.uninstall(&jq).unwrap(),
            vec![choco(&["uninstall", "jq", "-y", "--no-progress"])]
        );
        let feed = Source {
            backend: BackendId::new("choco"),
            id: SourceId::new("https://feed.example/api/v2"),
        };
        assert_eq!(
            b.ensure_source(&feed).unwrap(),
            vec![choco(&[
                "source",
                "add",
                "--name=https://feed.example/api/v2",
                "--source=https://feed.example/api/v2"
            ])]
        );
    }

    #[test]
    fn machine_scope_and_capabilities() {
        let b = backend(Arc::new(FakeCommandRunner::new()));
        assert_eq!(b.scope(), Scope::System);
        let c = b.capabilities();
        assert!(c.supports_exact_version && c.supports_downgrade);
        assert_eq!(c.hold, HoldSupport::Supported);
        b.refresh().unwrap();
    }

    #[test]
    fn registers_on_windows_when_choco_answers() {
        let f = ChocoFactory::new(Arc::new(FakeCommandRunner::new()));
        let host = |os, runner: &FakeCommandRunner| -> Option<Probe> {
            f.probe(&HostInfo {
                os,
                arch: Arch::Amd64,
                distro: None,
                runner,
            })
            .unwrap()
        };
        let answering = FakeCommandRunner::new().with_stdout("2.4.3");
        assert_eq!(
            host(Os::Windows, &answering).unwrap().version.as_str(),
            "2.4.3"
        );
        assert_eq!(answering.calls()[0].program, "choco");
        assert!(host(Os::Linux, &FakeCommandRunner::new()).is_none());
        let absent = FakeCommandRunner::new().with_output(Output {
            code: Some(1),
            stdout: Vec::new(),
            stderr: Vec::new(),
        });
        assert!(host(Os::Windows, &absent).is_none());
        assert_eq!(f.id().as_str(), "choco");
        assert_eq!(f.meta().purl_type, "choco");
    }
}
