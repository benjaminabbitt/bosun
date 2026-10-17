//! A test-only seam (set `BOSUN_FIXTURE=<path>`): describe the host facts and a
//! set of backends with canned available versions, so the real `bosun` binary
//! can be driven in integration tests without touching apt/cargo/the network.
//! Production runs ignore it and probe the real host.
//!
//! A backend marked `real` is bosun's own compiled-in backend (github-release,
//! where bosun is the installer) run against canned HTTP responses (`http`:
//! URL to a local file) and a scratch `home` (binaries go to its
//! `.local/bin`, as on a real host), so its asset picking, checksum lookup
//! and extraction run for real without the network.
//!
//! A compiled-in backend named in `absent` is probed for real on a machine
//! where no program exists, so `bosun backends` reports it as a real host
//! without that package manager would.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use bosun_core::vers::scheme_for_type;
use bosun_core::{
    Capabilities, Checksum, HoldSupport, Installed, Ref, Scope, Source, Target, Version,
};
use bosun_host::StepExecutor;
use bosun_ports::backend::{Index, Installer};
use bosun_ports::step::{Step, StepRunner, Steps};
use bosun_ports::testing::FakeCommandRunner;
use bosun_ports::{Command, CommandRunner, HttpFetcher, Output, PortError, UserInstalls};
use serde::Deserialize;

/// The fixture's in-memory machine: native id → installed version. Steps mutate
/// it and `verify_installed` reads it, so a plan sees what is present and apply
/// updates it — exactly as a real host behaves (seeded from each backend's
/// `installed` map, the packages already present before the run).
pub type Machine = Rc<RefCell<BTreeMap<String, String>>>;

/// The whole fixture.
#[derive(Deserialize)]
pub struct Fixture {
    /// The OS token (`linux`).
    pub os: String,
    /// The arch token (`amd64`).
    pub arch: String,
    /// Distro facts, when any.
    #[serde(default)]
    pub distro: Option<FixtureDistro>,
    /// The C library, when known.
    #[serde(default)]
    pub libc: Option<FixtureLibc>,
    /// `elevated` or `unelevated` (default); drives the privilege check.
    #[serde(default)]
    pub privilege: Option<String>,
    /// The invoking user name (default `tester`); stamped on system claims.
    #[serde(default)]
    pub user: Option<String>,
    /// A step runner fails any `Run` step whose argv contains this marker, to
    /// drive a crash-mid-apply test.
    #[serde(default)]
    pub fail_step: Option<String>,
    /// The backends present in this fixture.
    pub backends: Vec<FixtureBackend>,
    /// Canned HTTP responses for `real` backends: URL to the file served as
    /// its 200 body. Any other URL fails.
    #[serde(default)]
    pub http: BTreeMap<String, PathBuf>,
    /// The invoking user's home directory; `real` backends install binaries
    /// into its `.local/bin`, as they do on a real host.
    #[serde(default)]
    pub home: Option<PathBuf>,
    /// Compiled-in backends this machine lacks the program for.
    #[serde(default)]
    pub absent: Vec<String>,
}

/// Fixture distro facts (§5.4).
#[derive(Deserialize)]
pub struct FixtureDistro {
    /// The `ID`.
    pub id: String,
    /// The `ID_LIKE` families.
    #[serde(default)]
    pub id_like: Vec<String>,
    /// The `VERSION_CODENAME`.
    #[serde(default)]
    pub codename: Option<String>,
    /// The `VERSION_ID`.
    #[serde(default)]
    pub version_id: Option<String>,
}

/// Fixture C library facts (§5.4).
#[derive(Deserialize)]
pub struct FixtureLibc {
    /// The family: `gnu` or `musl`.
    pub family: String,
    /// The version the loader reports.
    #[serde(default)]
    pub version: Option<String>,
}

