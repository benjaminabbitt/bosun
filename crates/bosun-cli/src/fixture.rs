//! A test-only seam (set `BOSUN_FIXTURE=<path>`): describe the host facts and a
//! set of backends with canned available versions, so the real `bosun` binary
//! can be driven in integration tests without touching apt/cargo/the network.
//! Production runs ignore it and probe the real host.

use std::collections::BTreeMap;

use bosun_core::vers::scheme_for_type;
use bosun_core::{Ref, Target, Version};
use bosun_ports::backend::Index;
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
