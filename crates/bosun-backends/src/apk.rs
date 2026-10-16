//! The apk backend (Alpine, §5.2 versioned-OS tier). Mutations are step
//! lists (`apk add pkg=ver`, `apk del pkg`) a host executor runs;
//! `verify_installed`/`query_available` run directly via the
//! [`CommandRunner`]. apk is Linux-only and system-scope.
//!
//! A repository carries one version of a package per branch, so the
//! versions on offer are what `apk policy` lists for the configured
//! repositories: an exact install or downgrade reaches only those. A
//! version only a tagged repository (`@edge`) carries is left out, since
//! installing it needs the tag. `apk add pkg=ver` records the constraint in
//! `/etc/apk/world`, and that constraint is the hold: no upgrade moves the
//! package off it until the world says otherwise, so a hold adds no step.

use std::sync::Arc;

use bosun_core::{
    BackendId, Capabilities, HoldSupport, Installed, Os, Ref, Scheme, Scope, Source, Target,
    Version,
};
use bosun_ports::{
    Backend, BackendFactory, BackendMeta, Command, CommandRunner, HostInfo, Index, Installer,
    PortError, Probe, Step, Steps, UserInstall, UserInstalls,
};
use tracing::debug;

use crate::logmsg;

const BACKEND: &str = "apk";

/// The file apk keeps the user's requested packages in.
const WORLD: &str = "/etc/apk/world";

/// Builds the apk backend when `apk` answers on a Linux host (§5.3).
pub struct ApkFactory {
    runner: Arc<dyn CommandRunner>,
}

impl ApkFactory {
    /// Construct with the command runner the built backend will use.
    pub fn new(runner: Arc<dyn CommandRunner>) -> ApkFactory {
        ApkFactory { runner }
    }
}

impl BackendFactory for ApkFactory {
    fn id(&self) -> BackendId {
        BackendId::new(BACKEND)
    }

    fn meta(&self) -> BackendMeta {
        BackendMeta {
            purl_type: "apk",
            vers_type: "apk",
            platform_independent: false,
            option_keys: &[],
        }
    }

    fn probe(&self, host: &HostInfo) -> Result<Option<Probe>, PortError> {
        if host.os != Os::Linux {
            return Ok(None); // apk is Linux-only
        }
        let out = host
            .runner
            .run(&Command::new("apk", argv(&["--version"])))?;
        if !out.success() {
            return Ok(None);
        }
        Ok(Some(Probe {
            backend: self.id(),
            version: parse_apk_version(&out.stdout_utf8())?,
        }))
    }

    fn build(&self, probe: Probe) -> Backend {
        let backend = ApkBackend {
            runner: self.runner.clone(),
            _version: probe.version,
        };
        Backend {
            index: Box::new(backend.clone()),
            installer: Box::new(backend),
        }
    }
}

/// `apk-tools 2.14.4, compiled for x86_64.` → `2.14.4`.
fn parse_apk_version(stdout: &str) -> Result<Version, PortError> {
    let word = stdout
        .split_whitespace()
        .nth(1)
        .map(|w| w.trim_end_matches(','))
        .ok_or_else(|| PortError::CommandOutput {
            detail: format!("could not read apk version from {stdout:?}"),
        })?;
    Version::parse(word, Scheme::Alpine).map_err(|e| PortError::CommandOutput {
        detail: format!("apk version {word:?}: {e}"),
    })
}

fn argv(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|s| s.to_string()).collect()
}

/// One system-scope `apk` step.
fn apk_step(args: &[&str]) -> Steps {
    let mut command = vec!["apk"];
    command.extend(args);
    vec![Step::Run {
        argv: argv(&command),
        env: Vec::new(),
        scope: Scope::System,
        ok_exits: Vec::new(),
    }]
}

/// Run `program args`, its stdout when it exits 0.
fn run_ok(runner: &dyn CommandRunner, program: &str, args: &[&str]) -> Result<String, PortError> {
    let out = runner.run(&Command::new(program, argv(args)))?;
    if out.success() {
        Ok(out.stdout_utf8().into_owned())
    } else {
        Err(failed(program, &out))
    }
}

