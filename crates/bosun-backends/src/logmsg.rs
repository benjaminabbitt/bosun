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

/// The enabled dnf repositories' priorities, as dnf reports them.
pub const DNF_PRIORITIES: &str = "dnf repository priorities";

/// The version dnf's repository priorities choose among those the spec
/// admits.
pub const DNF_CHOSEN: &str = "dnf's repository priorities choose this version within the spec";

/// A newer version the spec admits was passed over: a repository of better
/// priority offers an older one.
pub const DNF_NEWER_PASSED_OVER: &str =
    "a newer version the spec admits was passed over by dnf's repository priorities";

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

/// The version apt's pin priorities choose among those the spec admits.
pub const APT_CHOSEN: &str = "apt's pin priorities choose this version within the spec";

/// A newer version the spec admits was passed over: apt gives it a lower pin
/// priority (a backports suite, a pin) or it is behind the installed one.
pub const APT_NEWER_PASSED_OVER: &str =
    "a newer version the spec admits was passed over by apt's pin priorities";

/// The spec names exactly a version apt's pins keep from installing
/// (priority 0 or below); apt installs it when named, so bosun does.
pub const APT_PINNED_AWAY_NAMED: &str =
    "the spec names a version apt's pins keep from installing; installing it as apt-get install pkg=version would";

/// The version npm's own picker resolved the spec's range to.
pub const NPM_CHOSEN: &str = "npm resolves the spec's range to this version";

/// npm resolves no version in the spec's range (or knows no such package).
pub const NPM_NO_MATCH: &str = "npm resolves no version in the spec's range";

/// Where npm global installs go: npm's own global prefix (`npm prefix -g`).
pub const NPM_PREFIX: &str = "npm installs globally under this prefix";

/// npm's global prefix is not this user's to write (a system Node's is
/// root's); user-scope steps never run elevated, so `npm install -g` fails.
pub const NPM_PREFIX_NEEDS_ROOT: &str =
    "npm's global prefix is not writable by this user, so npm install -g there needs root; set a prefix of your own (npm config set prefix ~/.npm-global)";

/// npm's global bin dir is not on PATH, so what it installs is not found by name.
pub const NPM_BIN_NOT_ON_PATH: &str = "npm's global bin dir is not on PATH";

/// A global package Node ships itself (npm, corepack), not one the user
/// installed; it is left out of the export.
pub const NPM_BUNDLED_LEFT_OUT: &str = "npm package Node ships left out of the export";

/// The interpreter `uv tool install` would use, which uv resolves against.
pub const UV_PYTHON: &str = "uv resolves for the interpreter uv tool install uses";

/// uv finds no interpreter of its own; it resolves with whatever it picks.
pub const UV_NO_PYTHON: &str = "uv found no interpreter; uv pip compile picks its own";

/// The version uv resolved the spec to.
pub const UV_CHOSEN: &str = "uv resolves the spec to this version";

/// uv resolves no version in the spec.
pub const UV_NO_MATCH: &str = "uv resolves no version in the spec";

/// A uv tool installed from a path or URL, not PyPI, which bosun installs
/// from; it is left out of the export.
pub const NOT_FROM_PYPI: &str = "uv tool not from PyPI left out of the export";
