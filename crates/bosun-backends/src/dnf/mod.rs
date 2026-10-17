//! The dnf backend (Fedora and the RHEL family, §5.2 versioned-OS tier).
//! Mutations are described as step lists (`dnf install -y pkg-evr`,
//! `dnf versionlock add`, …) a host executor runs; `verify_installed` (`rpm -q`)
//! and `query_available` (`dnf repoquery`) run directly via the
//! [`CommandRunner`]. dnf is Linux-only and system-scope.
//!
//! dnf4 (RHEL/Rocky 9) and dnf5 (Fedora 41+) take the same commands here. The
//! hold is `dnf versionlock`: built into dnf5, a plugin for dnf4
//! (`python3-dnf-plugin-versionlock`, providing `dnf-command(versionlock)`).
//! Without the plugin the backend reports holds unsupported, so a plan warns
//! instead of failing at the hold step. bosun locks only what an exact spec
//! pins; a lock blocks any other version of the package, so every version
//! change and removal releases it first.

use std::sync::{Arc, OnceLock};

use bosun_core::{
    Arch, BackendId, Capabilities, HoldSupport, Installed, Os, Ref, Scheme, Scope, Source, Target,
    Version, Want,
};
use bosun_ports::{
    Backend, BackendFactory, BackendMeta, Command, CommandRunner, HostInfo, Index, Installer,
    PortError, Probe, Selection, Step, Steps,
};
use tracing::{debug, warn};

mod priority;

use priority::{Build, Priorities, Ranking};

use crate::logmsg;

const BACKEND: &str = "dnf";

/// One available build per line, with the repository offering it, for
/// [`parse_repoquery`]. dnf5 needs the newline; dnf4 adds its own as well,
/// leaving blank lines between records.
const REPOQUERY_FORMAT: &str = "%{name}|%{epoch}|%{version}|%{release}|%{arch}|%{repoid}\\n";

/// The installed `[epoch:]version-release`, the epoch only when it is set.
const RPM_QUERY_FORMAT: &str = "%|EPOCH?{%{EPOCH}:}:{}|%{VERSION}-%{RELEASE}\\n";

/// The capability the dnf4 versionlock plugin provides.
const VERSIONLOCK_COMMAND: &str = "dnf-command(versionlock)";

/// Builds the dnf backend when `dnf` is present (§5.3).
pub struct DnfFactory {
    runner: Arc<dyn CommandRunner>,
}

impl DnfFactory {
    /// Construct with the command runner the built backend will use.
    pub fn new(runner: Arc<dyn CommandRunner>) -> DnfFactory {
        DnfFactory { runner }
    }

    /// Whether `dnf versionlock` exists: always in dnf5, and in dnf4 when a
    /// package provides the command (the plugin).
    fn has_versionlock(&self, dnf: &Version) -> bool {
        if is_dnf5(dnf) {
            return true;
        }
        let query = Command::new("rpm", argv(&["-q", "--whatprovides", VERSIONLOCK_COMMAND]));
        let present = self.runner.run(&query).is_ok_and(|out| out.success());
        if !present {
            warn!(message = logmsg::DNF_NO_VERSIONLOCK, dnf = %dnf);
        }
        present
    }
}

impl BackendFactory for DnfFactory {
    fn meta(&self) -> BackendMeta {
        BackendMeta {
            purl_type: "rpm",
            vers_type: "rpm",
            platform_independent: false,
            option_keys: &[],
        }
    }

    fn id(&self) -> BackendId {
        BackendId::new(BACKEND)
    }

    fn probe(&self, host: &HostInfo) -> Result<Option<Probe>, PortError> {
        if host.os != Os::Linux {
            return Ok(None); // dnf is Linux-only
        }
        let out = host
            .runner
            .run(&Command::new("dnf", ["--version".to_string()]))?;
        if !out.success() {
            return Ok(None); // dnf not on PATH / not usable
        }
        Ok(Some(Probe {
            backend: BackendId::new(BACKEND),
            version: parse_dnf_version(&out.stdout_utf8())?,
        }))
    }

    fn build(&self, probe: Probe) -> Backend {
        let backend = DnfBackend {
            runner: self.runner.clone(),
            versionlock: self.has_versionlock(&probe.version),
            dnf5: is_dnf5(&probe.version),
            priorities: Arc::new(OnceLock::new()),
        };
        Backend {
            index: Box::new(backend.clone()),
            installer: Box::new(backend),
        }
    }
}

/// Whether the probed dnf is dnf5.
fn is_dnf5(dnf: &Version) -> bool {
    *dnf >= Version::parse("5", Scheme::Rpm).expect("5 is a version")
}

/// `dnf5 version 5.2.17.0` (dnf5) or `4.14.0` (dnf4): the last word of the
/// first line.
fn parse_dnf_version(stdout: &str) -> Result<Version, PortError> {
    let word = stdout
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().last())
        .ok_or_else(|| PortError::CommandOutput {
            detail: format!("could not read dnf version from {stdout:?}"),
        })?;
    Version::parse(word, Scheme::Rpm).map_err(|e| PortError::CommandOutput {
        detail: format!("dnf version {word:?}: {e}"),
    })
}