/// One fixture backend, with the metadata the resolver needs and the versions
/// its index reports per native id.
#[derive(Deserialize)]
pub struct FixtureBackend {
    /// The backend id (`apt`).
    pub id: String,
    /// Build bosun's own compiled-in backend of this id over the fixture's
    /// canned HTTP and `bin_dir`, instead of a canned index and installer.
    #[serde(default)]
    pub real: bool,
    /// The backend's own version, for `bosun backends`.
    pub version: String,
    /// The purl type for identity (§5.6).
    pub purl_type: String,
    /// The vers type for comparison (§6).
    pub vers_type: String,
    /// Whether it resolves under `any` (§4.7).
    #[serde(default)]
    pub platform_independent: bool,
    /// Available versions per native id.
    #[serde(default)]
    pub available: BTreeMap<String, Vec<String>>,
    /// The checksum the index reports per native id (§12), for backends that
    /// enforce one (github-release). Absent for apt/cargo.
    #[serde(default)]
    pub checksums: BTreeMap<String, String>,
    /// The install scope (`system` or `user`, default `user`).
    #[serde(default)]
    pub scope: Option<String>,
    /// Whether the backend can hold (§5.2).
    #[serde(default)]
    pub supports_hold: bool,
    /// Whether the backend can downgrade (§5.2).
    #[serde(default)]
    pub supports_downgrade: bool,
    /// What `verify_installed` reports after apply, per native id (a version
    /// means Present; absence means Absent — e.g. an install that did not land).
    #[serde(default)]
    pub installed: BTreeMap<String, String>,
    /// What the package manager records as installed by the user (apt's
    /// manual marks, cargo's install list), by native id; each is also in
    /// `installed`.
    #[serde(default)]
    pub by_hand: Vec<String>,
    /// apt's `Priority` per native id; a package not named is `optional`.
    #[serde(default)]
    pub priorities: BTreeMap<String, String>,
}

/// An [`Index`] that answers from a fixture's canned version lists.
pub struct FixtureIndex {
    vers_type: String,
    available: BTreeMap<String, Vec<String>>,
    checksums: BTreeMap<String, String>,
}

impl FixtureIndex {
    /// Build from a backend's vers type, its available map, and its per-native-id
    /// checksum map (empty for backends that enforce none).
    pub fn new(
        vers_type: String,
        available: BTreeMap<String, Vec<String>>,
        checksums: BTreeMap<String, String>,
    ) -> FixtureIndex {
        FixtureIndex {
            vers_type,
            available,
            checksums,
        }
    }
}

impl Index for FixtureIndex {
    fn refresh(&self) -> Result<(), PortError> {
        Ok(())
    }

    fn query_available(&self, pkg: &Ref, _target: &Target) -> Result<Vec<Version>, PortError> {
        let scheme = scheme_for_type(&self.vers_type);
        self.available
            .get(&pkg.purl.native_id())
            .cloned()
            .unwrap_or_default()
            .iter()
            .map(|v| Version::parse(v, scheme).map_err(PortError::from))
            .collect()
    }

    fn checksum(
        &self,
        pkg: &Ref,
        _version: &Version,
        _target: &Target,
    ) -> Result<Option<Checksum>, PortError> {
        Ok(self
            .checksums
            .get(&pkg.purl.native_id())
            .map(|s| Checksum(s.clone())))
    }
}

/// An [`Installer`] that produces deterministic steps and reports a canned
/// `verify_installed` result, so apply can be driven over the seam.
pub struct FixtureInstaller {
    scope: Scope,
    hold: HoldSupport,
    supports_downgrade: bool,
    vers_type: String,
    machine: Machine,
    record: Option<HandRecord>,
}

/// A fixture backend's record of what the user installed, read by bosun's
/// own apt and cargo parsers through a [`HandRunner`].
#[derive(Clone)]
pub struct HandRecord {
    /// The backend id, which picks the parser (`apt`, `cargo`, `apk`, `npm`,
    /// `uv`); any other
    /// keeps no record.
    pub backend: String,
    /// What the user installed, by native id.
    pub names: Vec<String>,
    /// apt's priority per native id (default `optional`).
    pub priorities: BTreeMap<String, String>,
}

