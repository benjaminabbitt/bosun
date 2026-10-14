//! A test-only seam (set `BOSUN_FIXTURE=<path>`): describe the host facts and a
//! set of backends with canned available versions, so the real `bosun` binary
//! can be driven in integration tests without touching apt/cargo/the network.
//! Production runs ignore it and probe the real host.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;

use bosun_core::vers::scheme_for_type;
use bosun_core::{
    Capabilities, Checksum, HoldSupport, Installed, Ref, Scope, Source, Target, Version,
};
use bosun_ports::backend::{Index, Installer};
use bosun_ports::step::{Step, StepRunner, Steps};
use bosun_ports::PortError;
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
}

/// One fixture backend, with the metadata the resolver needs and the versions
/// its index reports per native id.
#[derive(Deserialize)]
pub struct FixtureBackend {
    /// The backend id (`apt`).
    pub id: String,
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
}

/// A [`StepRunner`] that applies each step to the shared machine — an
/// `install`/`downgrade` records the version, a `remove` clears it — so
/// `verify_installed` afterwards reflects reality. It fails any `Run` whose
/// argv contains `fail_on` (without mutating), to drive a crash-mid-apply test.
pub struct FixtureStepRunner {
    fail_on: Option<String>,
    machine: Machine,
}

impl FixtureStepRunner {
    /// Build with an optional failure marker, sharing the machine.
    pub fn new(fail_on: Option<String>, machine: Machine) -> FixtureStepRunner {
        FixtureStepRunner { fail_on, machine }
    }
}

impl StepRunner for FixtureStepRunner {
    fn execute(&self, steps: &[Step]) -> Result<(), PortError> {
        for step in steps {
            let Step::Run { argv, .. } = step else {
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
