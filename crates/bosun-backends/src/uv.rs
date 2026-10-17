//! The uv backend (§5.2 language tier): platform-orthogonal, user-scope,
//! installs any published version of a Python tool from PyPI into its own
//! environment. Mutations are step lists (`uv tool install name==ver`, which
//! also moves an installed tool up or down); `verify_installed` reads `uv tool
//! list`; `query_available` reads PyPI's JSON API over HTTP (the query side
//! may run anywhere, §4.7). Versions are PEP 440 (vers type `pypi`).
//!
//! uv places the tool's executables itself (`~/.local/bin` unless configured
//! otherwise). A tool keeps no durable hold — a re-run re-asserts the exact
//! version — so `hold` is a no-op and holds are not needed (§5.2).

use std::collections::BTreeMap;
use std::sync::Arc;

use bosun_core::{BackendId, Capabilities, Installed, Ref, Scheme, Scope, Source, Target, Version};
use bosun_ports::{
    Backend, BackendFactory, Command, CommandRunner, HostInfo, HttpFetcher, HttpRequest, Index,
    Installer, PortError, Probe, Step, Steps, UserInstall, UserInstalls,
};
use tracing::debug;

use crate::logmsg;

const BACKEND: &str = "uv";

/// Builds the uv backend when `uv` is present.
pub struct UvFactory {
    runner: Arc<dyn CommandRunner>,
    fetcher: Arc<dyn HttpFetcher>,
}

impl UvFactory {
    /// Construct with the command runner and HTTP fetcher the backend uses.
    pub fn new(runner: Arc<dyn CommandRunner>, fetcher: Arc<dyn HttpFetcher>) -> UvFactory {
        UvFactory { runner, fetcher }
    }
}

impl BackendFactory for UvFactory {
    fn meta(&self) -> bosun_ports::BackendMeta {
        bosun_ports::BackendMeta {
            purl_type: "pypi",
            vers_type: "pypi",
            platform_independent: true,
            option_keys: &[],
        }
    }

    fn id(&self) -> BackendId {
        BackendId::new(BACKEND)
    }

    fn probe(&self, host: &HostInfo) -> Result<Option<Probe>, PortError> {
        // uv is platform-orthogonal; present on any OS where it is on PATH.
        let out = host.runner.run(&Command::new("uv", argv(&["--version"])))?;
        if !out.success() {
            return Ok(None);
        }
        Ok(Some(Probe {
            backend: BackendId::new(BACKEND),
            version: parse_uv_version(&out.stdout_utf8())?,
        }))
    }

    fn build(&self, _probe: Probe) -> Backend {
        let backend = UvBackend {
            runner: self.runner.clone(),
            fetcher: self.fetcher.clone(),
        };
        Backend {
            index: Box::new(backend.clone()),
            installer: Box::new(backend),
        }
    }
}

/// `uv 0.13.1 (x86_64-unknown-linux-musl)` → `0.13.1`.
fn parse_uv_version(stdout: &str) -> Result<Version, PortError> {
    let word = stdout
        .split_whitespace()
        .nth(1)
        .ok_or_else(|| PortError::CommandOutput {
            detail: format!("could not read uv version from {stdout:?}"),
        })?;
    Version::parse(word, Scheme::Semver).map_err(|e| PortError::CommandOutput {
        detail: format!("uv version {word:?}: {e}"),
    })
}

#[derive(Clone)]
struct UvBackend {
    runner: Arc<dyn CommandRunner>,
    fetcher: Arc<dyn HttpFetcher>,
}

fn argv(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|s| s.to_string()).collect()
}

fn user_run(parts: &[&str]) -> Steps {
    vec![Step::Run {
        argv: argv(parts),
        env: vec![],
        scope: Scope::User,
        ok_exits: Vec::new(),
    }]
}

/// A project name as PyPI compares it (PEP 503): lowercased, with each run of
/// `-`, `_` and `.` one `-`.
fn normalize(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut separating = false;
    for c in name.chars() {
        if matches!(c, '-' | '_' | '.') {
            separating = true;
            continue;
        }
        if separating && !out.is_empty() {
            out.push('-');
        }
        separating = false;
        out.push(c.to_ascii_lowercase());
    }
    out
}

/// PyPI's JSON API for a project: every release and its files.
#[derive(serde::Deserialize)]
struct Project {
    #[serde(default)]
    releases: BTreeMap<String, Vec<ReleaseFile>>,
}

#[derive(serde::Deserialize)]
struct ReleaseFile {
    #[serde(default)]
    yanked: bool,
}

