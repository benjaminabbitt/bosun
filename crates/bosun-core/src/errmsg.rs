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

/// A vers range string that is not in canonical form, or is otherwise invalid.
pub const VERS_NONCANONICAL: &str = "non-canonical vers range";
/// A vers range missing its `vers:` scheme or `<type>/` prefix.
pub const VERS_NO_SCHEME: &str = "vers range must start with vers:<type>/";
/// A percent-encoded byte in a vers version that is not valid `%HH`.
pub const VERS_BAD_PERCENT: &str = "invalid percent-encoding in vers version";
/// No available version satisfies a spec's vers set.
pub const SELECT_UNSATISFIABLE: &str = "no available version satisfies the spec";

/// A spec's constraint version does not parse in the spec's scheme (§6).
pub const SPEC_BAD_VERSION: &str = "constraint version is not valid for its scheme";

/// A purl string missing the literal `pkg:` scheme.
pub const PURL_NO_SCHEME: &str = "purl must start with pkg:";
/// A purl type that is empty or has an invalid character.
pub const PURL_INVALID_TYPE: &str = "invalid purl type";
/// A purl with no name (names are required).
pub const PURL_EMPTY_NAME: &str = "purl name is required";
/// A purl qualifier key that is empty or has an invalid character.
pub const PURL_INVALID_QUALIFIER: &str = "invalid purl qualifier key";
/// A malformed `%HH` escape in a purl component.
pub const PURL_BAD_PERCENT: &str = "invalid percent-encoding in purl";

/// A string that is not one of bosun's known operating systems.
pub const PLATFORM_UNKNOWN_OS: &str = "unknown operating system";
/// A string that is not one of bosun's known architectures.
pub const PLATFORM_UNKNOWN_ARCH: &str = "unknown architecture";
/// A target string not of the form `os/arch` (or the literal `any`).
pub const PLATFORM_INVALID_TARGET: &str = "invalid target (expected os/arch or any)";
/// A selector string not of the form `any`, `os`, `os:distro` or `os:distro@release`.
pub const PLATFORM_INVALID_SELECTOR: &str =
    "invalid selector (expected any, os, os:distro or os:distro@release)";