/// Answers the commands a backend reads its user installs with — `apt-mark
/// showmanual`, `dpkg-query -W -f=… <names>`, `cargo install --list`, the
/// apk world file and `apk info -e -v <names>`, `npm ls -g --json` and `uv
/// tool list` — from a [`HandRecord`] and what the fixture machine has
/// installed now.
struct HandRunner {
    installed: BTreeMap<String, String>,
    record: HandRecord,
}

impl HandRunner {
    /// The recorded names still installed, with their versions.
    fn present(&self) -> impl Iterator<Item = (&String, &String)> {
        self.record
            .names
            .iter()
            .filter_map(|n| self.installed.get(n).map(|v| (n, v)))
    }
}

impl CommandRunner for HandRunner {
    fn run(&self, command: &Command) -> Result<Output, PortError> {
        let args: Vec<&str> = command.args.iter().map(String::as_str).collect();
        let (code, stdout) = match (command.program.as_str(), args.as_slice()) {
            ("apt-mark", ["showmanual"]) => {
                (0, self.present().map(|(n, _)| format!("{n}\n")).collect())
            }
            ("dpkg-query", ["-W", _, names @ ..]) => {
                let lines: Vec<String> = names
                    .iter()
                    .filter_map(|n| {
                        let version = self.installed.get(*n)?;
                        let priority = self
                            .record
                            .priorities
                            .get(*n)
                            .map_or("optional", String::as_str);
                        Some(format!("{n}\t{version}\t{priority}\n"))
                    })
                    .collect();
                (0, lines.concat())
            }
            ("cat", ["/etc/apk/world"]) => {
                (0, self.present().map(|(n, _)| format!("{n}\n")).collect())
            }
            ("apk", ["info", "-e", "-v", names @ ..]) => {
                let lines: Vec<String> = names
                    .iter()
                    .filter_map(|n| Some(format!("{n}-{}\n", self.installed.get(*n)?)))
                    .collect();
                // apk exits with the number of names it did not find.
                ((names.len() - lines.len()) as i32, lines.concat())
            }
            ("npm", ["ls", "-g", "--depth=0", "--json", "--prefix", _]) => {
                let dependencies: serde_json::Map<String, serde_json::Value> = self
                    .present()
                    .map(|(n, v)| (n.clone(), serde_json::json!({ "version": v })))
                    .collect();
                let ls = serde_json::json!({ "name": "lib", "dependencies": dependencies });
                (0, ls.to_string())
            }
            ("uv", ["tool", "list", "--show-version-specifiers"]) => (
                0,
                self.present()
                    .map(|(n, v)| format!("{n} v{v} [required: =={v}]\n- {n}\n"))
                    .collect(),
            ),
            ("cargo", ["install", "--list"]) => (
                0,
                self.present()
                    .map(|(n, v)| format!("{n} v{v}:\n    {n}\n"))
                    .collect(),
            ),
            _ => (127, String::new()),
        };
        Ok(Output {
            code: Some(code),
            stdout: stdout.into_bytes(),
            stderr: Vec::new(),
        })
    }
}

impl FixtureInstaller {
    /// Build from a fixture backend's install-side fields, sharing the machine.
    pub fn new(
        scope: Scope,
        hold: HoldSupport,
        supports_downgrade: bool,
        vers_type: String,
        machine: Machine,
    ) -> FixtureInstaller {
        FixtureInstaller {
            scope,
            hold,
            supports_downgrade,
            vers_type,
            machine,
            record: None,
        }
    }

    /// The same installer, reporting `record` as what the user installed.
    pub fn with_record(self, record: HandRecord) -> FixtureInstaller {
        FixtureInstaller {
            record: Some(record),
            ..self
        }
    }

    fn step(&self, verb: &str, pkg: &Ref, version: Option<&Version>) -> Steps {
        let mut argv = vec![verb.to_string(), pkg.purl.native_id()];
        if let Some(v) = version {
            argv.push(v.as_str().to_string());
        }
        vec![Step::Run {
            argv,
            env: Vec::new(),
            scope: self.scope,
            ok_exits: Vec::new(),
        }]
    }
}

