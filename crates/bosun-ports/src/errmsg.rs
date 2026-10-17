//! Error message text for the ports layer, as constants (house rule). Prose
//! lives here; thiserror variants read a constant and interpolate only data.

/// A command could not be launched (binary not found on PATH).
pub const COMMAND_NOT_FOUND: &str = "command not found";
/// A command ran but exited non-zero.
pub const COMMAND_FAILED: &str = "command failed";
/// How a command ended when a signal stopped it.
pub const KILLED_BY_SIGNAL: &str = "killed by a signal";
/// A command's output could not be understood.
pub const COMMAND_OUTPUT: &str = "could not parse command output";
/// An HTTP request failed to complete.
pub const HTTP_REQUEST: &str = "http request failed";
/// An HTTP request returned an error status.
pub const HTTP_STATUS: &str = "http request returned an error status";
/// Reading or writing machine state failed.
pub const STATE_IO: &str = "machine state i/o failed";
/// This user may not read a scope's machine state (§4.6).
pub const STATE_DENIED: &str = "machine state is not readable by this user";
/// A backend operation failed.
pub const BACKEND: &str = "backend operation failed";
/// A backend was asked to do something it does not support (§5.2).
pub const UNSUPPORTED: &str = "operation not supported by this backend";
