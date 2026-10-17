//! The npm backend (§5.2 language tier): platform-orthogonal, user-scope,
//! installs any published version of a Node tool from the npm registry.
//! Mutations are step lists (`npm install -g --prefix <prefix> name@ver`);
//! `verify_installed` reads `npm ls -g --depth=0 --json`; `query_available`
//! reads the registry's abbreviated metadata over HTTP (the query side may run
//! anywhere, §4.7).
//!
//! Global installs go to a user prefix: npm's configured prefix when it lies
//! in the user's home (nvm, fnm, `~/.npm-global`, `%APPDATA%\npm`), else
//! `~/.local`, whose `bin` is bosun's user bin dir. A system Node's own
//! prefix (`/usr/local`) is root's, and user-scope installs never run
//! elevated (§5.5). The prefix is read once, on first use, and logged.
//!
//! `npm -g` keeps no durable hold — a re-run re-asserts the exact version — so
//! `hold` is a no-op and holds are not needed (§5.2).

use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use bosun_core::{
    BackendId, Capabilities, Installed, Os, Ref, Scheme, Scope, Source, Target, Version,
};
use bosun_ports::{
    Backend, BackendFactory, Command, CommandRunner, HostInfo, HttpFetcher, HttpRequest, Index,
    Installer, PortError, Probe, Step, Steps, UserInstall, UserInstalls,
};
use tracing::debug;

use crate::logmsg;

const BACKEND: &str = "npm";

/// The registry's abbreviated ("corgi") metadata: just what installing needs.
const ABBREVIATED: &str = "application/vnd.npm.install-v1+json";

/// The packages Node itself ships in a global prefix; never the user's own.
const BUNDLED: [&str; 2] = ["npm", "corepack"];

/// Builds the npm backend when `npm` is present.
pub struct NpmFactory {
    runner: Arc<dyn CommandRunner>,
    fetcher: Arc<dyn HttpFetcher>,
    home: PathBuf,
    os: Os,
}

impl NpmFactory {
    /// Construct with the command runner and HTTP fetcher the backend uses,
    /// the user's home (where a user prefix must lie) and the OS bosun runs
    /// on (which names the program: `npm.cmd` on Windows).
    pub fn new(
        runner: Arc<dyn CommandRunner>,
        fetcher: Arc<dyn HttpFetcher>,
        home: PathBuf,
        os: Os,
    ) -> NpmFactory {
        NpmFactory {
            runner,
            fetcher,
            home,
            os,
        }
    }
}

/// The program to run: npm is a `.cmd` shim on Windows, which process
/// creation does not find by its bare name.
fn program(os: Os) -> &'static str {
    match os {
        Os::Windows => "npm.cmd",
        _ => "npm",
    }
}

impl BackendFactory for NpmFactory {
    fn meta(&self) -> bosun_ports::BackendMeta {
        bosun_ports::BackendMeta {
            purl_type: "npm",
            vers_type: "npm",
            platform_independent: true,
            option_keys: &[],
        }
    }

    fn id(&self) -> BackendId {
        BackendId::new(BACKEND)
    }

    fn probe(&self, host: &HostInfo) -> Result<Option<Probe>, PortError> {
        // npm is platform-orthogonal; present on any OS where it is on PATH.
        let out = host
            .runner
            .run(&Command::new(program(self.os), argv(&["--version"])))?;
        if !out.success() {
            return Ok(None);
        }
        let raw = out.stdout_utf8();
        let raw = raw.trim();
        let version =
            Version::parse(raw, Scheme::Semver).map_err(|e| PortError::CommandOutput {
                detail: format!("npm version {raw:?}: {e}"),
            })?;
        Ok(Some(Probe {
            backend: BackendId::new(BACKEND),
            version,
        }))
    }

    fn build(&self, _probe: Probe) -> Backend {
        let backend = NpmBackend {
            runner: self.runner.clone(),
            fetcher: self.fetcher.clone(),
            program: program(self.os),
            home: self.home.clone(),
            prefix: Arc::new(OnceLock::new()),
        };
        Backend {
            index: Box::new(backend.clone()),
            installer: Box::new(backend),
        }
    }
}

#[derive(Clone)]
struct NpmBackend {
    runner: Arc<dyn CommandRunner>,
    fetcher: Arc<dyn HttpFetcher>,
    program: &'static str,
    home: PathBuf,
    prefix: Arc<OnceLock<PathBuf>>,
}