impl Installer for FixtureInstaller {
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            supports_exact_version: true,
            hold: self.hold,
            supports_downgrade: self.supports_downgrade,
        }
    }
    fn scope(&self) -> Scope {
        self.scope
    }
    fn ensure_source(&self, _source: &Source) -> Result<Steps, PortError> {
        Ok(Vec::new())
    }
    fn install_exact(&self, pkg: &Ref, version: &Version) -> Result<Steps, PortError> {
        Ok(self.step("install", pkg, Some(version)))
    }
    fn verify_installed(&self, pkg: &Ref) -> Result<Installed, PortError> {
        match self.machine.borrow().get(&pkg.purl.native_id()) {
            Some(raw) => {
                let version = Version::parse(raw, scheme_for_type(&self.vers_type))?;
                Ok(Installed::Present {
                    version: Some(version),
                })
            }
            None => Ok(Installed::Absent),
        }
    }
    fn hold(&self, pkg: &Ref, version: &Version) -> Result<Steps, PortError> {
        Ok(self.step("hold", pkg, Some(version)))
    }
    fn downgrade(&self, pkg: &Ref, version: &Version) -> Result<Steps, PortError> {
        if self.supports_downgrade {
            Ok(self.step("downgrade", pkg, Some(version)))
        } else {
            Err(PortError::Unsupported {
                capability: "downgrade".to_string(),
            })
        }
    }
    fn uninstall(&self, pkg: &Ref) -> Result<Steps, PortError> {
        Ok(self.step("remove", pkg, None))
    }
    fn user_installs(&self) -> Result<UserInstalls, PortError> {
        let Some(record) = &self.record else {
            return Ok(UserInstalls::Unsupported);
        };
        let runner = HandRunner {
            installed: self.machine.borrow().clone(),
            record: record.clone(),
        };
        match record.backend.as_str() {
            "apt" => bosun_backends::apt::user_installs(&runner),
            "cargo" => bosun_backends::cargo::user_installs(&runner),
            "apk" => bosun_backends::apk::user_installs(&runner),
            "npm" => bosun_backends::npm::user_installs(
                &runner,
                "npm",
                std::path::Path::new("/home/tester/.local"),
            ),
            "uv" => bosun_backends::uv::user_installs(&runner),
            _ => Ok(UserInstalls::Unsupported),
        }
    }
}

/// The OS packages a fixture's dpkg/rpm reports installed, by package name:
/// the real github-deb and github-rpm backends install into it through
/// [`RealSteps`] and read it back through [`PackageDbRunner`].
pub type PackageDb = Arc<Mutex<BTreeSet<String>>>;

/// The package managers the real github-deb / github-rpm backends drive,
/// answered from a [`PackageDb`]: `apt-get --version` and `dnf --version`
/// (present), `dpkg-query -W -f=${Status} <name>` and `rpm -q <name>` (by the
/// db). Anything else exits 127, as an absent program would.
pub struct PackageDbRunner {
    db: PackageDb,
}

impl PackageDbRunner {
    /// A runner over `db`.
    pub fn new(db: PackageDb) -> PackageDbRunner {
        PackageDbRunner { db }
    }
}

impl CommandRunner for PackageDbRunner {
    fn run(&self, command: &Command) -> Result<Output, PortError> {
        let args: Vec<&str> = command.args.iter().map(String::as_str).collect();
        let installed = |name: &str| self.db.lock().expect("package db").contains(name);
        let (code, stdout) = match (command.program.as_str(), args.as_slice()) {
            ("apt-get", ["--version"]) => (0, "apt 2.6.1 (amd64)".to_string()),
            ("dnf", ["--version"]) => (0, "4.18.0".to_string()),
            ("dpkg-query", ["-W", "-f=${Status}", name]) if installed(name) => {
                (0, "install ok installed".to_string())
            }
            ("rpm", ["-q", name]) if installed(name) => (0, format!("{name}-fixture")),
            ("dpkg-query", _) | ("rpm", _) => (1, String::new()),
            _ => (127, String::new()),
        };
        Ok(Output {
            code: Some(code),
            stdout: stdout.into_bytes(),
            stderr: Vec::new(),
        })
    }
}