fn failed(program: &str, out: &bosun_ports::Output) -> PortError {
    PortError::CommandFailed {
        program: program.to_string(),
        code: out.code,
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    }
}

#[derive(Clone)]
struct ApkBackend {
    runner: Arc<dyn CommandRunner>,
    // Recorded for future per-version flag gating (Appendix B #224).
    _version: Version,
}

/// The versions `apk policy` lists, once each, that an untagged repository
/// or the installed database offers. The output is a header, then each
/// version indented two spaces, then where it comes from indented four; a
/// tagged repository's line starts with its `@tag`.
fn policy_versions(stdout: &str) -> Vec<Version> {
    let mut versions: Vec<Version> = Vec::new();
    let mut current: Option<Version> = None;
    for line in stdout.lines() {
        if let Some(origin) = line.strip_prefix("    ") {
            if let Some(v) = &current {
                if !origin.starts_with('@') && !versions.contains(v) {
                    versions.push(v.clone());
                }
            }
        } else if let Some(version) = line.strip_prefix("  ") {
            current = version
                .strip_suffix(':')
                .and_then(|v| Version::parse(v, Scheme::Alpine).ok());
        }
    }
    versions
}

/// The packages an Alpine image's world file lists before anyone adds to
/// it, and `alpine-base`, which a disk install adds: the base system, not
/// what the user asked for.
const BASE_PACKAGES: [&str; 9] = [
    "alpine-base",
    "alpine-baselayout",
    "alpine-baselayout-data",
    "alpine-keys",
    "alpine-release",
    "apk-tools",
    "busybox",
    "libc-utils",
    "musl-utils",
];

/// The package a world line asks for, or `None` for a line that names no
/// package bosun could install: a conflict (`!nano`), a virtual
/// (`cmd:less`, `so:libz.so.1`), one from a tagged repository (`jq@edge`),
/// or the base system. A version constraint is cut off (`tree=2.2.1-r0`).
fn world_package(line: &str) -> Option<&str> {
    if line.is_empty() || line.starts_with('!') || line.contains(':') {
        return None;
    }
    let end = line.find(['@', '=', '<', '>', '~']).unwrap_or(line.len());
    if line[end..].starts_with('@') {
        debug!(message = logmsg::TAGGED_LEFT_OUT, package = line);
        return None;
    }
    let name = &line[..end];
    (!BASE_PACKAGES.contains(&name)).then_some(name)
}

/// Which of `names` an `apk info -v` line (`<name>-<version>`) is, and its
/// version. The longest name whose remainder is a version wins, since one
/// name can begin another (`py3`, `py3-3to2`).
fn installed_line<'a>(line: &str, names: &[&'a str]) -> Option<(&'a str, Version)> {
    names
        .iter()
        .filter_map(|name| {
            let rest = line.strip_prefix(name)?.strip_prefix('-')?;
            let version = Version::parse(rest, Scheme::Alpine).ok()?;
            Some((*name, version))
        })
        .max_by_key(|(name, _)| name.len())
}

/// What the user installed through apk (§9 state export): the packages
/// `/etc/apk/world` names, less the base system ([`BASE_PACKAGES`]), at the
/// versions `apk info` reports installed. A world entry apk does not have
/// installed is skipped.
pub fn user_installs(runner: &dyn CommandRunner) -> Result<UserInstalls, PortError> {
    let world = run_ok(runner, "cat", &[WORLD])?;
    let names: Vec<&str> = world.lines().filter_map(world_package).collect();
    // With no names, apk info would list every installed package.
    if names.is_empty() {
        return Ok(UserInstalls::Listed(Vec::new()));
    }
    let mut args = vec!["info", "-e", "-v"];
    args.extend(&names);
    let out = runner.run(&Command::new("apk", argv(&args)))?;
    let mut found = Vec::new();
    for line in out.stdout_utf8().lines() {
        let (package, version) =
            installed_line(line, &names).ok_or_else(|| PortError::CommandOutput {
                detail: format!("apk info line {line:?}"),
            })?;
        found.push(UserInstall {
            package: package.to_string(),
            version,
        });
    }
    // apk info exits with the number of names it did not find; any other
    // exit is a failure.
    let missing = names.len() - found.len();
    match out.code {
        Some(code) if usize::try_from(code) == Ok(missing) => Ok(UserInstalls::Listed(found)),
        _ => Err(failed("apk", &out)),
    }
}

