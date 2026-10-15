//! The scoop backend (Windows, §5.2 rolling-OS tier). A bucket's manifest
//! names one version, so that is the only one listed (`scoop cat <app>`);
//! `scoop install <app>@<version>` installs it, `scoop hold` keeps scoop
//! from updating it, and `scoop list` reports what is installed. A package
//! id may name its bucket (`extras/vscode`); buckets are sources
//! (`scoop bucket add`).
//!
//! User scope (§4.6), scoop's default. scoop is a PowerShell script reached
//! through `scoop.cmd`, which is what bosun runs (Windows process creation
//! does not find `.cmd` shims by their bare name). Unverified on a real
//! Windows host (§11).

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

const BACKEND: &str = "scoop";
const PROGRAM: &str = "scoop.cmd";

/// Builds the scoop backend when `scoop` answers on a Windows host.
pub struct ScoopFactory {
    runner: Arc<dyn CommandRunner>,
}

impl ScoopFactory {
    /// Construct with the command runner the built backend will use.
    pub fn new(runner: Arc<dyn CommandRunner>) -> ScoopFactory {
        ScoopFactory { runner }
    }
}

impl BackendFactory for ScoopFactory {
    fn id(&self) -> BackendId {
        BackendId::new(BACKEND)
    }

    fn meta(&self) -> BackendMeta {
        BackendMeta {
            purl_type: "scoop",
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
            .run(&Command::new(PROGRAM, ["--version".to_string()]))?;
        if !out.success() {
            return Ok(None);
        }
        Ok(Some(Probe {
            backend: self.id(),
            version: first_version(&out.stdout_utf8()),
        }))
    }

    fn build(&self, probe: Probe) -> Backend {
        let backend = ScoopBackend {
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
struct ScoopBackend {
    runner: Arc<dyn CommandRunner>,
    // Recorded for future per-version flag gating (Appendix B #224).
    _version: Version,
}

fn step(parts: &[&str]) -> Step {
    let mut argv = vec![PROGRAM.to_string()];
    argv.extend(parts.iter().map(|s| s.to_string()));
    Step::Run {
        argv,
        env: vec![],
        scope: Scope::User,
        ok_exits: Vec::new(),
    }
}

/// The app's name without its bucket: `extras/vscode` → `vscode`.
fn app_name(id: &str) -> &str {
    id.rsplit('/').next().unwrap_or(id)
}

/// The `version` of a scoop manifest (`scoop cat` prints its JSON).
fn manifest_version(json: &str) -> Option<String> {
    let doc: serde_json::Value = serde_json::from_str(json).ok()?;
    doc.get("version")?.as_str().map(str::to_string)
}

/// The `Version` of `app`'s row in `scoop list` output (`Name Version Source
/// Updated Info`, whitespace-separated, names without spaces).
fn listed_version(stdout: &str, app: &str) -> Option<String> {
    stdout.lines().find_map(|line| {
        let mut words = line.split_whitespace();
        (words.next()? == app).then(|| words.next().map(str::to_string))?
    })
}

impl Index for ScoopBackend {
    fn refresh(&self) -> Result<(), PortError> {
        let out = self
            .runner
            .run(&Command::new(PROGRAM, ["update".to_string()]))?;
        if out.success() {
            Ok(())
        } else {
            Err(PortError::CommandFailed {
                program: PROGRAM.to_string(),
                code: out.code,
                stderr: out.stdout_utf8().into_owned(),
            })
        }
    }

    fn query_available(&self, pkg: &Ref, _target: &Target) -> Result<Vec<Version>, PortError> {
        let out = self.runner.run(&Command::new(
            PROGRAM,
            ["cat".to_string(), pkg.purl.native_id()],
        ))?;
        if !out.success() {
            return Ok(vec![]);
        }
        Ok(manifest_version(&out.stdout_utf8())
            .and_then(|v| Version::parse(v, Scheme::Generic).ok())
            .into_iter()
            .collect())
    }
}

impl Installer for ScoopBackend {
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            supports_exact_version: true,
            hold: HoldSupport::Supported,
            // An older version is not in the bucket; scoop can't go back.
            supports_downgrade: false,
        }
    }

    fn scope(&self) -> Scope {
        Scope::User
    }

    fn ensure_source(&self, source: &Source) -> Result<Steps, PortError> {
        Ok(vec![step(&["bucket", "add", source.id.as_str()])])
    }

    fn install_exact(&self, pkg: &Ref, version: &Version) -> Result<Steps, PortError> {
        let pinned = format!("{}@{}", pkg.purl.native_id(), version.as_str());
        Ok(vec![step(&["install", &pinned])])
    }

    fn verify_installed(&self, pkg: &Ref) -> Result<Installed, PortError> {
        let id = pkg.purl.native_id();
        let app = app_name(&id);
        let out = self.runner.run(&Command::new(
            PROGRAM,
            ["list".to_string(), app.to_string()],
        ))?;
        if !out.success() {
            return Ok(Installed::Absent);
        }
        Ok(match listed_version(&out.stdout_utf8(), app) {
            Some(v) => Installed::Present {
                version: Version::parse(v, Scheme::Generic).ok(),
            },
            None => Installed::Absent,
        })
    }

    fn hold(&self, pkg: &Ref, _version: &Version) -> Result<Steps, PortError> {
        let id = pkg.purl.native_id();
        Ok(vec![step(&["hold", app_name(&id)])])
    }

    fn downgrade(&self, _pkg: &Ref, version: &Version) -> Result<Steps, PortError> {
        Err(PortError::Backend {
            detail: format!(
                "scoop can't downgrade to {}: a bucket holds only its current version",
                version.as_str()
            ),
        })
    }

    fn uninstall(&self, pkg: &Ref) -> Result<Steps, PortError> {
        let id = pkg.purl.native_id();
        Ok(vec![step(&["uninstall", app_name(&id)])])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bosun_core::{Arch, SourceId};
    use bosun_ports::testing::FakeCommandRunner;
    use bosun_ports::Output;

    /// The identity as the resolver builds it: the whole id is the name.
    fn reff(id: &str) -> Ref {
        Ref::new(
            bosun_core::Purl::from_components("scoop", None, id, None, Default::default(), None)
                .unwrap(),
        )
    }

    fn backend(runner: Arc<FakeCommandRunner>) -> ScoopBackend {
        ScoopBackend {
            runner,
            _version: Version::parse("0.5.2", Scheme::Generic).unwrap(),
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

    fn scoop(rest: &[&str]) -> Step {
        let mut argv = vec!["scoop.cmd".to_string()];
        argv.extend(rest.iter().map(|s| s.to_string()));
        Step::Run {
            argv,
            env: vec![],
            scope: Scope::User,
            ok_exits: Vec::new(),
        }
    }

    fn v(s: &str) -> Version {
        Version::parse(s, Scheme::Generic).unwrap()
    }

    const LIST: &str = "\
Installed apps matching 'fd':

Name Version Source Updated             Info
---- ------- ------ -------             ----
fd   10.2.0  main   2026-09-30 10:00:00 Held package
";

    #[test]
    fn the_only_version_is_the_buckets_manifest() {
        let runner = fake(r#"{"version": "10.2.0", "url": "https://x/fd.zip"}"#);
        let b = backend(runner.clone());
        let versions = b
            .query_available(&reff("main/fd"), &"windows/amd64".parse().unwrap())
            .unwrap();
        assert_eq!(versions, vec![v("10.2.0")]);
        assert_eq!(runner.calls()[0].program, "scoop.cmd");
        assert_eq!(runner.calls()[0].args, vec!["cat", "main/fd"]);
        let none = backend(failing())
            .query_available(&reff("nope"), &"windows/amd64".parse().unwrap())
            .unwrap();
        assert!(none.is_empty());
        assert_eq!(manifest_version("not json"), None);
        assert_eq!(manifest_version(r#"{"url": "x"}"#), None);
    }

    #[test]
    fn the_installed_version_is_the_apps_list_row() {
        assert_eq!(listed_version(LIST, "fd"), Some("10.2.0".into()));
        assert_eq!(listed_version(LIST, "f"), None);
        let runner = fake(LIST);
        let b = backend(runner.clone());
        assert_eq!(
            b.verify_installed(&reff("main/fd")).unwrap(),
            Installed::Present {
                version: Some(v("10.2.0"))
            }
        );
        assert_eq!(runner.calls()[0].args, vec!["list", "fd"]);
        assert_eq!(
            backend(fake("Installed apps matching 'jq':\n"))
                .verify_installed(&reff("jq"))
                .unwrap(),
            Installed::Absent
        );
        assert_eq!(
            backend(failing()).verify_installed(&reff("jq")).unwrap(),
            Installed::Absent
        );
    }

    #[test]
    fn steps_name_the_app_and_its_bucket_where_scoop_wants_them() {
        let b = backend(Arc::new(FakeCommandRunner::new()));
        let fd = reff("main/fd");
        assert_eq!(
            b.install_exact(&fd, &v("10.2.0")).unwrap(),
            vec![scoop(&["install", "main/fd@10.2.0"])]
        );
        assert_eq!(
            b.hold(&fd, &v("10.2.0")).unwrap(),
            vec![scoop(&["hold", "fd"])]
        );
        assert_eq!(b.uninstall(&fd).unwrap(), vec![scoop(&["uninstall", "fd"])]);
        assert!(b.downgrade(&fd, &v("9.0.0")).is_err());
        let extras = Source {
            backend: BackendId::new("scoop"),
            id: SourceId::new("extras"),
        };
        assert_eq!(
            b.ensure_source(&extras).unwrap(),
            vec![scoop(&["bucket", "add", "extras"])]
        );
    }

    #[test]
    fn capabilities_scope_and_refresh() {
        let runner = fake("Scoop was updated successfully!");
        let b = backend(runner.clone());
        assert_eq!(b.scope(), Scope::User);
        let c = b.capabilities();
        assert!(c.supports_exact_version && !c.supports_downgrade);
        assert_eq!(c.hold, HoldSupport::Supported);
        b.refresh().unwrap();
        assert_eq!(runner.calls()[0].args, vec!["update"]);
        assert!(matches!(
            backend(failing()).refresh(),
            Err(PortError::CommandFailed { .. })
        ));
    }

    #[test]
    fn registers_on_windows_when_scoop_answers() {
        let f = ScoopFactory::new(Arc::new(FakeCommandRunner::new()));
        let host = |os, runner: &FakeCommandRunner| -> Option<Probe> {
            f.probe(&HostInfo {
                os,
                arch: Arch::Amd64,
                distro: None,
                runner,
            })
            .unwrap()
        };
        let answering = FakeCommandRunner::new().with_stdout("Current Scoop version:\nv0.5.2");
        assert_eq!(
            host(Os::Windows, &answering).unwrap().version.as_str(),
            "0.5.2"
        );
        assert_eq!(answering.calls()[0].program, "scoop.cmd");
        assert!(host(Os::Linux, &FakeCommandRunner::new()).is_none());
        let absent = FakeCommandRunner::new().with_output(Output {
            code: Some(1),
            stdout: Vec::new(),
            stderr: Vec::new(),
        });
        assert!(host(Os::Windows, &absent).is_none());
        assert_eq!(f.id().as_str(), "scoop");
        assert_eq!(f.meta().purl_type, "scoop");
    }
}
