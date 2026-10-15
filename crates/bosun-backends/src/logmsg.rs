//! Log message text for bosun-backends, as constants (house rule), emitted
//! with `tracing`.

/// The github-release install bin dir is not on `$PATH`; installed binaries
/// won't be found until the user adds it.
pub const BIN_DIR_NOT_ON_PATH: &str = "github-release bin dir is not on PATH";

/// A github-release asset has no published checksum (no sums file, or none
/// that lists it): the download is held to the sha256 this machine's lock
/// recorded on first use, not to anything the project published.
pub const NO_CHECKSUM: &str =
    "release publishes no checksum for this asset; checking it against the sha256 locked on first use";

/// A release publishes no checksum for an asset, so the lock records the
/// sha256 of the asset as downloaded now; later installs must match it.
pub const FIRST_USE_CHECKSUM: &str =
    "release publishes no checksum for this asset; locking the sha256 it has now";
