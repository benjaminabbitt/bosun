//! Error message text for the bosun CLI, as constants (house rule).

/// `render` for this machine found no lock for the manifest.
pub const RENDER_NO_LOCK: &str = "render needs this machine's lock for";

/// `render` for this machine found a lock older than the manifest.
pub const RENDER_STALE_LOCK: &str = "changed since it was locked";

/// The remedy for a missing or stale lock.
pub const RUN_LOCK: &str = "run `bosun lock`";