fn argv(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|s| s.to_string()).collect()
}

/// The registry URL for a package's metadata; a scope's `/` is escaped
/// (`@types/node` → `@types%2Fnode`), as npm itself asks.
fn registry_url(name: &str) -> String {
    format!("https://registry.npmjs.org/{}", name.replace('/', "%2F"))
}

/// Where global installs go (see the module docs): npm's configured prefix
/// when it is inside `home`, else `home/.local`.
pub fn user_prefix(runner: &dyn CommandRunner, program: &str, home: &Path) -> PathBuf {
    let fallback = home.join(".local");
    let configured = runner
        .run(&Command::new(program, argv(&["config", "get", "prefix"])))
        .ok()
        .filter(|out| out.success())
        .map(|out| PathBuf::from(out.stdout_utf8().trim()));
    let prefix = match configured {
        Some(p) if !home.as_os_str().is_empty() && p.starts_with(home) => p,
        _ => fallback,
    };
    debug!(message = logmsg::NPM_PREFIX, prefix = %prefix.display());
    prefix
}

impl NpmBackend {
    fn prefix(&self) -> &Path {
        self.prefix
            .get_or_init(|| user_prefix(self.runner.as_ref(), self.program, &self.home))
    }

    /// `npm <verb> -g --prefix <prefix> <arg>`, as a user-scope step.
    fn global(&self, verb: &str, arg: &str) -> Steps {
        let prefix = self.prefix().display().to_string();
        vec![Step::Run {
            argv: argv(&[self.program, verb, "-g", "--prefix", &prefix, arg]),
            env: vec![],
            scope: Scope::User,
            ok_exits: Vec::new(),
        }]
    }
}

#[derive(serde::Deserialize)]
struct Packument {
    #[serde(default)]
    versions: std::collections::BTreeMap<String, serde_json::Value>,
}

impl Index for NpmBackend {
    fn refresh(&self) -> Result<(), PortError> {
        // The registry is asked fresh per query; nothing to refresh.
        Ok(())
    }

    fn query_available(&self, pkg: &Ref, _target: &Target) -> Result<Vec<Version>, PortError> {
        let url = registry_url(&pkg.purl.native_id());
        let mut request = HttpRequest::get(&url);
        request
            .headers
            .push(("Accept".to_string(), ABBREVIATED.to_string()));
        let resp = self.fetcher.fetch(&request)?;
        if resp.status == 404 {
            return Ok(vec![]); // unknown package
        }
        if !(200..300).contains(&resp.status) {
            return Err(PortError::HttpStatus {
                url,
                status: resp.status,
            });
        }
        let doc: Packument =
            serde_json::from_slice(&resp.body).map_err(|e| PortError::Backend {
                detail: format!("npm registry metadata for {url}: {e}"),
            })?;
        Ok(doc
            .versions
            .keys()
            .filter_map(|v| Version::parse(v, Scheme::Semver).ok())
            .collect())
    }
}

#[derive(serde::Deserialize)]
struct Ls {
    #[serde(default)]
    dependencies: std::collections::BTreeMap<String, LsPackage>,
    #[serde(default)]
    error: Option<LsError>,
}

#[derive(serde::Deserialize)]
struct LsPackage {
    #[serde(default)]
    version: Option<String>,
}

#[derive(serde::Deserialize)]
struct LsError {
    #[serde(default)]
    code: String,
}

/// Every package installed globally under `prefix`, by `npm ls -g --depth=0
/// --json`. A prefix npm has never installed into does not exist yet; that is
/// an empty list, not an error.
fn global_packages(
    runner: &dyn CommandRunner,
    program: &str,
    prefix: &Path,
) -> Result<Vec<UserInstall>, PortError> {
    let prefix = prefix.display().to_string();
    let args = argv(&["ls", "-g", "--depth=0", "--json", "--prefix", &prefix]);
    let out = runner.run(&Command::new(program, args))?;
    let failed = || PortError::CommandFailed {
        program: program.to_string(),
        code: out.code,
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    };
    let ls: Ls = match serde_json::from_slice(&out.stdout) {
        Ok(ls) => ls,
        Err(_) if !out.success() => return Err(failed()),
        Err(e) => {
            return Err(PortError::CommandOutput {
                detail: format!("npm ls -g --json: {e}"),
            })
        }
    };
    match ls.error {
        Some(error) if error.code == "ENOENT" => return Ok(vec![]),
        Some(_) => return Err(failed()),
        None => {}
    }
    let mut found = Vec::new();
    for (name, package) in ls.dependencies {
        // A dependency npm lists without a version is missing from disk.
        let Some(raw) = package.version else { continue };
        let version =
            Version::parse(&raw, Scheme::Semver).map_err(|e| PortError::CommandOutput {
                detail: format!("npm ls version {raw:?} of {name}: {e}"),
            })?;
        found.push(UserInstall {
            package: name,
            version,
        });
    }
    Ok(found)
}