impl Index for ApkBackend {
    fn refresh(&self) -> Result<(), PortError> {
        run_ok(self.runner.as_ref(), "apk", &["update"]).map(|_| ())
    }

    fn query_available(&self, pkg: &Ref, _target: &Target) -> Result<Vec<Version>, PortError> {
        // An unknown package exits 0 with nothing listed.
        let name = pkg.purl.native_id();
        let out = run_ok(self.runner.as_ref(), "apk", &["policy", &name])?;
        Ok(policy_versions(&out))
    }
}

impl Installer for ApkBackend {
    fn capabilities(&self) -> Capabilities {
        // `apk add pkg=ver` installs, upgrades or downgrades to any version a
        // repository carries, and its world constraint holds it there.
        Capabilities {
            supports_exact_version: true,
            hold: HoldSupport::Supported,
            supports_downgrade: true,
        }
    }

    fn scope(&self) -> Scope {
        Scope::System
    }

    fn ensure_source(&self, _source: &Source) -> Result<Steps, PortError> {
        // A repository needs its signing key in /etc/apk/keys as well as its
        // URL, which a source id doesn't carry; repositories are configured
        // outside bosun.
        Ok(vec![])
    }

    fn install_exact(&self, pkg: &Ref, version: &Version) -> Result<Steps, PortError> {
        let pinned = format!("{}={}", pkg.purl.native_id(), version.as_str());
        Ok(apk_step(&["add", "--no-interactive", &pinned]))
    }

    fn verify_installed(&self, pkg: &Ref) -> Result<Installed, PortError> {
        // `apk info -e -v <pkg>` prints `<pkg>-<version>`, or nothing and
        // exits 1 when it is not installed: data, not an error.
        let name = pkg.purl.native_id();
        let out = self
            .runner
            .run(&Command::new("apk", argv(&["info", "-e", "-v", &name])))?;
        if !out.success() {
            return Ok(Installed::Absent);
        }
        let stdout = out.stdout_utf8();
        let line = stdout.trim();
        if line.is_empty() {
            return Ok(Installed::Absent);
        }
        installed_line(line, &[name.as_str()])
            .map(|(_, version)| Installed::Present {
                version: Some(version),
            })
            .ok_or_else(|| PortError::CommandOutput {
                detail: format!("apk info line {line:?} for {name}"),
            })
    }

    fn hold(&self, _pkg: &Ref, _version: &Version) -> Result<Steps, PortError> {
        // The install's `pkg=ver` in the world file is the hold.
        Ok(vec![])
    }

    fn downgrade(&self, pkg: &Ref, version: &Version) -> Result<Steps, PortError> {
        // apk moves to whatever version the world asks for, down included.
        self.install_exact(pkg, version)
    }

    fn user_installs(&self) -> Result<UserInstalls, PortError> {
        user_installs(self.runner.as_ref())
    }

