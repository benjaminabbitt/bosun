//! A test-only seam (set `BOSUN_FIXTURE=<path>`): describe the host facts and a
//! set of backends with canned available versions, so the real `bosun` binary
//! can be driven in integration tests without touching apt/cargo/the network.
//! Production runs ignore it and probe the real host.

use std::collections::BTreeMap;

use bosun_core::vers::scheme_for_type;
use bosun_core::{Capabilities, Installed, Ref, Scope, Source, Target, Version};
use bosun_ports::backend::{Index, Installer};
use bosun_ports::step::{Step, StepRunner, Steps};
use bosun_ports::PortError;
use serde::Deserialize;

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
}

impl FixtureIndex {
    /// Build from a backend's vers type and its available map.
    pub fn new(vers_type: String, available: BTreeMap<String, Vec<String>>) -> FixtureIndex {
        FixtureIndex {
            vers_type,
            available,
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
}

/// An [`Installer`] that produces deterministic steps and reports a canned
/// `verify_installed` result, so apply can be driven over the seam.
pub struct FixtureInstaller {
    scope: Scope,
    supports_hold: bool,
    supports_downgrade: bool,
    vers_type: String,
    installed: BTreeMap<String, String>,
}

impl FixtureInstaller {
    /// Build from a fixture backend's install-side fields.
    pub fn new(
        scope: Scope,
        supports_hold: bool,
        supports_downgrade: bool,
        vers_type: String,
        installed: BTreeMap<String, String>,
    ) -> FixtureInstaller {
        FixtureInstaller {
            scope,
            supports_hold,
            supports_downgrade,
            vers_type,
            installed,
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
            supports_hold: self.supports_hold,
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
        match self.installed.get(&pkg.purl.native_id()) {
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

/// A [`StepRunner`] that no-ops every step, except it fails any `Run` whose argv
/// contains `fail_on` (to drive a crash-mid-apply test).
pub struct FixtureStepRunner {
    fail_on: Option<String>,
}

impl FixtureStepRunner {
    /// Build with an optional failure marker.
    pub fn new(fail_on: Option<String>) -> FixtureStepRunner {
        FixtureStepRunner { fail_on }
    }
}

impl StepRunner for FixtureStepRunner {
    fn execute(&self, steps: &[Step]) -> Result<(), PortError> {
        for step in steps {
            if let Step::Run { argv, .. } = step {
                if let Some(marker) = &self.fail_on {
                    if argv.iter().any(|a| a == marker) {
                        return Err(PortError::CommandFailed {
                            program: argv.first().cloned().unwrap_or_default(),
                            code: Some(1),
                            stderr: "fixture step failure".to_string(),
                        });
                    }
                }
            }
        }
        Ok(())
    }
}
