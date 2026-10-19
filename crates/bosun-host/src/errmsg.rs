//! Error message text for bosun-host, as constants (house rule).

/// The running machine's architecture is not one bosun maps to a target.
pub const UNKNOWN_ARCH: &str = "unsupported architecture";

/// The machine's architecture could not be read.
pub const ARCH_UNREAD: &str = "could not read the machine's architecture";

/// No file in an archive has the binary's name.
pub const MEMBER_NOT_FOUND: &str = "no file with that name in the archive";

/// The child doing a file operation as another user ended without
/// answering.
pub const AS_USER_NO_ANSWER: &str = "the file operation as the invoking user gave no answer";

/// The child doing a file operation as another user answered, then failed.
pub const AS_USER_FAILED: &str = "the file operation as the invoking user failed part way";

/// More than one file in an archive has the binary's name.
pub const MEMBER_AMBIGUOUS: &str = "more than one file with that name in the archive";
