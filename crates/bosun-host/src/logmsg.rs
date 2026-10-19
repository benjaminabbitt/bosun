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

/// A file operation runs as another user (the one who elevated this run),
/// in a child that has dropped to their ids.
pub const AS_USER: &str = "file operation as the invoking user";

/// A command runs as another user (the one who elevated this run), in a
/// child that has dropped to their ids.
pub const RUN_AS_USER: &str = "command as the invoking user";

/// The environment commands run as the invoking user get: their `HOME`,
/// `USER` and `LOGNAME`, and a `PATH` of their own tools' directories then
/// this run's, each one they can reach; nothing else of root's.
pub const RUN_AS_USER_ENV: &str =
    "commands as the invoking user get their HOME, USER, LOGNAME and this PATH";

/// The machine's C library was read from the loader `/bin/sh` asks for.
pub const LIBC_DETECTED: &str = "libc detected";
/// No libc could be read: no `/bin/sh`, a static one, or an unfamiliar loader.
pub const LIBC_UNKNOWN: &str = "libc unknown; libc selectors will not match";
/// The libc family is known but its loader reported no version.
pub const LIBC_VERSION_UNKNOWN: &str = "libc version unknown; libc ranges will not match";