#[derive(Clone)]
struct DnfBackend {
    runner: Arc<dyn CommandRunner>,
    /// Whether `dnf versionlock` is available (dnf5, or dnf4 with the plugin).
    versionlock: bool,
    /// Whether this is dnf5, which prints its repository configuration.
    dnf5: bool,
    /// The enabled repositories' priorities, asked of dnf on first use.
    priorities: Arc<OnceLock<Result<Priorities, PortError>>>,
}

fn argv(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|s| s.to_string()).collect()
}

/// A system-scope `dnf …` step. dnf needs no environment to stay quiet; `-y`
/// on each mutating command answers its one prompt.
fn dnf(args: &[&str]) -> Step {
    let mut full = vec!["dnf"];
    full.extend_from_slice(args);
    Step::Run {
        argv: argv(&full),
        env: Vec::new(),
        scope: Scope::System,
        ok_exits: Vec::new(),
    }
}

/// `<name>-<[epoch:]version-release>`, how dnf names one exact build.
fn nevr(pkg: &Ref, version: &Version) -> String {
    format!("{}-{}", pkg.purl.native_id(), version.as_str())
}

/// rpm's name for an architecture (`uname -m`).
fn rpm_arch(arch: Arch) -> &'static str {
    match arch {
        Arch::Amd64 => "x86_64",
        Arch::Arm64 => "aarch64",
    }
}

/// The available builds in `dnf repoquery` output (one
/// `name|epoch|version|release|arch|repoid` per line) for exactly `name`,
/// built for `arch` or `noarch` (any arch when `None`), each version as
/// `[epoch:]version-release` with the repository offering it.
fn parse_repoquery(stdout: &str, name: &str, arch: Option<&str>) -> Vec<Build> {
    let mut builds: Vec<Build> = Vec::new();
    for line in stdout.lines() {
        let fields: Vec<&str> = line.split('|').collect();
        let [n, epoch, version, release, a, repo] = fields[..] else {
            continue;
        };
        if n != name || !arch.is_none_or(|want| a == want || a == "noarch") {
            continue;
        }
        let evr = match epoch {
            "" | "0" => format!("{version}-{release}"),
            e => format!("{e}:{version}-{release}"),
        };
        if let Ok(version) = Version::parse(evr, Scheme::Rpm) {
            let build = Build {
                version,
                repo: repo.to_string(),
            };
            if !builds.contains(&build) {
                builds.push(build);
            }
        }
    }
    builds
}

impl DnfBackend {
    /// Steps that release `pkg`'s versionlock, so another version can land or
    /// the package can go; `delete` exits 0 when there is none. Empty without
    /// versionlock.
    fn unlock(&self, pkg: &Ref) -> Steps {
        if self.versionlock {
            vec![dnf(&["versionlock", "delete", &pkg.purl.native_id()])]
        } else {
            Vec::new()
        }
    }

    fn failed(out: &bosun_ports::Output) -> PortError {
        PortError::CommandFailed {
            program: "dnf".to_string(),
            code: out.code,
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        }
    }

    /// The builds of `pkg` the enabled repositories offer for `target`.
    fn builds(&self, pkg: &Ref, target: &Target) -> Result<Vec<Build>, PortError> {
        let name = pkg.purl.native_id();
        let out = self.runner.run(&Command::new(
            "dnf",
            argv(&[
                "repoquery",
                "--available",
                "--queryformat",
                REPOQUERY_FORMAT,
                &name,
            ]),
        ))?;
        // An unknown package is an empty answer with exit 0; a failure is a
        // broken repo or no network, which must not read as "no versions".
        if !out.success() {
            return Err(Self::failed(&out));
        }
        let arch = match target {
            Target::Platform { arch, .. } => Some(rpm_arch(*arch)),
            Target::Any => None,
        };
        Ok(parse_repoquery(&out.stdout_utf8(), &name, arch))
    }

    /// The enabled repositories' priorities, as dnf reads its configuration
    /// (asked once).
    fn priorities(&self) -> Result<&Priorities, PortError> {
        self.priorities
            .get_or_init(|| {
                let (program, args) = if self.dnf5 {
                    ("dnf", argv(&["-q", "--dump-repo-config=*"]))
                } else {
                    ("python3", argv(&["-c", priority::DNF4_PRIORITIES]))
                };
                let out = self.runner.run(&Command::new(program, args))?;
                if !out.success() {
                    return Err(PortError::CommandFailed {
                        program: program.to_string(),
                        code: out.code,
                        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
                    });
                }
                let stdout = out.stdout_utf8();
                let found = if self.dnf5 {
                    priority::parse_dnf5(&stdout)
                } else {
                    priority::parse_dnf4(&stdout)
                }?;
                debug!(message = logmsg::DNF_PRIORITIES, priorities = ?found);
                Ok(found)
            })
            .as_ref()
            .map_err(Clone::clone)
    }
}

impl Index for DnfBackend {
    fn refresh(&self) -> Result<(), PortError> {
        let out = self
            .runner
            .run(&Command::new("dnf", argv(&["makecache", "--refresh"])))?;
        if out.success() {
            Ok(())
        } else {
            Err(Self::failed(&out))
        }
    }

