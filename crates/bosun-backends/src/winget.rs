//! The winget backend (Windows Package Manager, §5.2 versioned-OS tier):
//! `winget install --id <id> --exact --version <v>`, held with a gating pin
//! (`winget pin add --version <v>`, which keeps winget from moving it).
//! Versions come from `winget show --versions`; the installed one from
//! `winget list`, the word after the id in its row.
//!
//! User scope (§4.6): winget installs per the package's own installer; one
//! that installs machine-wide raises its own UAC prompt, so bosun runs
//! unelevated. Every call is non-interactive and accepts the source and
//! package agreements, which `bosun install`'s confirmation stands for.
//! Unverified on a real Windows host (§11).

use std::sync::Arc;

use bosun_core::{
    BackendId, Capabilities, HoldSupport, Installed, Os, Ref, Scheme, Scope, Source, Target,
    Version,
};
use bosun_ports::{
    Backend, BackendFactory, BackendMeta, Command, CommandRunner, HostInfo, Index, Installer,
    PortError, Probe, Step, Steps,
};

const BACKEND: &str = "winget";

/// Builds the winget backend when `winget` answers on a Windows host.
pub struct WingetFactory {
    runner: Arc<dyn CommandRunner>,
}

impl WingetFactory {
    /// Construct with the command runner the built backend will use.
    pub fn new(runner: Arc<dyn CommandRunner>) -> WingetFactory {
        WingetFactory { runner }
    }
}

impl BackendFactory for WingetFactory {
    fn id(&self) -> BackendId {
        BackendId::new(BACKEND)
    }

    fn meta(&self) -> BackendMeta {
        BackendMeta {
            // No registered purl type; the backend id stands in (§5.6).
            purl_type: "winget",
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
            .run(&Command::new("winget", ["--version".to_string()]))?;
        if !out.success() {
            return Ok(None);
        }
        Ok(Some(Probe {
            backend: self.id(),
            version: first_version(&out.stdout_utf8()),
        }))
    }

    fn build(&self, probe: Probe) -> Backend {
        let backend = WingetBackend {
            runner: self.runner.clone(),
            _version: probe.version,
        };
        Backend {
            index: Box::new(backend.clone()),
            installer: Box::new(backend),
        }
    }
}

/// The first word that reads as a version, a leading `v` dropped
/// (`v1.9.25200` → `1.9.25200`), else `0`.
pub(crate) fn first_version(stdout: &str) -> Version {
    stdout
        .split_whitespace()
        .map(|w| w.strip_prefix('v').unwrap_or(w))
        .find(|w| w.starts_with(|c: char| c.is_ascii_digit()))
        .and_then(|w| Version::parse(w, Scheme::Generic).ok())
        .unwrap_or_else(|| Version::parse("0", Scheme::Generic).expect("0 is a version"))
}

#[derive(Clone)]
struct WingetBackend {
    runner: Arc<dyn CommandRunner>,
    // Recorded for future per-version flag gating (Appendix B #224).
    _version: Version,
}

/// The flags every winget call takes, so it never prompts.
const QUIET: &[&str] = &["--accept-source-agreements", "--disable-interactivity"];

fn args(parts: &[&str], quiet: bool) -> Vec<String> {
    let mut v: Vec<String> = parts.iter().map(|s| s.to_string()).collect();
    if quiet {
        v.extend(QUIET.iter().map(|s| s.to_string()));
    }
    v
}

fn step(parts: &[&str]) -> Step {
    let mut argv = vec!["winget".to_string()];
    argv.extend(args(parts, true));
    Step::Run {
        argv,
        env: vec![],
        scope: Scope::User,
        ok_exits: Vec::new(),
    }
}

/// The versions in `winget show --versions` output: the lines after the
/// `-------` rule under the `Version` heading.
fn versions_listed(stdout: &str) -> Vec<Version> {
    stdout
        .lines()
        .skip_while(|l| !l.trim_start().starts_with("---"))
        .skip(1)
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .filter_map(|l| Version::parse(l, Scheme::Generic).ok())
        .collect()
}

/// The version in `id`'s row of a `winget list` table (`Name  Id  Version
/// Available  Source`): the word after the id. A name may hold spaces, but it
/// comes before the id, and an id and a version never do.
fn installed_version(stdout: &str, id: &str) -> Option<String> {
    stdout.lines().find_map(|line| {
        let mut words = line.split_whitespace().skip_while(|w| *w != id);
        words.next()?;
        words.next().map(str::to_string)
    })
}

impl Index for WingetBackend {
    fn refresh(&self) -> Result<(), PortError> {
        let out = self
            .runner
            .run(&Command::new("winget", args(&["source", "update"], false)))?;
        if out.success() {
            Ok(())
        } else {
            Err(PortError::CommandFailed {
                program: "winget".to_string(),
                code: out.code,
                stderr: out.stdout_utf8().into_owned(),
            })
        }
    }

