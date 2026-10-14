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

/// A string that is not one of bosun's known operating systems.
pub const PLATFORM_UNKNOWN_OS: &str = "unknown operating system";
/// A string that is not one of bosun's known architectures.
pub const PLATFORM_UNKNOWN_ARCH: &str = "unknown architecture";
/// A target string not of the form `os/arch` (or the literal `any`).
pub const PLATFORM_INVALID_TARGET: &str = "invalid target (expected os/arch or any)";
/// A selector string not of the form `os` or `os:distro`.
pub const PLATFORM_INVALID_SELECTOR: &str = "invalid selector (expected os or os:distro)";
