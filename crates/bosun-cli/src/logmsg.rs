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

/// `render` for another machine was given no `--libc`, so the manifest's libc
/// selectors don't match there.
pub const RENDER_NO_LIBC: &str =
    "the target's C library was not given (--libc), so libc selectors don't match it";

/// `render` for another machine was given no `--distro`, so the manifest's
/// distro selectors don't match there.
pub const RENDER_NO_DISTRO: &str =
    "the target's distro was not given (--distro), so distro selectors don't match it";

/// `render` reads the target from an image's files, copied out with a
/// container engine.
pub const RENDER_FROM_IMAGE: &str =
    "reading the target's os-release and apt sources from the image";

/// `render` reads the target from a root filesystem directory.
pub const RENDER_FROM_ROOTFS: &str =
    "reading the target's os-release and apt sources from the root filesystem";

/// A path render copies out of an image is not in it.
pub const RENDER_IMAGE_LACKS: &str = "the image has no such path; left out";
