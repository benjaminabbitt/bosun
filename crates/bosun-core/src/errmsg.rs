//! Error message text, as constants.
//!
//! House rule: error prose lives here, never as a string literal in a
//! `#[error("…")]` attribute. thiserror variants read a constant with
//! `#[error("{}", errmsg::X)]` and interpolate only data after it, so the
//! wording is defined once and reviewable in one place.

/// A version string was empty.
pub const VERSION_EMPTY: &str = "empty version string";
/// A string the SemVer grammar rejected (the parser's own message follows).
pub const VERSION_INVALID_SEMVER: &str = "invalid semver version";

/// A range spec the grammar could not parse (the parser's message follows).
pub const SPEC_INVALID_RANGE: &str = "invalid version range";
/// A comparator clause with no recognised operator or an unparseable version.
pub const SPEC_INVALID_COMPARATOR: &str = "invalid version comparator";

/// Resolution was asked to choose from an empty set of available versions.
pub const RESOLVE_NO_AVAILABLE: &str = "no versions available to resolve against";
/// No available version satisfies the spec under its policy.
pub const RESOLVE_UNSATISFIABLE: &str = "no available version satisfies the spec";
