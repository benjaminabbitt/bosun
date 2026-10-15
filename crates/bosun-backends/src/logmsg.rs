//! Log message text for bosun-backends, as constants (house rule), emitted
//! with `tracing`.

/// The github-release install bin dir is not on `$PATH`; installed binaries
/// won't be found until the user adds it.
pub const BIN_DIR_NOT_ON_PATH: &str = "github-release bin dir is not on PATH";

/// A github-release asset has no published checksum (no sums file, or none
/// that lists it), so it is installed unverified.
pub const NO_CHECKSUM: &str = "release publishes no checksum for this asset; installing unverified";
