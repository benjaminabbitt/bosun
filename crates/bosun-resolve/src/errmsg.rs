//! Error message text for bosun-resolve, as constants (house rule).

/// An entry that applies to this host declares no version spec to resolve.
pub const MISSING_SPEC: &str = "entry has no spec to resolve";

/// A spec string could not be parsed as a vers set under the backend's type.
pub const BAD_SPEC: &str = "invalid spec";

/// The identity purl could not be built from the entry and host facts.
pub const BAD_PURL: &str = "could not build package identity";

/// A deb or rpm identity needs distro facts (os-release) this host did not
/// provide.
pub const NO_DISTRO: &str = "deb/rpm backend needs distro facts this host lacks";

/// A github package id must be written `owner/repo`.
pub const BAD_GITHUB_ID: &str = "github package id must be owner/repo";

/// Querying a backend index for available versions failed.
pub const QUERY_FAILED: &str = "index query failed";

/// A claim's spec string could not be parsed as a vers set.
pub const BAD_CLAIM_SPEC: &str = "invalid claim spec";

/// No available version satisfies every claim on a package (§4.6): the specs
/// disagree and bosun refuses rather than flip-flop the installed version.
pub const EMPTY_INTERSECTION: &str = "claims cannot be jointly satisfied";

/// A lockfile's pinned version is excluded by another claim's spec, so
/// `--frozen` cannot honour the lock without breaking that claim (§7).
pub const FROZEN_CLASH: &str = "locked version clashes with another claim";
