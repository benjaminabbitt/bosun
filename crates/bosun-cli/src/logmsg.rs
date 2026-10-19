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

/// A run without elevation may not read the system state an elevated run
/// left; it leaves that state, and the packages it records, out.
pub const SYSTEM_STATE_HIDDEN: &str =
    "system state is not readable by this user; its packages are left out (run elevated to include them)";

/// The package manager moved a package bosun recorded (its own updates under a
/// range); bosun follows the version it reports.
pub const INSTALLED_MOVED: &str =
    "the package manager moved this package since bosun recorded it; following its version";

/// The package manager moved a package bosun recorded to a version outside the
/// spec; the plan moves it back as for any other.
pub const INSTALLED_OUTSIDE_SPEC: &str =
    "the package manager moved this package outside the spec; planning to move it back";

/// prune could not check whether a claim's manifest exists (a directory this
/// user may not search); the claim stands.
pub const MANIFEST_UNCHECKED: &str =
    "could not check that the claim's manifest exists; keeping the claim";

/// A release selector names the distro's own codename where its packages
/// spell the release by the base distro's (Linux Mint 22: `wilma`, whose apt
/// reads Ubuntu `noble`), so it matches nothing.
pub const OWN_CODENAME_SELECTOR: &str =
    "a release selector names the distro's own codename, which is not its release; it matches nothing, name the release instead";

/// `install --scope` left a package's scope out of its apply, so its lock
/// entry stays as it was.
pub const LOCK_KEPT: &str = "--scope left the package's scope out; its lock entry is kept";

/// An elevated run reads and writes the user state and lock of the user who
/// elevated it (sudo's `SUDO_USER`, doas's `DOAS_USER`), in the default state
/// dir of their passwd home, as that user. Their `XDG_STATE_HOME` is not
/// consulted: sudo does not pass it, and one it passes cannot be told from
/// root's.
pub const INVOKER_STATE: &str =
    "elevated: using the user state and lock of the user who elevated, under their home's .local/state, as them";

/// An elevated run found `XDG_STATE_HOME` set: it cannot be told from root's,
/// so the invoking user's state and lock are under their home's default
/// state dir instead.
pub const XDG_STATE_HOME_IGNORED: &str = "XDG_STATE_HOME is not read under elevation, as it cannot be told from root's; using the invoking user's <home>/.local/state";
