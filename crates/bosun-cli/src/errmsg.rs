//! Error message text for the bosun CLI, as constants (house rule).

/// `render` for this machine found no lock for the manifest.
pub const RENDER_NO_LOCK: &str = "render needs this machine's lock for";

/// `render` for this machine found a lock older than the manifest.
pub const RENDER_STALE_LOCK: &str = "changed since it was locked";

/// The remedy for a missing or stale lock.
pub const RUN_LOCK: &str = "run `bosun lock`";

/// Resolving for another machine needs a remote index none of an entry's
/// backends has (§4.7).
pub const RENDER_NO_REMOTE_INDEX: &str =
    "bosun has no remote index to resolve for another machine through";

/// A deb or rpm entry for another machine needs its distro.
pub const RENDER_GIVE_DISTRO: &str = "give the target's distro with --distro <id>@<release>";

/// `--target` / `--targets` names no platform.
pub const RENDER_BAD_TARGET: &str = "a render target is a platform such as linux/arm64";

/// `--distro` is not `<id>[@<release>]`.
pub const RENDER_BAD_DISTRO: &str =
    "--distro takes <id>[@<release>], as in a selector (debian@bookworm)";

/// `--libc` is not `<family>[@<version>]`.
pub const RENDER_BAD_LIBC: &str = "--libc takes gnu or musl, optionally @<version> (gnu@2.36)";
