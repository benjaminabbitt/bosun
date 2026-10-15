//! Log message text for the bosun CLI, as constants (house rule), emitted with
//! `tracing`.

/// A `bosun.lock` beside the manifest: the shared lock of older builds, which
/// this one no longer reads (the lock is now this machine's own).
pub const PROJECT_LOCK_IGNORED: &str =
    "bosun.lock beside the manifest is no longer read; this machine's lock is elsewhere";