    fn query_available(&self, pkg: &Ref, target: &Target) -> Result<Vec<Version>, PortError> {
        let builds = self.builds(pkg, target)?;
        Ok(Ranking::new(&builds, &Priorities::new()).versions())
    }

    /// dnf's own choice in `want`: by repository priority, then version
    /// ([`priority`]).
    fn select(
        &self,
        pkg: &Ref,
        target: &Target,
        want: &Want,
        prefer: &[Version],
    ) -> Result<Selection, PortError> {
        let builds = self.builds(pkg, target)?;
        let ranking = Ranking::new(&builds, self.priorities()?);
        let chosen = ranking.choose(want, prefer);
        if let Some(v) = &chosen {
            let name = pkg.purl.native_id();
            let priority = ranking.priority(v);
            debug!(message = logmsg::DNF_CHOSEN, package = %name, version = %v, priority);
            if let Some(newer) = ranking.passed_over(want, v) {
                debug!(
                    message = logmsg::DNF_NEWER_PASSED_OVER,
                    package = %name,
                    newer = %newer,
                    priority = ranking.priority(&newer),
                );
            }
        }
        Ok(Selection {
            chosen,
            available: ranking.versions(),
        })
    }
}

impl Installer for DnfBackend {
    fn capabilities(&self) -> Capabilities {
        // dnf installs and downgrades to exact repo builds; it holds through
        // versionlock when that exists.
        Capabilities {
            supports_exact_version: true,
            hold: if self.versionlock {
                HoldSupport::Supported
            } else {
                HoldSupport::Unsupported
            },
            supports_downgrade: true,
        }
    }

    fn scope(&self) -> Scope {
        Scope::System
    }

    fn ensure_source(&self, source: &Source) -> Result<Steps, PortError> {
        // A Copr project (`owner/project`), then a refresh of its metadata.
        Ok(vec![
            dnf(&["copr", "enable", "-y", source.id.as_str()]),
            dnf(&["makecache"]),
        ])
    }

    fn install_exact(&self, pkg: &Ref, version: &Version) -> Result<Steps, PortError> {
        let mut steps = self.unlock(pkg);
        steps.push(dnf(&[
            "install",
            "-y",
            "--setopt=install_weak_deps=False",
            &nevr(pkg, version),
        ]));
        Ok(steps)
    }

    fn verify_installed(&self, pkg: &Ref) -> Result<Installed, PortError> {
        // `rpm -q` exits non-zero when the package is not installed: data, not
        // an error. Installonly packages (kernel) list one line per version.
        let out = self.runner.run(&Command::new(
            "rpm",
            argv(&[
                "-q",
                "--queryformat",
                RPM_QUERY_FORMAT,
                &pkg.purl.native_id(),
            ]),
        ))?;
        if !out.success() {
            return Ok(Installed::Absent);
        }
        let newest = out
            .stdout_utf8()
            .lines()
            .filter_map(|line| Version::parse(line.trim(), Scheme::Rpm).ok())
            .max();
        Ok(match newest {
            Some(v) => Installed::Present { version: Some(v) },
            None => Installed::Absent,
        })
    }

    fn hold(&self, pkg: &Ref, _version: &Version) -> Result<Steps, PortError> {
        // Locks the installed build, which `install_exact` just put there.
        if self.versionlock {
            Ok(vec![dnf(&["versionlock", "add", &pkg.purl.native_id()])])
        } else {
            Ok(Vec::new())
        }
    }

    fn unhold(&self, pkg: &Ref) -> Result<Steps, PortError> {
        Ok(self.unlock(pkg))
    }

    fn downgrade(&self, pkg: &Ref, version: &Version) -> Result<Steps, PortError> {
        let mut steps = self.unlock(pkg);
        steps.push(dnf(&[
            "downgrade",
            "-y",
            "--setopt=install_weak_deps=False",
            &nevr(pkg, version),
        ]));
        Ok(steps)
    }

