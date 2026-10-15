//! Error message text for bosun-host, as constants (house rule).

/// The running machine's architecture is not one bosun maps to a target.
pub const UNKNOWN_ARCH: &str = "unsupported architecture";

/// No file in an archive has the binary's name.
pub const MEMBER_NOT_FOUND: &str = "no file with that name in the archive";

/// More than one file in an archive has the binary's name.
pub const MEMBER_AMBIGUOUS: &str = "more than one file with that name in the archive";