    fn query_available(&self, pkg: &Ref, _target: &Target) -> Result<Vec<Version>, PortError> {
        let id = pkg.purl.native_id();
        let out = self.runner.run(&Command::new(
            "winget",
            args(&["show", "--id", &id, "--exact", "--versions"], true),
        ))?;
        if !out.success() {
            return Ok(vec![]); // unknown id → no versions
        }
        Ok(versions_listed(&out.stdout_utf8()))
    }
}

impl Installer for WingetBackend {
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            supports_exact_version: true,
            hold: HoldSupport::Supported,
            // `--force` lets winget install a lower version over a higher one.
            supports_downgrade: true,
        }
    }

    fn scope(&self) -> Scope {
        Scope::User
    }

    fn ensure_source(&self, _source: &Source) -> Result<Steps, PortError> {
        // A winget source needs its URL and type, which a source id doesn't
        // carry; sources (winget, msstore by default) are set up outside bosun.
        Ok(vec![])
    }

    fn install_exact(&self, pkg: &Ref, version: &Version) -> Result<Steps, PortError> {
        let id = pkg.purl.native_id();
        Ok(vec![step(&[
            "install",
            "--id",
            &id,
            "--exact",
            "--version",
            version.as_str(),
            "--silent",
            "--accept-package-agreements",
        ])])
    }

    fn verify_installed(&self, pkg: &Ref) -> Result<Installed, PortError> {
        let id = pkg.purl.native_id();
        let out = self.runner.run(&Command::new(
            "winget",
            args(&["list", "--id", &id, "--exact"], true),
        ))?;
        if !out.success() {
            return Ok(Installed::Absent); // "No installed package found"
        }
        Ok(match installed_version(&out.stdout_utf8(), &id) {
            Some(v) => Installed::Present {
                version: Version::parse(&v, Scheme::Generic).ok(),
            },
            None => Installed::Absent,
        })
    }

    fn hold(&self, pkg: &Ref, version: &Version) -> Result<Steps, PortError> {
        let id = pkg.purl.native_id();
        // A gating pin at exactly this version; --force replaces an older pin.
        Ok(vec![step(&[
            "pin",
            "add",
            "--id",
            &id,
            "--exact",
            "--version",
            version.as_str(),
            "--force",
        ])])
    }

    fn downgrade(&self, pkg: &Ref, version: &Version) -> Result<Steps, PortError> {
        let id = pkg.purl.native_id();
        Ok(vec![step(&[
            "install",
            "--id",
            &id,
            "--exact",
            "--version",
            version.as_str(),
            "--silent",
            "--accept-package-agreements",
            "--force",
        ])])
    }

    fn uninstall(&self, pkg: &Ref) -> Result<Steps, PortError> {
        let id = pkg.purl.native_id();
        // The pin is left in winget's list: `pin remove` fails when there is
        // none, and a failed step would leave the package recorded.
        Ok(vec![step(&[
            "uninstall",
            "--id",
            &id,
            "--exact",
            "--silent",
        ])])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bosun_core::Arch;
    use bosun_ports::testing::FakeCommandRunner;
    use bosun_ports::Output;

    fn reff(id: &str) -> Ref {
        Ref::new(format!("pkg:winget/{id}").parse().unwrap())
    }

    fn backend(runner: FakeCommandRunner) -> WingetBackend {
        backend_over(Arc::new(runner))
    }

    fn backend_over(runner: Arc<FakeCommandRunner>) -> WingetBackend {
        WingetBackend {
            runner,
            _version: Version::parse("1.9.25200", Scheme::Generic).unwrap(),
        }
    }

    fn v(s: &str) -> Version {
        Version::parse(s, Scheme::Generic).unwrap()
    }

    fn winget(rest: &[&str]) -> Step {
        let mut argv = vec!["winget".to_string()];
        argv.extend(rest.iter().map(|s| s.to_string()));
        argv.extend(["--accept-source-agreements", "--disable-interactivity"].map(String::from));
        Step::Run {
            argv,
            env: vec![],
            scope: Scope::User,
            ok_exits: Vec::new(),
        }
    }

    const SHOW_VERSIONS: &str = "\
Found fd [sharkdp.fd]
Version
-------
10.2.0
10.1.0
9.0.0
";

    const LIST: &str = "\
Name   Id          Version  Available Source
---------------------------------------------
fd     sharkdp.fd  10.1.0   10.2.0    winget
";

    #[test]
    fn versions_are_the_lines_under_the_rule() {
        let got: Vec<String> = versions_listed(SHOW_VERSIONS)
            .iter()
            .map(|v| v.as_str().to_string())
            .collect();
        assert_eq!(got, vec!["10.2.0", "10.1.0", "9.0.0"]);
        assert!(versions_listed("No package found matching input criteria.").is_empty());
    }

    #[test]
    fn query_available_asks_winget_show_for_the_exact_id() {
        let runner = Arc::new(FakeCommandRunner::new().with_stdout(SHOW_VERSIONS));
        let b = backend_over(runner.clone());
        let versions = b
            .query_available(&reff("sharkdp.fd"), &"windows/amd64".parse().unwrap())
            .unwrap();
        assert_eq!(versions.len(), 3);
        let call = &runner.calls()[0];
        assert_eq!(call.program, "winget");
        assert_eq!(
            call.args,
            args(
                &["show", "--id", "sharkdp.fd", "--exact", "--versions"],
                true
            )
        );
    }

    #[test]
    fn an_unknown_id_has_no_versions() {
        let runner = FakeCommandRunner::new().with_output(Output {
            code: Some(-1978335212),
            stdout: b"No package found matching input criteria.".to_vec(),
            stderr: Vec::new(),
        });
        let versions = backend(runner)
            .query_available(&reff("nope"), &"windows/amd64".parse().unwrap())
            .unwrap();
        assert!(versions.is_empty());
    }

    #[test]
    fn the_installed_version_is_read_from_the_list_table_by_column() {
        assert_eq!(installed_version(LIST, "sharkdp.fd"), Some("10.1.0".into()));
        assert_eq!(installed_version(LIST, "other.id"), None);
        let no_available = "\
Name  Id           Version Source
---------------------------------
jq    jqlang.jq    1.7.1   winget
";
        assert_eq!(
            installed_version(no_available, "jqlang.jq"),
            Some("1.7.1".into())
        );
        assert_eq!(installed_version("", "jqlang.jq"), None);
        let spaced = "\
Name                Id                         Version Source
-------------------------------------------------------------
Visual Studio Code  Microsoft.VisualStudioCode 1.105.0 winget
";
        assert_eq!(
            installed_version(spaced, "Microsoft.VisualStudioCode"),
            Some("1.105.0".into())
        );
        // A row with the id last has no version to read.
        assert_eq!(installed_version("fd sharkdp.fd", "sharkdp.fd"), None);
    }

    #[test]
    fn verify_reports_the_listed_version_or_absent() {
        let present = backend(FakeCommandRunner::new().with_stdout(LIST));
        assert_eq!(
            present.verify_installed(&reff("sharkdp.fd")).unwrap(),
            Installed::Present {
                version: Some(v("10.1.0"))
            }
        );
        let missing = backend(FakeCommandRunner::new().with_output(Output {
            code: Some(-1978335212),
            stdout: b"No installed package found matching input criteria.".to_vec(),
            stderr: Vec::new(),
        }));
        assert_eq!(
            missing.verify_installed(&reff("sharkdp.fd")).unwrap(),
            Installed::Absent
        );
        let other = backend(FakeCommandRunner::new().with_stdout(LIST));
        assert_eq!(
            other.verify_installed(&reff("not.listed")).unwrap(),
            Installed::Absent
        );
    }

    #[test]
    fn install_hold_downgrade_and_uninstall_steps() {
        let b = backend(FakeCommandRunner::new());
        let fd = reff("sharkdp.fd");
        assert_eq!(
            b.install_exact(&fd, &v("10.2.0")).unwrap(),
            vec![winget(&[
                "install",
                "--id",
                "sharkdp.fd",
                "--exact",
                "--version",
                "10.2.0",
                "--silent",
                "--accept-package-agreements"
            ])]
        );
        assert_eq!(
            b.hold(&fd, &v("10.2.0")).unwrap(),
            vec![winget(&[
                "pin",
                "add",
                "--id",
                "sharkdp.fd",
                "--exact",
                "--version",
                "10.2.0",
                "--force"
            ])]
        );
        assert_eq!(
            b.downgrade(&fd, &v("9.0.0")).unwrap(),
            vec![winget(&[
                "install",
                "--id",
                "sharkdp.fd",
                "--exact",
                "--version",
                "9.0.0",
                "--silent",
                "--accept-package-agreements",
                "--force"
            ])]
        );
        assert_eq!(
            b.uninstall(&fd).unwrap(),
            vec![winget(&[
                "uninstall",
                "--id",
                "sharkdp.fd",
                "--exact",
                "--silent"
            ])]
        );
        let src = Source {
            backend: BackendId::new("winget"),
            id: bosun_core::SourceId::new("msstore"),
        };
        assert!(b.ensure_source(&src).unwrap().is_empty());
    }

    #[test]
    fn keeps_no_record_of_what_the_user_installed() {
        // `winget list` mixes what the user asked for with what Windows and
        // other installers put there; nothing tells them apart.
        let b = backend(FakeCommandRunner::new());
        assert_eq!(
            b.user_installs().unwrap(),
            bosun_ports::UserInstalls::Unsupported
        );
    }

    #[test]
    fn capabilities_and_scope() {
        let b = backend(FakeCommandRunner::new());
        assert_eq!(b.scope(), Scope::User);
        let c = b.capabilities();
        assert!(c.supports_exact_version && c.supports_downgrade);
        assert_eq!(c.hold, HoldSupport::Supported);
    }

    #[test]
    fn refresh_updates_the_sources_and_reports_a_failure() {
        let runner = Arc::new(FakeCommandRunner::new().with_stdout("Done"));
        backend_over(runner.clone()).refresh().unwrap();
        assert_eq!(runner.calls()[0].args, vec!["source", "update"]);
        let failing = backend(FakeCommandRunner::new().with_output(Output {
            code: Some(1),
            stdout: b"failed".to_vec(),
            stderr: Vec::new(),
        }));
        assert!(matches!(
            failing.refresh(),
            Err(PortError::CommandFailed { .. })
        ));
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
    fn registers_on_windows_when_winget_answers() {
        let f = WingetFactory::new(Arc::new(FakeCommandRunner::new()));
        let answering = FakeCommandRunner::new().with_stdout("v1.9.25200");
        let probe = f.probe(&host(Os::Windows, &answering)).unwrap().unwrap();
        assert_eq!(probe.version.as_str(), "1.9.25200");
        assert_eq!(answering.calls()[0].program, "winget");
        let unused = FakeCommandRunner::new();
        assert!(f.probe(&host(Os::Linux, &unused)).unwrap().is_none());
        let absent = FakeCommandRunner::new().with_output(Output {
            code: Some(1),
            stdout: Vec::new(),
            stderr: Vec::new(),
        });
        assert!(f.probe(&host(Os::Windows, &absent)).unwrap().is_none());
        assert_eq!(f.id().as_str(), "winget");
        assert_eq!(f.meta().purl_type, "winget");
    }

    #[test]
    fn first_version_drops_a_v_and_defaults_to_zero() {
        assert_eq!(first_version("v1.9.25200").as_str(), "1.9.25200");
        assert_eq!(
            first_version("Current Scoop version:\nv0.5.2 - Released").as_str(),
            "0.5.2"
        );
        assert_eq!(first_version("nothing").as_str(), "0");
    }
}
