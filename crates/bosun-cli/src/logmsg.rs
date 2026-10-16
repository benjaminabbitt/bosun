//! Log message text for the bosun CLI, as constants (house rule), emitted with
//! `tracing`.

/// A present backend keeps no record of what the user installed through it,
/// so `export` takes nothing from it.
pub const EXPORT_NO_RECORD: &str =
    "backend keeps no record of what the user installed; nothing exported from it";

/// A `bosun.lock` beside the manifest: the shared lock of older builds, which
/// this one no longer reads (the lock is now this machine's own).
pub const PROJECT_LOCK_IGNORED: &str =
    "bosun.lock beside the manifest is no longer read; this machine's lock is elsewhere";