impl Index for UvBackend {
    fn refresh(&self) -> Result<(), PortError> {
        // PyPI is asked fresh per query; nothing to refresh.
        Ok(())
    }

    /// The releases with at least one file not yanked: a release with no
    /// files has nothing to install, and an installer skips a yanked one
    /// unless pinned to it (PEP 592). A version outside PEP 440 is skipped.
    fn query_available(&self, pkg: &Ref, _target: &Target) -> Result<Vec<Version>, PortError> {
        let url = format!("https://pypi.org/pypi/{}/json", pkg.purl.native_id());
        let resp = self.fetcher.fetch(&HttpRequest::get(&url))?;
        if resp.status == 404 {
            return Ok(vec![]); // unknown project
        }
        if !(200..300).contains(&resp.status) {
            return Err(PortError::HttpStatus {
                url,
                status: resp.status,
            });
        }
        let project: Project =
            serde_json::from_slice(&resp.body).map_err(|e| PortError::Backend {
                detail: format!("PyPI project metadata for {url}: {e}"),
            })?;
        Ok(project
            .releases
            .into_iter()
            .filter(|(_, files)| files.iter().any(|f| !f.yanked))
            .filter_map(|(v, _)| Version::parse(v, Scheme::Pep440).ok())
            .collect())
    }
}

/// One tool in `uv tool list`: its name, version and, with
/// `--show-version-specifiers`, what it was installed from.
struct Tool<'a> {
    name: &'a str,
    version: Version,
    required: Option<&'a str>,
}

/// The tools in `uv tool list` output: `name vX.Y [required: …] [with: …]`
/// lines, each followed by `- executable` lines. Any other line (`No tools
/// installed`) names no tool.
fn tools(stdout: &str) -> Result<Vec<Tool<'_>>, PortError> {
    let mut found = Vec::new();
    for line in stdout.lines().filter(|l| !l.starts_with('-')) {
        let mut words = line.split_whitespace();
        let (Some(name), Some(raw)) = (words.next(), words.next()) else {
            continue;
        };
        let Some(raw) = raw.strip_prefix('v') else {
            continue;
        };
        let version =
            Version::parse(raw, Scheme::Pep440).map_err(|e| PortError::CommandOutput {
                detail: format!("uv tool list version {raw:?} of {name}: {e}"),
            })?;
        let required = line
            .split_once("[required: ")
            .and_then(|(_, rest)| rest.split_once(']'))
            .map(|(spec, _)| spec);
        found.push(Tool {
            name,
            version,
            required,
        });
    }
    Ok(found)
}

/// Run `uv tool list` with `extra` arguments, failing on a non-zero exit.
fn tool_list(runner: &dyn CommandRunner, extra: &[&str]) -> Result<String, PortError> {
    let mut args = argv(&["tool", "list"]);
    args.extend(argv(extra));
    let out = runner.run(&Command::new("uv", args))?;
    if !out.success() {
        return Err(PortError::CommandFailed {
            program: "uv".to_string(),
            code: out.code,
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        });
    }
    Ok(out.stdout_utf8().into_owned())
}

/// What the user installed with uv (§9 state export): every tool in `uv tool
/// list`. One installed from a path or URL (`[required: file:///…]`) is left
/// out: bosun installs from PyPI, which may not have it. A version specifier
/// (`[required: ==0.6.9]`) or none at all means PyPI.
pub fn user_installs(runner: &dyn CommandRunner) -> Result<UserInstalls, PortError> {
    let stdout = tool_list(runner, &["--show-version-specifiers"])?;
    let mut found = Vec::new();
    for tool in tools(&stdout)? {
        let from_pypi = tool
            .required
            .is_none_or(|spec| spec.starts_with(['=', '<', '>', '!', '~']));
        if !from_pypi {
            debug!(message = logmsg::NOT_FROM_PYPI, tool = tool.name);
            continue;
        }
        found.push(UserInstall {
            package: tool.name.to_string(),
            version: tool.version,
        });
    }
    Ok(UserInstalls::Listed(found))
}