    fn uninstall(&self, pkg: &Ref) -> Result<Steps, PortError> {
        // Release the lock first, so a package bosun installed and held leaves
        // no lingering lock once removed (§9).
        let mut steps = self.unlock(pkg);
        steps.push(dnf(&["remove", "-y", &pkg.purl.native_id()]));
        Ok(steps)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bosun_core::SourceId;
    use bosun_ports::testing::FakeCommandRunner;
    use bosun_ports::Output;

    /// Output captured from `registry.fedoraproject.org/fedora:41` (dnf5) and
    /// `rockylinux/rockylinux:9` (dnf4).
    mod captured {
        pub const DNF5_VERSION: &str = include_str!("testdata/dnf5-version.txt");
        pub const DNF4_VERSION: &str = include_str!("testdata/dnf4-version.txt");
        pub const DNF5_REPOQUERY_JQ: &str = include_str!("testdata/dnf5-repoquery-jq.txt");
        pub const DNF4_REPOQUERY_JQ: &str = include_str!("testdata/dnf4-repoquery-jq.txt");
        pub const DNF5_REPOQUERY_TZDATA: &str = include_str!("testdata/dnf5-repoquery-tzdata.txt");
        pub const DNF4_REPOQUERY_TZDATA: &str = include_str!("testdata/dnf4-repoquery-tzdata.txt");
        pub const DNF5_REPOQUERY_SHADOW: &str =
            include_str!("testdata/dnf5-repoquery-shadow-utils.txt");
        pub const DNF4_REPOQUERY_SHADOW: &str =
            include_str!("testdata/dnf4-repoquery-shadow-utils.txt");
        pub const DNF5_RPMQ_JQ: &str = include_str!("testdata/dnf5-rpmq-jq.txt");
        pub const DNF4_RPMQ_JQ: &str = include_str!("testdata/dnf4-rpmq-jq.txt");
        pub const DNF5_RPMQ_SHADOW: &str = include_str!("testdata/dnf5-rpmq-shadow-utils.txt");
        pub const DNF4_RPMQ_SHADOW: &str = include_str!("testdata/dnf4-rpmq-shadow-utils.txt");
        pub const DNF5_RPMQ_ABSENT: &str = include_str!("testdata/dnf5-rpmq-tree-absent.txt");
        pub const DNF4_RPMQ_ABSENT: &str = include_str!("testdata/dnf4-rpmq-tree-absent.txt");
        pub const DNF4_NO_VERSIONLOCK: &str = include_str!("testdata/dnf4-whatprovides-before.txt");
        pub const DNF4_VERSIONLOCK: &str = include_str!("testdata/dnf4-whatprovides-after.txt");
    }
    use captured::*;

    fn rpm(v: &str) -> Version {
        Version::parse(v, Scheme::Rpm).unwrap()
    }

    fn reff(name: &str) -> Ref {
        Ref::new(format!("pkg:rpm/fedora/{name}").parse().unwrap())
    }

    fn backend(runner: Arc<dyn CommandRunner>, versionlock: bool) -> DnfBackend {
        DnfBackend {
            runner,
            versionlock,
            dnf5: true,
            priorities: Arc::new(OnceLock::new()),
        }
    }

    /// The versions of `builds`, for comparing a parse with a version list.
    fn versions(builds: Vec<Build>) -> Vec<Version> {
        Ranking::new(&builds, &Priorities::new()).versions()
    }

    fn idle(versionlock: bool) -> DnfBackend {
        backend(Arc::new(FakeCommandRunner::new()), versionlock)
    }

    fn exit(code: i32, stdout: &str) -> Output {
        Output {
            code: Some(code),
            stdout: stdout.as_bytes().to_vec(),
            stderr: Vec::new(),
        }
    }

    /// A system-scope Run step with no extra environment.
    fn run_step(argv: &[&str]) -> Step {
        Step::Run {
            argv: argv.iter().map(|s| s.to_string()).collect(),
            env: Vec::new(),
            scope: Scope::System,
            ok_exits: Vec::new(),
        }
    }

    const AMD64: Target = Target::Platform {
        os: Os::Linux,
        arch: Arch::Amd64,
    };
    const ARM64: Target = Target::Platform {
        os: Os::Linux,
        arch: Arch::Arm64,
    };

    fn host<'a>(os: Os, runner: &'a FakeCommandRunner) -> HostInfo<'a> {
        HostInfo {
            os,
            arch: Arch::Amd64,
            distro: None,
            runner,
        }
    }

    // ---- factory ----------------------------------------------------------

    #[test]
    fn meta_is_rpm_identity_and_ordering_per_distro() {
        let f = DnfFactory::new(Arc::new(FakeCommandRunner::new()));
        assert_eq!(f.id().as_str(), "dnf");
        assert_eq!(
            f.meta(),
            BackendMeta {
                purl_type: "rpm",
                vers_type: "rpm",
                platform_independent: false,
                option_keys: &[],
            }
        );
    }

    #[test]
    fn probe_reads_the_dnf5_and_dnf4_version() {
        let factory = DnfFactory::new(Arc::new(FakeCommandRunner::new()));
        for (stdout, want) in [(DNF5_VERSION, "5.2.17.0"), (DNF4_VERSION, "4.14.0")] {
            let runner = FakeCommandRunner::new().with_stdout(stdout);
            let probe = factory.probe(&host(Os::Linux, &runner)).unwrap().unwrap();
            assert_eq!(probe.backend.as_str(), "dnf");
            assert_eq!(probe.version.as_str(), want);
            assert_eq!(probe.version.scheme(), Scheme::Rpm);
            assert_eq!(runner.calls()[0].program, "dnf");
            assert_eq!(runner.calls()[0].args, vec!["--version"]);
        }
    }

    #[test]
    fn probe_is_none_off_linux() {
        let factory = DnfFactory::new(Arc::new(FakeCommandRunner::new()));
        let runner = FakeCommandRunner::new().with_stdout(DNF5_VERSION);
        assert!(factory.probe(&host(Os::Macos, &runner)).unwrap().is_none());
        assert!(runner.calls().is_empty());
    }

    #[test]
    fn probe_is_none_when_dnf_fails() {
        let factory = DnfFactory::new(Arc::new(FakeCommandRunner::new()));
        let runner = FakeCommandRunner::new().with_output(exit(127, ""));
        assert!(factory.probe(&host(Os::Linux, &runner)).unwrap().is_none());
    }

    #[test]
    fn parse_dnf_version_needs_a_word() {
        assert_eq!(parse_dnf_version("4.14.0\n").unwrap().as_str(), "4.14.0");
        assert!(parse_dnf_version("").is_err());
        assert!(parse_dnf_version("\n4.14.0").is_err());
    }

    fn probe(version: &str) -> Probe {
        Probe {
            backend: BackendId::new("dnf"),
            version: rpm(version),
        }
    }

    #[test]
    fn dnf5_holds_with_its_built_in_versionlock() {
        let runner = Arc::new(FakeCommandRunner::new());
        let built = DnfFactory::new(runner.clone()).build(probe("5.2.17.0"));
        assert_eq!(built.installer.capabilities().hold, HoldSupport::Supported);
        assert!(runner.calls().is_empty(), "dnf5 needs no plugin check");
    }

    #[test]
    fn dnf4_holds_only_with_the_versionlock_plugin() {
        let with = Arc::new(FakeCommandRunner::new().with_stdout(DNF4_VERSIONLOCK));
        let built = DnfFactory::new(with.clone()).build(probe("4.14.0"));
        assert_eq!(built.installer.capabilities().hold, HoldSupport::Supported);
        assert_eq!(with.calls()[0].program, "rpm");
        assert_eq!(
            with.calls()[0].args,
            vec!["-q", "--whatprovides", "dnf-command(versionlock)"]
        );

        let without = Arc::new(FakeCommandRunner::new().with_output(exit(1, DNF4_NO_VERSIONLOCK)));
        let built = DnfFactory::new(without).build(probe("4.14.0"));
        assert_eq!(
            built.installer.capabilities().hold,
            HoldSupport::Unsupported
        );

        // rpm itself failing to run reads as no plugin, not a crash.
        let broken = Arc::new(FakeCommandRunner::new());
        let built = DnfFactory::new(broken).build(probe("4.14.0"));
        assert_eq!(
            built.installer.capabilities().hold,
            HoldSupport::Unsupported
        );
    }

    /// What `f` logs, as the formatted lines a subscriber would print.
    fn logged(f: impl FnOnce()) -> String {
        #[derive(Clone, Default)]
        struct Buf(Arc<std::sync::Mutex<Vec<u8>>>);
        impl std::io::Write for Buf {
            fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(b);
                Ok(b.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let buf = Buf::default();
        let sink = buf.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(move || sink.clone())
            .with_ansi(false)
            .finish();
        tracing::subscriber::with_default(subscriber, f);
        let bytes = buf.0.lock().unwrap().clone();
        String::from_utf8(bytes).unwrap()
    }

    #[test]
    fn a_missing_versionlock_plugin_is_logged_with_its_remedy() {
        let without = Arc::new(FakeCommandRunner::new().with_output(exit(1, DNF4_NO_VERSIONLOCK)));
        let log = logged(|| {
            DnfFactory::new(without).build(probe("4.14.0"));
        });
        assert!(log.contains(logmsg::DNF_NO_VERSIONLOCK), "{log}");

        let with = Arc::new(FakeCommandRunner::new().with_stdout(DNF4_VERSIONLOCK));
        let log = logged(|| {
            DnfFactory::new(with).build(probe("4.14.0"));
        });
        assert!(!log.contains(logmsg::DNF_NO_VERSIONLOCK), "{log}");
    }

    #[test]
    fn capabilities_and_scope() {
        let caps = idle(true).capabilities();
        assert!(caps.supports_exact_version && caps.supports_downgrade);
        assert_eq!(caps.hold, HoldSupport::Supported);
        assert_eq!(idle(false).capabilities().hold, HoldSupport::Unsupported);
        assert_eq!(idle(true).scope(), Scope::System);
    }

    // ---- index ------------------------------------------------------------

    #[test]
    fn refresh_runs_makecache() {
        let fake = Arc::new(FakeCommandRunner::new().with_stdout("Metadata cache created.\n"));
        backend(fake.clone(), true).refresh().unwrap();
        let calls = fake.calls();
        assert_eq!(calls[0].program, "dnf");
        assert_eq!(calls[0].args, vec!["makecache", "--refresh"]);
    }

    #[test]
    fn refresh_propagates_failure() {
        let fake = Arc::new(FakeCommandRunner::new().with_output(Output {
            code: Some(1),
            stdout: vec![],
            stderr: b"Curl error (6): Couldn't resolve host name".to_vec(),
        }));
        let err = backend(fake, true).refresh().unwrap_err();
        assert_eq!(
            err,
            PortError::CommandFailed {
                program: "dnf".to_string(),
                code: Some(1),
                stderr: "Curl error (6): Couldn't resolve host name".to_string(),
            }
        );
    }

    #[test]
    fn query_available_runs_repoquery_for_the_package() {
        let fake = Arc::new(FakeCommandRunner::new().with_stdout(DNF5_REPOQUERY_JQ));
        let got = backend(fake.clone(), true)
            .query_available(&reff("jq"), &AMD64)
            .unwrap();
        assert_eq!(got, vec![rpm("1.7.1-8.fc41")]);
        let calls = fake.calls();
        assert_eq!(calls[0].program, "dnf");
        assert_eq!(
            calls[0].args,
            vec![
                "repoquery",
                "--available",
                "--queryformat",
                "%{name}|%{epoch}|%{version}|%{release}|%{arch}|%{repoid}\\n",
                "jq"
            ]
        );
    }

    #[test]
    fn query_available_fails_when_repoquery_fails() {
        let fake = Arc::new(FakeCommandRunner::new().with_output(Output {
            code: Some(1),
            stdout: vec![],
            stderr: b"Error: Unknown repo: 'nosuch'".to_vec(),
        }));
        let err = backend(fake, true)
            .query_available(&reff("jq"), &AMD64)
            .unwrap_err();
        assert!(matches!(err, PortError::CommandFailed { .. }), "{err:?}");
    }

    #[test]
    fn repoquery_reads_dnf5_and_dnf4_output() {
        // dnf4 ends each record with a blank line; dnf5 does not.
        for out in [DNF5_REPOQUERY_JQ, DNF4_REPOQUERY_JQ] {
            assert_eq!(parse_repoquery(out, "jq", Some("x86_64")).len(), 1);
        }
        assert_eq!(
            parse_repoquery(DNF4_REPOQUERY_JQ, "jq", Some("x86_64")),
            vec![Build {
                version: rpm("1.6-19.el9_8.2"),
                repo: "baseos".to_string()
            }]
        );
        assert_eq!(
            parse_repoquery(DNF5_REPOQUERY_TZDATA, "tzdata", Some("x86_64")),
            vec![
                Build {
                    version: rpm("2024a-9.fc41"),
                    repo: "fedora".to_string()
                },
                Build {
                    version: rpm("2025b-1.fc41"),
                    repo: "updates".to_string()
                }
            ]
        );
        assert_eq!(
            versions(parse_repoquery(
                DNF4_REPOQUERY_TZDATA,
                "tzdata",
                Some("aarch64")
            )),
            vec![
                rpm("2026a-1.el9"),
                rpm("2026b-1.el9_8"),
                rpm("2026c-1.el9_8"),
                rpm("2026e-1.el9_8")
            ]
        );
    }

    #[test]
    fn repoquery_keeps_a_nonzero_epoch() {
        let got = versions(parse_repoquery(
            DNF5_REPOQUERY_SHADOW,
            "shadow-utils",
            Some("x86_64"),
        ));
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].as_str(), "2:4.15.1-12.fc41");
        let got = versions(parse_repoquery(
            DNF4_REPOQUERY_SHADOW,
            "shadow-utils",
            Some("x86_64"),
        ));
        assert_eq!(got[0].as_str(), "2:4.9-16.el9");
        // A zero epoch is left off, as rpm prints it.
        assert_eq!(
            parse_repoquery(DNF5_REPOQUERY_JQ, "jq", None)[0]
                .version
                .as_str(),
            "1.7.1-8.fc41"
        );
    }

    #[test]
    fn repoquery_keeps_this_arch_and_noarch_only() {
        assert!(parse_repoquery(DNF5_REPOQUERY_JQ, "jq", Some("aarch64")).is_empty());
        let mixed = "jq|0|1.7.1|8.fc41|i686|fedora\njq|0|1.7.1|9.fc41|x86_64|updates\n";
        assert_eq!(
            versions(parse_repoquery(mixed, "jq", Some("x86_64"))),
            vec![rpm("1.7.1-9.fc41")]
        );
        // With no arch to match, every build counts, each once.
        assert_eq!(
            versions(parse_repoquery(mixed, "jq", None)),
            vec![rpm("1.7.1-8.fc41"), rpm("1.7.1-9.fc41")]
        );
        assert_eq!(parse_repoquery(DNF5_REPOQUERY_JQ, "jq", None).len(), 1);
    }

    #[test]
    fn repoquery_keeps_each_repository_offering_a_build() {
        let twice = "jq|0|1.7.1|8.fc41|x86_64|fedora\njq|0|1.7.1|8.fc41|x86_64|mirror\n";
        let got = parse_repoquery(twice, "jq", Some("x86_64"));
        assert_eq!(got.len(), 2);
        assert_eq!(got[1].repo, "mirror");
    }

    #[test]
    fn repoquery_keeps_exactly_the_named_package() {
        // repoquery also matches globs and provides; only `name` counts.
        let out = "jq-devel|0|1.7.1|8.fc41|x86_64|fedora\njq|0|1.7.1|8.fc41|x86_64|fedora\n";
        assert_eq!(
            versions(parse_repoquery(out, "jq", Some("x86_64"))),
            vec![rpm("1.7.1-8.fc41")]
        );
        // Malformed lines are skipped.
        assert!(parse_repoquery("jq|0|1.7.1|8.fc41|x86_64\njq\n", "jq", None).is_empty());
        assert!(parse_repoquery("jq|0|1.7.1|8.fc41|x86_64|fedora|extra\n", "jq", None).is_empty());
    }

    const DNF5_DUMP: &str = include_str!("testdata/dnf5-dump-repo-config.txt");
    const DNF5_VIM_PRIORITY: &str =
        include_str!("testdata/dnf5-repoquery-vim-enhanced-fedora-priority-1.txt");
    const DNF4_CURL_PRIORITY: &str =
        include_str!("testdata/dnf4-repoquery-curl-vault-priority-1.txt");
    const DNF4_PRIORITIES: &str = include_str!("testdata/dnf4-repo-priorities.txt");

    fn want(range: &str) -> Want {
        Want::of(bosun_core::VersRange::parse(range).unwrap())
    }

    #[test]
    fn dnf5_selects_by_the_priorities_it_prints() {
        // Fedora 41 with `priority=1` on fedora: `dnf install vim-enhanced`
        // takes fedora's 9.1.737 over updates' 9.1.1914.
        let fake = Arc::new(
            FakeCommandRunner::new()
                .with_stdout(DNF5_VIM_PRIORITY)
                .with_stdout(DNF5_DUMP),
        );
        let got = backend(fake.clone(), true)
            .select(&reff("vim-enhanced"), &AMD64, &want("vers:rpm/*"), &[])
            .unwrap();
        assert_eq!(got.chosen, Some(rpm("2:9.1.737-1.fc41")));
        assert_eq!(
            got.available,
            vec![rpm("2:9.1.1914-1.fc41"), rpm("2:9.1.737-1.fc41")]
        );
        let calls = fake.calls();
        assert_eq!(calls[1].program, "dnf");
        assert_eq!(calls[1].args, vec!["-q", "--dump-repo-config=*"]);
    }

    #[test]
    fn dnf4_selects_by_the_priorities_its_api_reports() {
        // Rocky 9 with the 9.5 vault at `priority=1`: `dnf install curl`
        // takes the vault's 7.76.1-31.el9 over baseos' newer builds.
        let fake = Arc::new(
            FakeCommandRunner::new()
                .with_stdout(DNF4_CURL_PRIORITY)
                .with_stdout(DNF4_PRIORITIES)
                .with_stdout(DNF4_CURL_PRIORITY),
        );
        let mut b = backend(fake.clone(), true);
        b.dnf5 = false;
        let got = b
            .select(&reff("curl"), &AMD64, &want("vers:rpm/*"), &[])
            .unwrap();
        assert_eq!(got.chosen, Some(rpm("7.76.1-31.el9")));
        let wider = b
            .select(&reff("curl"), &AMD64, &want("vers:rpm/>=7.76.1-40"), &[])
            .unwrap();
        assert_eq!(wider.chosen, Some(rpm("7.76.1-40.el9_8.8")));
        let calls = fake.calls();
        assert_eq!(calls[1].program, "python3");
        assert_eq!(calls[1].args, vec!["-c", priority::DNF4_PRIORITIES]);
        // The priorities are asked once.
        assert_eq!(calls.len(), 3);
        assert_eq!(calls[2].args[0], "repoquery");
    }

    #[test]
    fn priorities_dnf_cannot_report_fail_the_selection() {
        let fake = Arc::new(
            FakeCommandRunner::new()
                .with_stdout(DNF5_VIM_PRIORITY)
                .with_output(exit(1, "")),
        );
        let err = backend(fake, true)
            .select(&reff("vim-enhanced"), &AMD64, &want("vers:rpm/*"), &[])
            .unwrap_err();
        assert!(
            matches!(err, PortError::CommandFailed { ref program, code: Some(1), .. } if program == "dnf"),
            "{err:?}"
        );
        let fake = Arc::new(
            FakeCommandRunner::new()
                .with_stdout(DNF5_VIM_PRIORITY)
                .with_stdout("priority = 1\n"),
        );
        assert!(backend(fake, true)
            .select(&reff("vim-enhanced"), &AMD64, &want("vers:rpm/*"), &[])
            .is_err());
    }

    #[test]
    fn query_available_uses_the_targets_rpm_arch() {
        assert_eq!(rpm_arch(Arch::Amd64), "x86_64");
        assert_eq!(rpm_arch(Arch::Arm64), "aarch64");
        let fake = Arc::new(FakeCommandRunner::new().with_stdout(DNF5_REPOQUERY_JQ));
        assert!(backend(fake, true)
            .query_available(&reff("jq"), &ARM64)
            .unwrap()
            .is_empty());
        let fake = Arc::new(FakeCommandRunner::new().with_stdout(DNF5_REPOQUERY_JQ));
        assert_eq!(
            backend(fake, true)
                .query_available(&reff("jq"), &Target::Any)
                .unwrap(),
            vec![rpm("1.7.1-8.fc41")]
        );
    }

    // ---- installer: reads -------------------------------------------------

    #[test]
    fn verify_installed_reads_rpm_q() {
        for (out, want) in [
            (DNF5_RPMQ_JQ, "1.7.1-8.fc41"),
            (DNF4_RPMQ_JQ, "1.6-19.el9_8.2"),
        ] {
            let fake = Arc::new(FakeCommandRunner::new().with_stdout(out));
            assert_eq!(
                backend(fake.clone(), true)
                    .verify_installed(&reff("jq"))
                    .unwrap(),
                Installed::Present {
                    version: Some(rpm(want))
                }
            );
            let calls = fake.calls();
            assert_eq!(calls[0].program, "rpm");
            assert_eq!(
                calls[0].args,
                vec![
                    "-q",
                    "--queryformat",
                    "%|EPOCH?{%{EPOCH}:}:{}|%{VERSION}-%{RELEASE}\\n",
                    "jq"
                ]
            );
        }
    }

    #[test]
    fn verify_installed_keeps_the_epoch() {
        for (out, want) in [
            (DNF5_RPMQ_SHADOW, "2:4.15.1-12.fc41"),
            (DNF4_RPMQ_SHADOW, "2:4.9-16.el9"),
        ] {
            let fake = Arc::new(FakeCommandRunner::new().with_stdout(out));
            let got = backend(fake, true)
                .verify_installed(&reff("shadow-utils"))
                .unwrap();
            let Installed::Present { version: Some(v) } = got else {
                panic!("{got:?}")
            };
            assert_eq!(v.as_str(), want);
        }
    }

    #[test]
    fn verify_installed_not_installed_is_absent_not_error() {
        // rpm prints "package tree is not installed" to stdout and exits 1.
        for out in [DNF5_RPMQ_ABSENT, DNF4_RPMQ_ABSENT] {
            let fake = Arc::new(FakeCommandRunner::new().with_output(exit(1, out)));
            assert_eq!(
                backend(fake, true).verify_installed(&reff("tree")).unwrap(),
                Installed::Absent
            );
        }
        let fake = Arc::new(FakeCommandRunner::new().with_stdout("\n"));
        assert_eq!(
            backend(fake, true).verify_installed(&reff("tree")).unwrap(),
            Installed::Absent
        );
    }

    #[test]
    fn verify_installed_reports_the_newest_of_several() {
        // Installonly packages (kernel) keep several versions side by side.
        let out = "6.11.4-301.fc41\n6.12.5-200.fc41\n6.11.10-300.fc41\n";
        let fake = Arc::new(FakeCommandRunner::new().with_stdout(out));
        assert_eq!(
            backend(fake, true)
                .verify_installed(&reff("kernel-core"))
                .unwrap(),
            Installed::Present {
                version: Some(rpm("6.12.5-200.fc41"))
            }
        );
    }

    // ---- installer: steps -------------------------------------------------

    #[test]
    fn install_exact_releases_the_lock_then_installs_name_evr() {
        assert_eq!(
            idle(true)
                .install_exact(&reff("jq"), &rpm("1.7.1-8.fc41"))
                .unwrap(),
            vec![
                run_step(&["dnf", "versionlock", "delete", "jq"]),
                run_step(&[
                    "dnf",
                    "install",
                    "-y",
                    "--setopt=install_weak_deps=False",
                    "jq-1.7.1-8.fc41"
                ]),
            ]
        );
        assert_eq!(
            idle(false)
                .install_exact(&reff("shadow-utils"), &rpm("2:4.9-16.el9"))
                .unwrap(),
            vec![run_step(&[
                "dnf",
                "install",
                "-y",
                "--setopt=install_weak_deps=False",
                "shadow-utils-2:4.9-16.el9"
            ])]
        );
    }

    #[test]
    fn hold_is_a_versionlock_when_there_is_one() {
        assert_eq!(
            idle(true).hold(&reff("jq"), &rpm("1.7.1-8.fc41")).unwrap(),
            vec![run_step(&["dnf", "versionlock", "add", "jq"])]
        );
        assert!(idle(false)
            .hold(&reff("jq"), &rpm("1.7.1-8.fc41"))
            .unwrap()
            .is_empty());
    }

    #[test]
    fn unhold_deletes_the_versionlock_when_there_is_one() {
        assert_eq!(
            idle(true).unhold(&reff("jq")).unwrap(),
            vec![run_step(&["dnf", "versionlock", "delete", "jq"])]
        );
        assert!(idle(false).unhold(&reff("jq")).unwrap().is_empty());
    }

    #[test]
    fn downgrade_releases_the_lock_then_downgrades() {
        assert_eq!(
            idle(true)
                .downgrade(&reff("tzdata"), &rpm("2024a-9.fc41"))
                .unwrap(),
            vec![
                run_step(&["dnf", "versionlock", "delete", "tzdata"]),
                run_step(&[
                    "dnf",
                    "downgrade",
                    "-y",
                    "--setopt=install_weak_deps=False",
                    "tzdata-2024a-9.fc41"
                ]),
            ]
        );
        assert_eq!(
            idle(false)
                .downgrade(&reff("tzdata"), &rpm("2024a-9.fc41"))
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn uninstall_releases_the_lock_then_removes() {
        assert_eq!(
            idle(true).uninstall(&reff("jq")).unwrap(),
            vec![
                run_step(&["dnf", "versionlock", "delete", "jq"]),
                run_step(&["dnf", "remove", "-y", "jq"]),
            ]
        );
        assert_eq!(
            idle(false).uninstall(&reff("jq")).unwrap(),
            vec![run_step(&["dnf", "remove", "-y", "jq"])]
        );
    }

    #[test]
    fn ensure_source_enables_a_copr_then_refreshes() {
        let steps = idle(true)
            .ensure_source(&Source {
                backend: BackendId::new("dnf"),
                id: SourceId::new("atim/starship"),
            })
            .unwrap();
        assert_eq!(
            steps,
            vec![
                run_step(&["dnf", "copr", "enable", "-y", "atim/starship"]),
                run_step(&["dnf", "makecache"]),
            ]
        );
    }
}