    fn uninstall(&self, pkg: &Ref) -> Result<Steps, PortError> {
        // Deleting it from the world drops its pin too.
        Ok(apk_step(&[
            "del",
            "--no-interactive",
            &pkg.purl.native_id(),
        ]))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bosun_core::{Arch, SourceId};
    use bosun_ports::testing::FakeCommandRunner;
    use bosun_ports::Output;

    fn apk(v: &str) -> Version {
        Version::parse(v, Scheme::Alpine).unwrap()
    }

    fn reff(name: &str) -> Ref {
        Ref::new(
            format!("pkg:apk/alpine/{name}?arch=x86_64")
                .parse()
                .unwrap(),
        )
    }

    fn backend(runner: Arc<dyn CommandRunner>) -> ApkBackend {
        ApkBackend {
            runner,
            _version: apk("2.14.4"),
        }
    }

    fn run_step(argv: &[&str]) -> Step {
        Step::Run {
            argv: argv.iter().map(|s| s.to_string()).collect(),
            env: Vec::new(),
            scope: Scope::System,
            ok_exits: Vec::new(),
        }
    }

    fn exit(code: i32, stdout: &str) -> Output {
        Output {
            code: Some(code),
            stdout: stdout.as_bytes().to_vec(),
            stderr: b"ERROR: something".to_vec(),
        }
    }

    fn host<'a>(os: Os, runner: &'a FakeCommandRunner) -> HostInfo<'a> {
        HostInfo {
            os,
            arch: Arch::Amd64,
            distro: None,
            runner,
        }
    }

    // apk-tools 2 (Alpine 3.20) and 3 (Alpine 3.23), as captured.
    const VERSION_320: &str = include_str!("../tests/fixtures/apk/alpine320-version.txt");
    const VERSION_323: &str = include_str!("../tests/fixtures/apk/alpine323-version.txt");
    // Alpine 3.20 with the 3.21 repository added, and 3.23's under a tag.
    const POLICY_320_JQ: &str = include_str!("../tests/fixtures/apk/alpine320-policy-jq.txt");
    const POLICY_320_CURL: &str = include_str!("../tests/fixtures/apk/alpine320-policy-curl.txt");
    const POLICY_323_JQ: &str = include_str!("../tests/fixtures/apk/alpine323-policy-jq.txt");
    // Alpine 3.23 with curl, jq, ripgrep and tree added by hand, one pinned,
    // one ranged, plus a virtual (`cmd:less`) and a conflict (`!nano`).
    const WORLD_323: &str = include_str!("../tests/fixtures/apk/alpine323-world.txt");
    const INFO_323: &str = include_str!("../tests/fixtures/apk/alpine323-info-installed.txt");

    #[test]
    fn meta_is_apk_for_identity_and_versions() {
        let meta = ApkFactory::new(Arc::new(FakeCommandRunner::new())).meta();
        assert_eq!(meta.purl_type, "apk");
        assert_eq!(meta.vers_type, "apk");
        assert!(!meta.platform_independent);
        assert!(meta.option_keys.is_empty());
    }

    #[test]
    fn probe_reads_the_apk_tools_version_of_either_major() {
        let factory = ApkFactory::new(Arc::new(FakeCommandRunner::new()));
        for (out, want) in [(VERSION_320, "2.14.4"), (VERSION_323, "3.0.8-r0")] {
            let runner = FakeCommandRunner::new().with_stdout(out);
            let probe = factory.probe(&host(Os::Linux, &runner)).unwrap().unwrap();
            assert_eq!(probe.backend.as_str(), "apk");
            assert_eq!(probe.version, apk(want));
            assert_eq!(probe.version.scheme(), Scheme::Alpine);
            let calls = runner.calls();
            assert_eq!(calls[0].program, "apk");
            assert_eq!(calls[0].args, vec!["--version"]);
        }
    }

    #[test]
    fn probe_is_none_off_linux() {
        let factory = ApkFactory::new(Arc::new(FakeCommandRunner::new()));
        let runner = FakeCommandRunner::new().with_stdout(VERSION_323);
        assert!(factory
            .probe(&host(Os::Windows, &runner))
            .unwrap()
            .is_none());
        assert!(runner.calls().is_empty());
    }

    #[test]
    fn probe_is_none_when_apk_fails() {
        let factory = ApkFactory::new(Arc::new(FakeCommandRunner::new()));
        let runner = FakeCommandRunner::new().with_output(exit(127, ""));
        assert!(factory.probe(&host(Os::Linux, &runner)).unwrap().is_none());
    }

    #[test]
    fn an_unreadable_apk_version_is_an_error() {
        for out in ["", "apk-tools", "apk-tools not.a-version, compiled"] {
            assert!(
                matches!(parse_apk_version(out), Err(PortError::CommandOutput { .. })),
                "{out:?}"
            );
        }
    }

    #[test]
    fn refresh_runs_apk_update() {
        let fake = Arc::new(FakeCommandRunner::new().with_stdout("OK: 24171 distinct packages"));
        backend(fake.clone()).refresh().unwrap();
        let calls = fake.calls();
        assert_eq!(calls[0].program, "apk");
        assert_eq!(calls[0].args, vec!["update"]);
    }

    #[test]
    fn refresh_propagates_failure() {
        let fake = Arc::new(FakeCommandRunner::new().with_output(exit(1, "")));
        let err = backend(fake).refresh().unwrap_err();
        assert!(
            matches!(err, PortError::CommandFailed { ref program, code: Some(1), ref stderr }
                if program == "apk" && stderr == "ERROR: something"),
            "{err:?}"
        );
    }

    #[test]
    fn query_available_lists_each_untagged_repository_version_once() {
        let fake = Arc::new(FakeCommandRunner::new().with_stdout(POLICY_320_CURL));
        let got = backend(fake.clone())
            .query_available(&reff("curl"), &"linux/amd64".parse().unwrap())
            .unwrap();
        // 8.22.0-r0 is only in the repository tagged @v323.
        assert_eq!(got, vec![apk("8.14.1-r2")]);
        let calls = fake.calls();
        assert_eq!(calls[0].program, "apk");
        assert_eq!(calls[0].args, vec!["policy", "curl"]);
    }

    #[test]
    fn query_available_counts_the_installed_version() {
        assert_eq!(policy_versions(POLICY_320_JQ), vec![apk("1.7.1-r0")]);
        assert_eq!(policy_versions(POLICY_323_JQ), vec![apk("1.8.2-r0")]);
    }

    #[test]
    fn query_available_reads_every_version_a_repository_offers() {
        let policy = "jq policy:\n  1.7.1-r0:\n    https://a/v3.20/main\n  \
                      1.8.2-r0:\n    https://a/v3.23/main\n    @edge https://a/edge/main\n";
        assert_eq!(
            policy_versions(policy),
            vec![apk("1.7.1-r0"), apk("1.8.2-r0")]
        );
    }

    #[test]
    fn an_unknown_package_has_no_versions() {
        // apk policy exits 0 and prints nothing for a name no repository has.
        let fake = Arc::new(FakeCommandRunner::new().with_stdout(""));
        let got = backend(fake)
            .query_available(&reff("nosuch"), &"linux/amd64".parse().unwrap())
            .unwrap();
        assert!(got.is_empty());
    }

    #[test]
    fn a_failing_policy_is_an_error() {
        let fake = Arc::new(FakeCommandRunner::new().with_output(exit(99, "")));
        let err = backend(fake)
            .query_available(&reff("jq"), &"linux/amd64".parse().unwrap())
            .unwrap_err();
        assert!(matches!(err, PortError::CommandFailed { .. }), "{err:?}");
    }

    #[test]
    fn a_version_apk_could_not_have_printed_is_skipped() {
        let policy = "jq policy:\n  1.0-1:\n    https://a/main\n  1.1-r0:\n    https://a/main\n";
        assert_eq!(policy_versions(policy), vec![apk("1.1-r0")]);
    }

    #[test]
    fn install_and_downgrade_pin_the_version_in_the_world() {
        let b = backend(Arc::new(FakeCommandRunner::new()));
        let want = vec![run_step(&["apk", "add", "--no-interactive", "jq=1.7.1-r0"])];
        assert_eq!(
            b.install_exact(&reff("jq"), &apk("1.7.1-r0")).unwrap(),
            want
        );
        assert_eq!(b.downgrade(&reff("jq"), &apk("1.7.1-r0")).unwrap(), want);
    }

    #[test]
    fn the_world_pin_is_the_hold() {
        let b = backend(Arc::new(FakeCommandRunner::new()));
        assert_eq!(b.hold(&reff("jq"), &apk("1.7.1-r0")).unwrap(), vec![]);
        let caps = b.capabilities();
        assert!(caps.supports_exact_version && caps.supports_downgrade);
        assert_eq!(caps.hold, HoldSupport::Supported);
        assert_eq!(b.scope(), Scope::System);
    }

    #[test]
    fn uninstall_deletes_from_the_world() {
        let b = backend(Arc::new(FakeCommandRunner::new()));
        assert_eq!(
            b.uninstall(&reff("jq")).unwrap(),
            vec![run_step(&["apk", "del", "--no-interactive", "jq"])]
        );
    }

    #[test]
    fn repositories_are_configured_outside_bosun() {
        let b = backend(Arc::new(FakeCommandRunner::new()));
        let source = Source {
            backend: BackendId::new("apk"),
            id: SourceId::new("https://dl-cdn.alpinelinux.org/alpine/edge/testing"),
        };
        assert!(b.ensure_source(&source).unwrap().is_empty());
    }

    #[test]
    fn verify_installed_reads_apk_info() {
        let fake = Arc::new(FakeCommandRunner::new().with_stdout("jq-1.8.2-r0\n"));
        let b = backend(fake.clone());
        assert_eq!(
            b.verify_installed(&reff("jq")).unwrap(),
            Installed::Present {
                version: Some(apk("1.8.2-r0"))
            }
        );
        let calls = fake.calls();
        assert_eq!(calls[0].program, "apk");
        assert_eq!(calls[0].args, vec!["info", "-e", "-v", "jq"]);
    }

    #[test]
    fn verify_installed_not_installed_is_absent_not_error() {
        let fake = Arc::new(FakeCommandRunner::new().with_output(exit(1, "")));
        assert_eq!(
            backend(fake).verify_installed(&reff("jq")).unwrap(),
            Installed::Absent
        );
        let fake = Arc::new(FakeCommandRunner::new().with_stdout("\n"));
        assert_eq!(
            backend(fake).verify_installed(&reff("jq")).unwrap(),
            Installed::Absent
        );
    }

    #[test]
    fn verify_installed_rejects_another_package_or_a_bad_version() {
        for out in ["jql-1.0-r0\n", "jq-1.0-1\n", "jq\n"] {
            let fake = Arc::new(FakeCommandRunner::new().with_stdout(out));
            let err = backend(fake).verify_installed(&reff("jq")).unwrap_err();
            assert!(
                matches!(err, PortError::CommandOutput { .. }),
                "{out:?}: {err:?}"
            );
        }
    }

    fn listed(found: UserInstalls) -> Vec<(String, String)> {
        let UserInstalls::Listed(found) = found else {
            panic!("want a listing, got {found:?}");
        };
        found
            .into_iter()
            .map(|u| (u.package, u.version.as_str().to_string()))
            .collect()
    }

    fn pairs(v: &[(&str, &str)]) -> Vec<(String, String)> {
        v.iter()
            .map(|(p, v)| (p.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn user_installs_are_the_world_less_the_base_system() {
        let fake = Arc::new(
            FakeCommandRunner::new()
                .with_stdout(WORLD_323)
                .with_stdout(INFO_323),
        );
        let found = backend(fake.clone()).user_installs().unwrap();
        assert_eq!(
            listed(found),
            pairs(&[
                ("curl", "8.22.0-r0"),
                ("jq", "1.8.2-r0"),
                ("ripgrep", "15.1.0-r0"),
                ("tree", "2.2.1-r0"),
            ])
        );
        let calls = fake.calls();
        assert_eq!(calls[0].program, "cat");
        assert_eq!(calls[0].args, vec!["/etc/apk/world"]);
        assert_eq!(calls[1].program, "apk");
        assert_eq!(
            calls[1].args,
            vec!["info", "-e", "-v", "curl", "jq", "ripgrep", "tree"]
        );
    }

    #[test]
    fn every_base_package_is_left_out() {
        let base = "alpine-base\nalpine-baselayout\nalpine-baselayout-data\nalpine-keys\n\
                    alpine-release\napk-tools\nbusybox\nlibc-utils\nmusl-utils\n";
        let fake = FakeCommandRunner::new().with_stdout(base);
        assert_eq!(listed(user_installs(&fake).unwrap()), vec![]);
        // With no names, apk info would list every installed package.
        assert_eq!(fake.calls().len(), 1);
    }

    #[test]
    fn a_tagged_entry_is_left_out() {
        let fake = FakeCommandRunner::new()
            .with_stdout("jq@edge\nfd@edge=10.2.0-r0\ntree\n")
            .with_stdout("tree-2.2.1-r0\n");
        assert_eq!(
            listed(user_installs(&fake).unwrap()),
            pairs(&[("tree", "2.2.1-r0")])
        );
        assert_eq!(fake.calls()[1].args[3..], ["tree"]);
    }

    #[test]
    fn each_constraint_operator_is_cut_from_the_name() {
        let fake = FakeCommandRunner::new()
            .with_stdout("a=1-r0\nb<2\nc>1\nd~1.2\ne>=1\nf<=2\ng><abc\n")
            .with_output(exit(7, ""));
        assert_eq!(listed(user_installs(&fake).unwrap()), vec![]);
        assert_eq!(
            fake.calls()[1].args[3..],
            ["a", "b", "c", "d", "e", "f", "g"]
        );
    }

    #[test]
    fn a_name_that_prefixes_another_is_told_apart() {
        let fake = FakeCommandRunner::new()
            .with_stdout("py3\npy3-3to2\n")
            .with_output(exit(0, "py3-3to2-1.1.1-r0\npy3-3.12.8-r1\n"));
        assert_eq!(
            listed(user_installs(&fake).unwrap()),
            pairs(&[("py3-3to2", "1.1.1-r0"), ("py3", "3.12.8-r1")])
        );
    }

    #[test]
    fn a_world_package_apk_no_longer_has_is_skipped() {
        // apk info exits with the number of names it did not find.
        let fake = FakeCommandRunner::new()
            .with_stdout("jq\ngone\nalso-gone\n")
            .with_output(exit(2, "jq-1.8.2-r0\n"));
        assert_eq!(
            listed(user_installs(&fake).unwrap()),
            pairs(&[("jq", "1.8.2-r0")])
        );
    }

    #[test]
    fn apk_info_failing_beyond_missing_names_is_an_error() {
        let fake = FakeCommandRunner::new()
            .with_stdout("jq\n")
            .with_output(exit(2, ""));
        let err = user_installs(&fake).unwrap_err();
        assert!(
            matches!(err, PortError::CommandFailed { ref program, code: Some(2), .. } if program == "apk"),
            "{err:?}"
        );
        let fake = FakeCommandRunner::new()
            .with_stdout("jq\n")
            .with_output(Output {
                code: None,
                stdout: vec![],
                stderr: vec![],
            });
        assert!(user_installs(&fake).is_err());
    }

    #[test]
    fn an_unreadable_world_is_an_error() {
        let fake = FakeCommandRunner::new().with_output(exit(1, ""));
        let err = user_installs(&fake).unwrap_err();
        assert!(
            matches!(err, PortError::CommandFailed { ref program, code: Some(1), .. } if program == "cat"),
            "{err:?}"
        );
    }

    #[test]
    fn a_line_apk_info_should_not_print_is_an_error() {
        for line in ["jq\n", "jq-\n", "jq-1.0-1\n", "other-1.0-r0\n"] {
            let fake = FakeCommandRunner::new()
                .with_stdout("jq\n")
                .with_stdout(line);
            let err = user_installs(&fake).unwrap_err();
            assert!(
                matches!(err, PortError::CommandOutput { .. }),
                "{line:?}: {err:?}"
            );
        }
    }
}