impl Installer for UvBackend {
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            supports_exact_version: true,
            // Reinstalls the exact version each run, so a hold is unnecessary (§5.2).
            hold: bosun_core::HoldSupport::NotNeeded,
            supports_downgrade: true, // installing a lower version replaces it
        }
    }

    fn scope(&self) -> Scope {
        Scope::User
    }

    fn ensure_source(&self, _source: &Source) -> Result<Steps, PortError> {
        // PyPI is the default index; extra indexes are future.
        Ok(vec![])
    }

    fn install_exact(&self, pkg: &Ref, version: &Version) -> Result<Steps, PortError> {
        let requirement = format!("{}=={}", pkg.purl.native_id(), version.as_str());
        Ok(user_run(&["uv", "tool", "install", &requirement]))
    }

    fn verify_installed(&self, pkg: &Ref) -> Result<Installed, PortError> {
        let stdout = tool_list(self.runner.as_ref(), &[])?;
        let name = normalize(&pkg.purl.native_id());
        Ok(tools(&stdout)?
            .into_iter()
            .find(|t| normalize(t.name) == name)
            .map_or(Installed::Absent, |t| Installed::Present {
                version: Some(t.version),
            }))
    }

    fn hold(&self, _pkg: &Ref, _version: &Version) -> Result<Steps, PortError> {
        Ok(vec![]) // no durable hold; bosun re-asserts the exact version each run
    }

    fn downgrade(&self, pkg: &Ref, version: &Version) -> Result<Steps, PortError> {
        // `uv tool install name==lower` replaces the installed version.
        self.install_exact(pkg, version)
    }

    fn user_installs(&self) -> Result<UserInstalls, PortError> {
        user_installs(self.runner.as_ref())
    }

    fn uninstall(&self, pkg: &Ref) -> Result<Steps, PortError> {
        Ok(user_run(&[
            "uv",
            "tool",
            "uninstall",
            &pkg.purl.native_id(),
        ]))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bosun_core::Os;
    use bosun_ports::testing::{FakeCommandRunner, FakeHttpFetcher};
    use bosun_ports::{HttpResponse, Output};

    const PYPI_HTTPIE: &str = include_str!("../tests/fixtures/uv/pypi-httpie.json");
    const TOOL_LIST: &str = include_str!("../tests/fixtures/uv/tool-list.txt");
    const TOOL_LIST_SPECIFIERS: &str =
        include_str!("../tests/fixtures/uv/tool-list-specifiers.txt");

    fn pep(v: &str) -> Version {
        Version::parse(v, Scheme::Pep440).unwrap()
    }

    fn reff(name: &str) -> Ref {
        Ref::new(format!("pkg:pypi/{name}").parse().unwrap())
    }

    fn backend(runner: Arc<dyn CommandRunner>, fetcher: Arc<dyn HttpFetcher>) -> UvBackend {
        UvBackend { runner, fetcher }
    }

    fn quiet() -> UvBackend {
        backend(
            Arc::new(FakeCommandRunner::new()),
            Arc::new(FakeHttpFetcher::new()),
        )
    }

    /// One user-scope `Run` step, written out rather than built by the
    /// code under test.
    fn expected(argv: &[&str]) -> Steps {
        vec![Step::Run {
            argv: argv.iter().map(|s| s.to_string()).collect(),
            env: vec![],
            scope: Scope::User,
            ok_exits: Vec::new(),
        }]
    }

    fn failed(code: i32) -> Output {
        Output {
            code: Some(code),
            stdout: vec![],
            stderr: b"error: boom".to_vec(),
        }
    }

    #[test]
    fn install_and_downgrade_install_the_exact_version() {
        let b = quiet();
        let want = expected(&["uv", "tool", "install", "ruff==0.6.9"]);
        assert_eq!(b.install_exact(&reff("ruff"), &pep("0.6.9")).unwrap(), want);
        assert_eq!(b.downgrade(&reff("ruff"), &pep("0.6.9")).unwrap(), want);
    }

    #[test]
    fn uninstall_argv_and_hold_is_a_noop() {
        let b = quiet();
        assert_eq!(
            b.uninstall(&reff("httpie")).unwrap(),
            expected(&["uv", "tool", "uninstall", "httpie"])
        );
        assert_eq!(
            b.hold(&reff("httpie"), &pep("3.2.3")).unwrap(),
            Vec::<Step>::new()
        );
    }

    #[test]
    fn capabilities_language_tier() {
        let b = quiet();
        let c = b.capabilities();
        assert!(c.supports_exact_version);
        assert_eq!(c.hold, bosun_core::HoldSupport::NotNeeded);
        assert!(c.supports_downgrade);
        assert_eq!(b.scope(), Scope::User);
        let pypi = Source {
            backend: BackendId::new(BACKEND),
            id: bosun_core::SourceId::new("pypi.org"),
        };
        assert_eq!(b.ensure_source(&pypi).unwrap(), vec![]);
    }

    #[test]
    fn verify_installed_reads_uv_tool_list() {
        let fake = Arc::new(
            FakeCommandRunner::new()
                .with_stdout(TOOL_LIST)
                .with_stdout(TOOL_LIST)
                .with_stdout(TOOL_LIST),
        );
        let b = backend(fake.clone(), Arc::new(FakeHttpFetcher::new()));
        assert_eq!(
            b.verify_installed(&reff("black")).unwrap(),
            Installed::Present {
                version: Some(pep("24.1a1"))
            }
        );
        // Asked for as written; uv lists the normalized name.
        assert_eq!(
            b.verify_installed(&reff("Django.Upgrade")).unwrap(),
            Installed::Present {
                version: Some(pep("1.20.0"))
            }
        );
        // An executable is not a tool.
        assert_eq!(
            b.verify_installed(&reff("http")).unwrap(),
            Installed::Absent
        );
        assert_eq!(fake.calls()[0].program, "uv");
        assert_eq!(fake.calls()[0].args, vec!["tool", "list"]);
    }

    #[test]
    fn no_tools_installed_is_absent() {
        let fake = Arc::new(FakeCommandRunner::new().with_stdout("No tools installed\n"));
        let b = backend(fake, Arc::new(FakeHttpFetcher::new()));
        assert_eq!(
            b.verify_installed(&reff("ruff")).unwrap(),
            Installed::Absent
        );
    }

    #[test]
    fn uv_tool_list_failing_is_an_error() {
        let fake = Arc::new(FakeCommandRunner::new().with_output(failed(2)));
        let err = backend(fake, Arc::new(FakeHttpFetcher::new()))
            .verify_installed(&reff("ruff"))
            .unwrap_err();
        assert!(
            matches!(err, PortError::CommandFailed { ref program, code: Some(2), ref stderr } if program == "uv" && stderr == "error: boom"),
            "{err:?}"
        );
    }

    #[test]
    fn a_tool_line_uv_should_not_print_is_an_error() {
        let err = tools("ruff vnope\n- ruff\n").err().unwrap();
        assert!(matches!(err, PortError::CommandOutput { .. }), "{err:?}");
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

    #[test]
    fn user_installs_are_the_tools_from_pypi() {
        // localtool came from a directory, not PyPI.
        let fake = FakeCommandRunner::new().with_stdout(TOOL_LIST_SPECIFIERS);
        let want: Vec<(String, String)> = [
            ("black", "24.1a1"),
            ("pycowsay", "0.0.0.2"),
            ("ruff", "0.6.9"),
        ]
        .iter()
        .map(|(p, v)| (p.to_string(), v.to_string()))
        .collect();
        assert_eq!(listed(user_installs(&fake).unwrap()), want);
        assert_eq!(
            fake.calls()[0].args,
            vec!["tool", "list", "--show-version-specifiers"]
        );
    }

    #[test]
    fn every_specifier_operator_means_pypi() {
        for spec in ["==1.0", "<2", ">=1.0", "!=1.1", "~=1.0"] {
            let line = format!("tool v1.0 [required: {spec}]\n- tool\n");
            let fake = FakeCommandRunner::new().with_stdout(line);
            assert_eq!(listed(user_installs(&fake).unwrap()).len(), 1, "{spec}");
        }
        let url = "tool v1.0 [required: https://example.com/tool-1.0.tar.gz]\n";
        let fake = FakeCommandRunner::new().with_stdout(url);
        assert_eq!(listed(user_installs(&fake).unwrap()), vec![]);
    }

    #[test]
    fn the_backend_exports_through_uv_tool_list() {
        let fake = Arc::new(FakeCommandRunner::new().with_stdout(TOOL_LIST_SPECIFIERS));
        let b = backend(fake.clone(), Arc::new(FakeHttpFetcher::new()));
        assert_eq!(listed(b.user_installs().unwrap()).len(), 3);
        let err = user_installs(&FakeCommandRunner::new().with_output(failed(2))).unwrap_err();
        assert!(matches!(err, PortError::CommandFailed { .. }), "{err:?}");
    }

    #[test]
    fn query_available_skips_empty_yanked_and_non_pep440_releases() {
        let fetcher = Arc::new(
            FakeHttpFetcher::new().with_body("https://pypi.org/pypi/httpie/json", PYPI_HTTPIE),
        );
        let got = backend(Arc::new(FakeCommandRunner::new()), fetcher)
            .query_available(&reff("httpie"), &Target::Any)
            .unwrap();
        // 55 releases: 0.9.5 has no files and 2.0.0 is yanked.
        assert_eq!(got.len(), 53);
        assert!(got.contains(&pep("3.2.4")));
        assert!(got.contains(&pep("0.2.4.dev0"))); // `0.2.4dev`, a pre-release
        assert!(!got.contains(&pep("0.9.5")));
        assert!(!got.contains(&pep("2.0.0")));

        let body = r#"{"releases": {"1.0": [{"yanked": false}], "2004d": [{"yanked": false}], "1.1": [{"yanked": true}, {"yanked": false}]}}"#;
        let fetcher =
            Arc::new(FakeHttpFetcher::new().with_body("https://pypi.org/pypi/x/json", body));
        let got = backend(Arc::new(FakeCommandRunner::new()), fetcher)
            .query_available(&reff("x"), &Target::Any)
            .unwrap();
        assert_eq!(got, vec![pep("1.0"), pep("1.1")]);
    }

    #[test]
    fn an_unknown_project_has_no_versions_and_other_statuses_are_errors() {
        let url = "https://pypi.org/pypi/nope/json";
        let status = |status| {
            let fetcher = FakeHttpFetcher::new().with_response(
                url,
                HttpResponse {
                    status,
                    body: vec![],
                },
            );
            backend(Arc::new(FakeCommandRunner::new()), Arc::new(fetcher))
                .query_available(&reff("nope"), &Target::Any)
        };
        assert_eq!(status(404).unwrap(), vec![]);
        for code in [199, 300, 503] {
            assert!(
                matches!(status(code), Err(PortError::HttpStatus { status, .. }) if status == code),
                "{code}"
            );
        }
        assert!(matches!(
            backend(
                Arc::new(FakeCommandRunner::new()),
                Arc::new(FakeHttpFetcher::new().with_body(url, "<html>")),
            )
            .query_available(&reff("nope"), &Target::Any),
            Err(PortError::Backend { .. })
        ));
    }

    #[test]
    fn names_compare_as_pep_503_normalizes_them() {
        assert_eq!(normalize("Django_Upgrade"), "django-upgrade");
        assert_eq!(normalize("zope.interface"), "zope-interface");
        assert_eq!(normalize("a-_.b"), "a-b");
        assert_eq!(normalize("ruff"), "ruff");
    }

    #[test]
    fn probe_reads_version_on_any_os() {
        let factory = UvFactory::new(
            Arc::new(FakeCommandRunner::new()),
            Arc::new(FakeHttpFetcher::new()),
        );
        let runner =
            FakeCommandRunner::new().with_stdout("uv 0.13.1 (x86_64-unknown-linux-musl)\n");
        let host = HostInfo {
            os: Os::Macos,
            arch: bosun_core::Arch::Arm64,
            distro: None,
            runner: &runner,
        };
        let probe = factory.probe(&host).unwrap().unwrap();
        assert_eq!(
            probe.version,
            Version::parse("0.13.1", Scheme::Semver).unwrap()
        );
        assert_eq!(runner.calls()[0].program, "uv");
        assert_eq!(runner.calls()[0].args, vec!["--version"]);
        assert_eq!(factory.id(), BackendId::new("uv"));
        let meta = factory.meta();
        assert_eq!((meta.purl_type, meta.vers_type), ("pypi", "pypi"));
        assert!(meta.platform_independent);
    }

    #[test]
    fn probe_without_a_working_uv_is_absent_or_an_error() {
        let factory = UvFactory::new(
            Arc::new(FakeCommandRunner::new()),
            Arc::new(FakeHttpFetcher::new()),
        );
        let probe = |out: Output| {
            let runner = FakeCommandRunner::new().with_output(out);
            let host = HostInfo {
                os: Os::Linux,
                arch: bosun_core::Arch::Amd64,
                distro: None,
                runner: &runner,
            };
            factory.probe(&host)
        };
        assert_eq!(probe(failed(1)).unwrap(), None);
        for stdout in ["uv\n", "uv nope\n"] {
            let out = Output {
                code: Some(0),
                stdout: stdout.as_bytes().to_vec(),
                stderr: vec![],
            };
            assert!(
                matches!(probe(out), Err(PortError::CommandOutput { .. })),
                "{stdout}"
            );
        }
    }

    #[test]
    fn a_built_backend_answers_both_sides() {
        let factory = UvFactory::new(
            Arc::new(FakeCommandRunner::new().with_stdout(TOOL_LIST)),
            Arc::new(FakeHttpFetcher::new()),
        );
        let built = factory.build(Probe {
            backend: BackendId::new(BACKEND),
            version: Version::parse("0.13.1", Scheme::Semver).unwrap(),
        });
        assert_eq!(
            built.installer.verify_installed(&reff("ruff")).unwrap(),
            Installed::Present {
                version: Some(pep("0.6.8"))
            }
        );
        built.index.refresh().unwrap();
    }
}