/// Where `real` backends' steps run: `Fetch`/`Extract`/`Remove` through the
/// host executor over the canned HTTP, and the github-deb / github-rpm
/// package-manager commands applied to `packages`.
pub struct RealSteps {
    /// The canned HTTP the real backends fetch through.
    pub fetcher: Arc<dyn HttpFetcher>,
    /// The OS packages installed through apt-get / dnf.
    pub packages: PackageDb,
}

impl RealSteps {
    fn fetch_or_extract(&self, step: &Step) -> Result<(), PortError> {
        StepExecutor::new(&FakeCommandRunner::new(), self.fetcher.as_ref())
            .execute(std::slice::from_ref(step))
    }

    /// A github-deb / github-rpm package-manager command, applied to the
    /// package db: installing a fetched file (cached under a directory named
    /// for its package) adds the package, a removal drops it, a hold changes
    /// nothing. Any other argv is not ours (`false`).
    fn package(&self, argv: &[String]) -> Result<bool, PortError> {
        let args: Vec<&str> = argv.iter().map(String::as_str).collect();
        let mut db = self.packages.lock().expect("package db");
        match args.as_slice() {
            ["apt-get", "install", .., file] | ["dnf", "install" | "downgrade", .., file] => {
                let file = Path::new(file);
                if !file.exists() {
                    return Err(PortError::Backend {
                        detail: format!("{} was not fetched", file.display()),
                    });
                }
                let name = file
                    .parent()
                    .and_then(Path::file_name)
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                db.insert(name);
                Ok(true)
            }
            ["apt-get", "remove", "-y", name] | ["dnf", "remove", "-y", name] => {
                db.remove(*name);
                Ok(true)
            }
            ["apt-mark", ..] => Ok(true),
            _ => Ok(false),
        }
    }
}

/// A [`StepRunner`] that applies each step to the shared machine — an
/// `install`/`downgrade` records the version, a `remove` clears it — so
/// `verify_installed` afterwards reflects reality. It fails any `Run` whose
/// argv contains `fail_on` (without mutating), to drive a crash-mid-apply test.
/// With `real` set, it also carries out real backends' steps.
pub struct FixtureStepRunner {
    fail_on: Option<String>,
    machine: Machine,
    real: Option<RealSteps>,
}

impl FixtureStepRunner {
    /// Build with an optional failure marker, sharing the machine, and the
    /// real-backend step target when the fixture has real backends.
    pub fn new(
        fail_on: Option<String>,
        machine: Machine,
        real: Option<RealSteps>,
    ) -> FixtureStepRunner {
        FixtureStepRunner {
            fail_on,
            machine,
            real,
        }
    }
}

impl StepRunner for FixtureStepRunner {
    fn execute(&self, steps: &[Step]) -> Result<(), PortError> {
        for step in steps {
            let Step::Run { argv, .. } = step else {
                if let Some(real) = &self.real {
                    real.fetch_or_extract(step)?;
                }
                continue;
            };
            if let Some(marker) = &self.fail_on {
                if argv.iter().any(|a| a == marker) {
                    return Err(PortError::CommandFailed {
                        program: argv.first().cloned().unwrap_or_default(),
                        code: Some(1),
                        stderr: "fixture step failure".to_string(),
                    });
                }
            }
            if let Some(real) = &self.real {
                if real.package(argv)? {
                    continue;
                }
            }
            // argv is [verb, native_id, version?]; reflect it on the machine.
            match (argv.first().map(String::as_str), argv.get(1), argv.get(2)) {
                (Some("install") | Some("downgrade"), Some(native), Some(version)) => {
                    self.machine
                        .borrow_mut()
                        .insert(native.clone(), version.clone());
                }
                (Some("remove"), Some(native), _) => {
                    self.machine.borrow_mut().remove(native);
                }
                _ => {} // hold and anything else change no presence
            }
        }
        Ok(())
    }
}
