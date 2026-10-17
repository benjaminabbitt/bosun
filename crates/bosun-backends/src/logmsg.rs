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

/// The sparse index cargo's configuration has crates.io crates come from.
pub const CARGO_INDEX: &str = "cargo installs crates.io crates from this index";

/// cargo's configuration names an index bosun cannot read.
pub const CARGO_INDEX_UNREADABLE: &str =
    "cargo's configured crates.io index is not one bosun can read";

/// The apk-tools major the apk backend probed, whose version rules every
/// Alpine version on this machine follows.
pub const APK_ORDERING: &str = "apk versions order as this apk-tools major orders them";

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

/// The registry npm is configured to use for a package's scope.
pub const NPM_REGISTRY: &str = "npm's configured registry";

/// npm could not say which registry it uses; the public one is listed from.
pub const NPM_REGISTRY_UNREAD: &str =
    "could not read npm's configured registry; listing from the public registry";

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

/// uv resolves no version in the spec; its explanation follows.
pub const UV_NO_MATCH: &str = "uv resolves no version in the spec";

/// A uv tool installed from a path or URL, not PyPI, which bosun installs
/// from; it is left out of the export.
pub const NOT_FROM_PYPI: &str = "uv tool not from PyPI left out of the export";

/// A source list line or stanza bosun could not read; apt would refuse it.
pub const APT_SOURCE_SKIPPED: &str = "unreadable apt source entry; skipped";

/// An `http://` mirror from the target's sources is read over HTTPS.
pub const APT_HTTPS: &str = "reading the apt mirror over HTTPS";

/// No apt on this machine: bosun reads the target's mirrors itself.
pub const APT_NO_LOCAL_APT: &str = "no apt-get here; reading the apt mirrors directly";

/// This machine's apt could not read a stock image's sources (a release
/// without an updates or security suite): bosun reads them itself.
pub const APT_SCRATCH_STOCK: &str =
    "apt could not update the stock sources; reading the apt mirrors directly";

/// A source entry has no key in the target's root to check its release file
/// against: the scratch configuration trusts it, read over HTTPS, and apt
/// still checks the package indexes against the release file's hashes.
pub const APT_SCRATCH_UNCHECKED: &str =
    "no key for this apt source in the target; trusting it over HTTPS in the scratch root";

/// The scratch root's `apt-get update` ran within the freshness window.
pub const APT_SCRATCH_FRESH: &str = "apt lists in the scratch root are fresh";

/// The scratch root's `apt-get update` failed; the lists it fetched before
/// are used.
pub const APT_SCRATCH_STALE: &str =
    "apt-get update for the target's sources failed; using the lists fetched before";

/// apt's remote index resolves through this machine's apt, in a scratch root.
pub const APT_THROUGH_LOCAL_APT: &str =
    "resolving the target's apt packages with this machine's apt, in a scratch root";

/// No target root names the release's sources (no `--image` / `--rootfs`,
/// or another release than the root's): a stock image's are read.
pub const APT_STOCK_SOURCES: &str =
    "no target image gives this release's apt sources; reading the ones a stock image lists";

/// The target pins packages, which only apt applies; without apt here,
/// bosun's reading of the mirrors does not.
pub const APT_PINS_IGNORED: &str =
    "the target pins apt packages (preferences), which are applied only when apt-get is on this machine";

/// dnf keeps the installed build: `dnf install` never goes back past it, and
/// no repository offers a newer one it would take.
pub const DNF_INSTALLED_KEPT: &str = "dnf keeps the installed build within the spec";

/// A scratch root neither updated nor used for 30 days was removed.
pub const APT_SCRATCH_PRUNED: &str = "removed an apt scratch root unused for 30 days";

/// A scratch root due for removal could not be removed; it is left.
pub const APT_SCRATCH_PRUNE_FAILED: &str =
    "could not remove an apt scratch root unused for 30 days";
