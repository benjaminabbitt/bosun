//! Error message text for bosun-resolve, as constants (house rule).

/// An entry that applies to this host declares no version spec to resolve.
pub const MISSING_SPEC: &str = "entry has no spec to resolve";

/// A spec string could not be parsed as a vers set under the backend's type.
pub const BAD_SPEC: &str = "invalid spec";

/// The identity purl could not be built from the entry and host facts.
pub const BAD_PURL: &str = "could not build package identity";

/// A deb identity needs distro facts (os-release) this host did not provide.
pub const NO_DISTRO: &str = "deb backend needs distro facts this host lacks";

/// A github package id must be written `owner/repo`.
pub const BAD_GITHUB_ID: &str = "github package id must be owner/repo";

/// Querying a backend index for available versions failed.
pub const QUERY_FAILED: &str = "index query failed";
