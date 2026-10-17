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

/// A package apt marks manual is of another architecture (`libc6:i386`);
/// exporting it would name this machine's own, so it is left out.
pub const FOREIGN_ARCH_LEFT_OUT: &str =
    "manual apt package of another architecture left out of the export";

/// A package the apk world names with a repository tag (`jq@edge`): bosun
/// installs only from untagged repositories, so it is left out.
pub const TAGGED_LEFT_OUT: &str = "apk world entry from a tagged repository left out of the export";

/// A crate cargo installed from a path or git, not crates.io, which bosun
/// installs from; it is left out of the export.
pub const NOT_FROM_CRATES_IO: &str = "cargo install not from crates.io left out of the export";

/// A release publishes no checksum for an asset, so the lock records the
/// sha256 of the asset as downloaded now; later installs must match it.
pub const FIRST_USE_CHECKSUM: &str =
    "release publishes no checksum for this asset; locking the sha256 it has now";

/// dnf4 without its versionlock plugin cannot hold a package; dnf packages
/// install unheld (each plan says so) until the plugin is installed.
pub const DNF_NO_VERSIONLOCK: &str =
    "dnf has no versionlock command, so dnf packages cannot be held; install python3-dnf-plugin-versionlock";

/// apt's remote index trusts a release file's hashes without checking its
/// OpenPGP signature (§4.7): the package indexes are checked against it, but
/// it is only as trustworthy as the HTTPS connection it came over.
pub const APT_SIGNATURE_NOT_VERIFIED: &str =
    "the apt release file's signature is not verified; its package index hashes are trusted as fetched over HTTPS";

/// A release's mirror could not be reached, so the release file fetched
/// earlier is used.
pub const APT_STALE_RELEASE: &str =
    "could not fetch the apt release file; using the one fetched earlier";

/// A release file fetched within the cache's freshness window is reused.
pub const APT_RELEASE_CACHED: &str = "using the cached apt release file";

/// An optional suite (updates, security) is not on the mirror; skipped.
pub const APT_SUITE_ABSENT: &str = "apt suite not on the mirror; skipped";

/// A suite lists no package index for a component and arch; skipped.
pub const APT_NO_INDEX_LISTED: &str = "apt suite lists no package index for it; skipped";