/// What the user installed with npm (§9 state export): every global package
/// under `prefix`, less the ones Node ships itself (npm, corepack).
pub fn user_installs(
    runner: &dyn CommandRunner,
    program: &str,
    prefix: &Path,
) -> Result<UserInstalls, PortError> {
    let mut found = global_packages(runner, program, prefix)?;
    found.retain(|u| {
        let bundled = BUNDLED.contains(&u.package.as_str());
        if bundled {
            debug!(message = logmsg::NPM_BUNDLED_LEFT_OUT, package = %u.package);
        }
        !bundled
    });
    Ok(UserInstalls::Listed(found))
}

impl Installer for NpmBackend {
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
        // The public registry is the default; scoped registries are future.
        Ok(vec![])
    }

    fn install_exact(&self, pkg: &Ref, version: &Version) -> Result<Steps, PortError> {
        let at = format!("{}@{}", pkg.purl.native_id(), version.as_str());
        Ok(self.global("install", &at))
    }

    fn verify_installed(&self, pkg: &Ref) -> Result<Installed, PortError> {
        let name = pkg.purl.native_id();
        Ok(
            global_packages(self.runner.as_ref(), self.program, self.prefix())?
                .into_iter()
                .find(|u| u.package == name)
                .map_or(Installed::Absent, |u| Installed::Present {
                    version: Some(u.version),
                }),
        )
    }

    fn hold(&self, _pkg: &Ref, _version: &Version) -> Result<Steps, PortError> {
        Ok(vec![]) // no durable hold; bosun re-asserts the exact version each run
    }

    fn downgrade(&self, pkg: &Ref, version: &Version) -> Result<Steps, PortError> {
        // `npm install -g name@lower` replaces it with the lower version.
        self.install_exact(pkg, version)
    }

    fn user_installs(&self) -> Result<UserInstalls, PortError> {
        user_installs(self.runner.as_ref(), self.program, self.prefix())
    }

    fn uninstall(&self, pkg: &Ref) -> Result<Steps, PortError> {
        Ok(self.global("uninstall", &pkg.purl.native_id()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bosun_ports::testing::{FakeCommandRunner, FakeHttpFetcher};
    use bosun_ports::{HttpResponse, Output};

    const REGISTRY_COWSAY: &str = include_str!("../tests/fixtures/npm/registry-cowsay.json");
    const LS_GLOBAL: &str = include_str!("../tests/fixtures/npm/ls-global.json");
    const LS_NO_PREFIX: &str = include_str!("../tests/fixtures/npm/ls-no-prefix.json");
    const LS_NODE_IMAGE: &str = include_str!("../tests/fixtures/npm/ls-node-image.json");

    const PREFIX: &str = "/home/u/.local";

    fn sem(v: &str) -> Version {
        Version::parse(v, Scheme::Semver).unwrap()
    }

    fn reff(purl: &str) -> Ref {
        Ref::new(purl.parse().unwrap())
    }

    /// A backend whose prefix is already settled at [`PREFIX`].
    fn backend(runner: Arc<dyn CommandRunner>, fetcher: Arc<dyn HttpFetcher>) -> NpmBackend {
        let prefix = OnceLock::new();
        prefix.set(PathBuf::from(PREFIX)).unwrap();
        NpmBackend {
            runner,
            fetcher,
            program: "npm",
            home: PathBuf::from("/home/u"),
            prefix: Arc::new(prefix),
        }
    }

    fn quiet() -> NpmBackend {
        backend(
            Arc::new(FakeCommandRunner::new()),
            Arc::new(FakeHttpFetcher::new()),
        )
    }

    fn user_run(argv: &[&str]) -> Step {
        Step::Run {
            argv: argv.iter().map(|s| s.to_string()).collect(),
            env: vec![],
            scope: Scope::User,
            ok_exits: Vec::new(),
        }
    }

    fn failed(code: i32, stdout: &str) -> Output {
        Output {
            code: Some(code),
            stdout: stdout.as_bytes().to_vec(),
            stderr: b"npm error".to_vec(),
        }
    }

    #[test]
    fn install_and_downgrade_install_globally_into_the_prefix() {
        let b = quiet();
        let want = vec![user_run(&[
            "npm",
            "install",
            "-g",
            "--prefix",
            PREFIX,
            "cowsay@1.6.0",
        ])];
        let cowsay = reff("pkg:npm/cowsay");
        assert_eq!(b.install_exact(&cowsay, &sem("1.6.0")).unwrap(), want);
        assert_eq!(b.downgrade(&cowsay, &sem("1.6.0")).unwrap(), want);
    }

    #[test]
    fn a_scoped_package_is_named_with_its_scope() {
        let b = quiet();
        let biome = reff("pkg:npm/%40biomejs/biome");
        assert_eq!(
            b.install_exact(&biome, &sem("1.9.4")).unwrap(),
            vec![user_run(&[
                "npm",
                "install",
                "-g",
                "--prefix",
                PREFIX,
                "@biomejs/biome@1.9.4"
            ])]
        );
        assert_eq!(
            b.uninstall(&biome).unwrap(),
            vec![user_run(&[
                "npm",
                "uninstall",
                "-g",
                "--prefix",
                PREFIX,
                "@biomejs/biome"
            ])]
        );
    }

    #[test]
    fn hold_is_a_noop_and_holds_are_not_needed() {
        let b = quiet();
        assert_eq!(
            b.hold(&reff("pkg:npm/cowsay"), &sem("1.6.0")).unwrap(),
            Vec::<Step>::new()
        );
        let c = b.capabilities();
        assert!(c.supports_exact_version);
        assert_eq!(c.hold, bosun_core::HoldSupport::NotNeeded);
        assert!(c.supports_downgrade);
        assert_eq!(b.scope(), Scope::User);
        let registry = Source {
            backend: BackendId::new(BACKEND),
            id: bosun_core::SourceId::new("registry.npmjs.org"),
        };
        assert_eq!(b.ensure_source(&registry).unwrap(), vec![]);
    }

    #[test]
    fn verify_installed_reads_npm_ls_under_the_prefix() {
        let fake = Arc::new(
            FakeCommandRunner::new()
                .with_stdout(LS_GLOBAL)
                .with_stdout(LS_GLOBAL)
                .with_stdout(LS_GLOBAL),
        );
        let b = backend(fake.clone(), Arc::new(FakeHttpFetcher::new()));
        assert_eq!(
            b.verify_installed(&reff("pkg:npm/semver")).unwrap(),
            Installed::Present {
                version: Some(sem("7.6.0"))
            }
        );
        assert_eq!(
            b.verify_installed(&reff("pkg:npm/%40types/semver"))
                .unwrap(),
            Installed::Present {
                version: Some(sem("7.5.8"))
            }
        );
        assert_eq!(
            b.verify_installed(&reff("pkg:npm/left-pad")).unwrap(),
            Installed::Absent
        );
        assert_eq!(fake.calls()[0].program, "npm");
        assert_eq!(
            fake.calls()[0].args,
            vec!["ls", "-g", "--depth=0", "--json", "--prefix", PREFIX]
        );
    }

    #[test]
    fn a_prefix_npm_never_installed_into_holds_nothing() {
        let fake = Arc::new(FakeCommandRunner::new().with_output(failed(254, LS_NO_PREFIX)));
        let b = backend(fake, Arc::new(FakeHttpFetcher::new()));
        assert_eq!(
            b.verify_installed(&reff("pkg:npm/cowsay")).unwrap(),
            Installed::Absent
        );
    }

    #[test]
    fn npm_ls_failing_otherwise_is_an_error() {
        let other = r#"{"error": {"code": "EACCES", "summary": "permission denied"}}"#;
        for out in [failed(1, other), failed(1, "not json")] {
            let err = global_packages(
                &FakeCommandRunner::new().with_output(out),
                "npm",
                Path::new(PREFIX),
            )
            .unwrap_err();
            assert!(
                matches!(err, PortError::CommandFailed { ref program, code: Some(1), .. } if program == "npm"),
                "{err:?}"
            );
        }
    }

    #[test]
    fn npm_ls_output_npm_should_not_print_is_an_error() {
        for stdout in ["not json", r#"{"dependencies": {"x": {"version": "one"}}}"#] {
            let fake = FakeCommandRunner::new().with_stdout(stdout);
            let err = global_packages(&fake, "npm", Path::new(PREFIX)).unwrap_err();
            assert!(
                matches!(err, PortError::CommandOutput { .. }),
                "{stdout}: {err:?}"
            );
        }
    }

    #[test]
    fn npm_ls_problems_still_list_what_is_installed() {
        // npm exits 1 when the tree has problems (an extraneous or missing
        // dependency) but still lists it; a package with no version is
        // missing from disk.
        let ls = r#"{"problems": ["missing: x@1"], "dependencies": {"cowsay": {"version": "1.6.0"}, "x": {"required": "1"}}}"#;
        let fake = FakeCommandRunner::new().with_output(failed(1, ls));
        let found = global_packages(&fake, "npm", Path::new(PREFIX)).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].package, "cowsay");
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
    fn user_installs_are_the_global_packages() {
        let fake = Arc::new(FakeCommandRunner::new().with_stdout(LS_GLOBAL));
        let b = backend(fake.clone(), Arc::new(FakeHttpFetcher::new()));
        let want: Vec<(String, String)> = [
            ("@types/semver", "7.5.8"),
            ("cowsay", "1.6.0"),
            ("semver", "7.6.0"),
        ]
        .iter()
        .map(|(p, v)| (p.to_string(), v.to_string()))
        .collect();
        assert_eq!(listed(b.user_installs().unwrap()), want);
        assert_eq!(
            fake.calls()[0].args,
            vec!["ls", "-g", "--depth=0", "--json", "--prefix", PREFIX]
        );
    }

    #[test]
    fn what_node_ships_is_left_out_of_the_export() {
        let fake = FakeCommandRunner::new().with_stdout(LS_NODE_IMAGE);
        assert_eq!(
            listed(user_installs(&fake, "npm", Path::new("/usr/local")).unwrap()),
            vec![]
        );
    }

    #[test]
    fn query_available_reads_the_abbreviated_registry_metadata() {
        let fetcher = Arc::new(
            FakeHttpFetcher::new().with_body("https://registry.npmjs.org/cowsay", REGISTRY_COWSAY),
        );
        let got = backend(Arc::new(FakeCommandRunner::new()), fetcher.clone())
            .query_available(&reff("pkg:npm/cowsay"), &bosun_core::Target::Any)
            .unwrap();
        assert_eq!(got.len(), 21);
        assert!(got.contains(&sem("1.6.0")));
        assert!(got.contains(&sem("1.0.0")));
        assert_eq!(
            fetcher.calls()[0].headers,
            vec![("Accept".to_string(), ABBREVIATED.to_string())]
        );
    }

    #[test]
    fn query_available_escapes_a_scope_and_skips_what_is_not_semver() {
        let body = r#"{"versions": {"1.0.0": {}, "1.0": {}, "2.0.0-rc.1": {}}}"#;
        let fetcher = Arc::new(
            FakeHttpFetcher::new().with_body("https://registry.npmjs.org/@types%2Fsemver", body),
        );
        let got = backend(Arc::new(FakeCommandRunner::new()), fetcher)
            .query_available(&reff("pkg:npm/%40types/semver"), &bosun_core::Target::Any)
            .unwrap();
        assert_eq!(got, vec![sem("1.0.0"), sem("2.0.0-rc.1")]);
    }

    #[test]
    fn an_unknown_package_has_no_versions_and_other_statuses_are_errors() {
        let url = "https://registry.npmjs.org/nope";
        let status = |status| {
            let fetcher = FakeHttpFetcher::new().with_response(
                url,
                HttpResponse {
                    status,
                    body: vec![],
                },
            );
            backend(Arc::new(FakeCommandRunner::new()), Arc::new(fetcher))
                .query_available(&reff("pkg:npm/nope"), &bosun_core::Target::Any)
        };
        assert_eq!(status(404).unwrap(), vec![]);
        for code in [199, 300, 500] {
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
            .query_available(&reff("pkg:npm/nope"), &bosun_core::Target::Any),
            Err(PortError::Backend { .. })
        ));
    }

    #[test]
    fn the_prefix_is_npms_own_inside_home_else_dot_local() {
        let home = Path::new("/home/u");
        let asked = |stdout: &str| {
            let fake = FakeCommandRunner::new().with_stdout(stdout);
            let prefix = user_prefix(&fake, "npm", home);
            assert_eq!(fake.calls()[0].args, vec!["config", "get", "prefix"]);
            prefix
        };
        assert_eq!(
            asked("/home/u/.nvm/versions/node/v22.11.0\n"),
            PathBuf::from("/home/u/.nvm/versions/node/v22.11.0")
        );
        assert_eq!(asked("/usr/local\n"), PathBuf::from("/home/u/.local"));
        // npm failing, or no home to compare with, falls back too.
        let failing = FakeCommandRunner::new().with_output(failed(1, ""));
        assert_eq!(
            user_prefix(&failing, "npm", home),
            PathBuf::from("/home/u/.local")
        );
        let anywhere = FakeCommandRunner::new().with_stdout("/usr/local\n");
        assert_eq!(
            user_prefix(&anywhere, "npm", Path::new("")),
            PathBuf::from(".local")
        );
    }

    #[test]
    fn the_prefix_is_asked_once() {
        let fake = Arc::new(
            FakeCommandRunner::new()
                .with_stdout("/usr/local\n")
                .with_stdout(LS_GLOBAL)
                .with_stdout(LS_GLOBAL),
        );
        let factory = NpmFactory::new(
            fake.clone(),
            Arc::new(FakeHttpFetcher::new()),
            PathBuf::from("/home/u"),
            Os::Linux,
        );
        let built = factory.build(Probe {
            backend: BackendId::new(BACKEND),
            version: sem("10.9.9"),
        });
        let cowsay = reff("pkg:npm/cowsay");
        built.installer.verify_installed(&cowsay).unwrap();
        built.installer.verify_installed(&cowsay).unwrap();
        let calls = fake.calls();
        assert_eq!(calls.len(), 3);
        assert_eq!(calls[1].args.last().map(String::as_str), Some(PREFIX));
        assert_eq!(calls[2].args.last().map(String::as_str), Some(PREFIX));
    }

    #[test]
    fn probe_reads_the_version_on_any_os_and_names_the_windows_shim() {
        for (os, program) in [(Os::Macos, "npm"), (Os::Windows, "npm.cmd")] {
            let factory = NpmFactory::new(
                Arc::new(FakeCommandRunner::new()),
                Arc::new(FakeHttpFetcher::new()),
                PathBuf::from("/home/u"),
                os,
            );
            let runner = FakeCommandRunner::new().with_stdout("10.9.9\n");
            let host = HostInfo {
                os,
                arch: bosun_core::Arch::Arm64,
                distro: None,
                runner: &runner,
            };
            let probe = factory.probe(&host).unwrap().unwrap();
            assert_eq!(probe.version, sem("10.9.9"));
            assert_eq!(runner.calls()[0].program, program);
            assert_eq!(runner.calls()[0].args, vec!["--version"]);
        }
    }

    #[test]
    fn probe_without_a_working_npm_is_absent_or_an_error() {
        let factory = NpmFactory::new(
            Arc::new(FakeCommandRunner::new()),
            Arc::new(FakeHttpFetcher::new()),
            PathBuf::from("/home/u"),
            Os::Linux,
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
        assert_eq!(probe(failed(1, "")).unwrap(), None);
        assert!(matches!(
            probe(Output {
                code: Some(0),
                stdout: b"ten\n".to_vec(),
                stderr: vec![],
            }),
            Err(PortError::CommandOutput { .. })
        ));
    }

    #[test]
    fn metadata_is_npm_and_platform_independent() {
        let factory = NpmFactory::new(
            Arc::new(FakeCommandRunner::new()),
            Arc::new(FakeHttpFetcher::new()),
            PathBuf::from("/home/u"),
            Os::Linux,
        );
        let meta = factory.meta();
        assert_eq!(meta.purl_type, "npm");
        assert_eq!(meta.vers_type, "npm");
        assert!(meta.platform_independent);
        assert_eq!(factory.id(), BackendId::new("npm"));
    }
}
