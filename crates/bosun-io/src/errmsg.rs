//! Error message text for bosun-io, as constants (house rule).

/// A file could not be read.
pub const READ: &str = "could not read file";
/// A file could not be written.
pub const WRITE: &str = "could not write file";
/// A file's contents could not be parsed.
pub const PARSE: &str = "could not parse file";
/// A machine-state lock could not be acquired.
pub const LOCK: &str = "could not lock machine state";
/// A lockfile declares a format version this build cannot read.
pub const LOCKFILE_VERSION: &str = "unsupported lockfile version";
/// A recorded value could not be converted to bosun's domain model.
pub const CONVERT: &str = "invalid recorded value";
