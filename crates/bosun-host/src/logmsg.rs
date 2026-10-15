//! Log message text for bosun-host, as constants (house rule): emitted with
//! `tracing`, as `debug!(message = logmsg::X, field = %v)`.

/// A backend probe found the backend present and registered it.
pub const BACKEND_REGISTERED: &str = "backend registered";
/// A backend probe found the backend absent; it was skipped.
pub const BACKEND_ABSENT: &str = "backend not present; skipped";
/// A backend probe returned an error; it was skipped.
pub const BACKEND_PROBE_ERROR: &str = "backend probe failed; skipped";
/// A backend probe panicked; it was skipped and the others continued.
pub const BACKEND_PROBE_PANIC: &str = "backend probe panicked; skipped";

/// A command exited with a nonzero code its step says is success, and what
/// that code says beyond it (msiexec 3010: reboot to finish).
pub const OK_EXIT: &str = "command succeeded with a nonzero exit";
