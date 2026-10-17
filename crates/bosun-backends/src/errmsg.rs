//! Error message text for bosun-backends, as constants (house rule).

/// A github-release `asset` template could not be rendered.
pub const ASSET_TEMPLATE: &str = "invalid asset template";

/// An asset template names a variable bosun does not provide.
pub const ASSET_UNKNOWN_VARIABLE: &str = "unknown variable";

/// An asset template uses a libc variable on a host whose libc is unknown.
pub const ASSET_LIBC_UNKNOWN: &str = "the host's C library is unknown, so there is no";

/// Why the libc is unknown, and what to write instead.
pub const ASSET_LIBC_UNKNOWN_WHY: &str =
    "bosun reads it from the loader /bin/sh uses, and found none; write the libc into the template (e.g. musl)";

/// A rendered asset name still holds a brace: a single-brace placeholder.
pub const ASSET_BRACE: &str =
    "the rendered name contains a brace; write placeholders as {{ binary }}";

/// The asset a template names is not a binary bosun can install (a package,
/// signature or checksum file).
pub const NOT_A_BINARY: &str = "not a binary bosun can install";

/// A github-release `tag` template is not a literal tag with one `{{ version }}`.
pub const TAG_TEMPLATE: &str = "invalid tag template";

/// An OS-package backend (github-deb, github-rpm) was given no `asset`
/// template; it has no default name to look for.
pub const ASSET_REQUIRED: &str =
    "needs an asset template (backends.<backend>.asset) naming the package file";

/// The asset an OS-package backend was pointed at is not its kind of package.
pub const NOT_A_DEB: &str = "not a .deb package";

/// The asset an OS-package backend was pointed at is not its kind of package.
pub const NOT_AN_RPM: &str = "not an .rpm package";

/// github-installer was not given a setting it needs.
pub const INSTALLER_NEEDS: &str = "github-installer needs";

/// The asset github-installer was pointed at is not an installer.
pub const NOT_AN_INSTALLER: &str = "not an .msi or .exe installer";

/// A release publishes no checksum for a file a system-scope backend would
/// install as root or Administrator, and the entry hasn't consented.
pub const UNVERIFIED_ELEVATED: &str =
    "the release publishes no checksum for it, and it would install as root or Administrator";
/// The `unverified` option takes only `allow`.
pub const UNVERIFIED_VALUE: &str = "unverified takes only `allow`, not";

/// A deb identity lacks the distro or release apt's remote index reads.
pub const APT_NO_DISTRO: &str =
    "apt's remote index needs the distro, release and arch in the identity";

/// apt's remote index knows no mirror for the distro.
pub const APT_NO_MIRROR: &str = "no apt mirror is known for distro";

/// The release's own suite is not on its mirror.
pub const APT_NO_RELEASE: &str = "no such apt release";

/// A release file names another suite than the one fetched.
pub const APT_RELEASE_MISMATCH: &str = "the apt release file is for another suite";

/// A release file bosun could not read.
pub const APT_BAD_RELEASE: &str = "malformed apt release file";

/// A fetched package index does not match the size and SHA256 its release
/// file lists.
pub const APT_HASH_MISMATCH: &str =
    "the apt package index does not match the hash its release file lists";

/// No suite of a release lists a package index for the arch.
pub const APT_NO_INDEX: &str = "the apt release lists no package index";

/// A package index would not decompress.
pub const APT_DECOMPRESS: &str = "could not decompress the apt package index";

/// The apt index cache could not be read or written.
pub const APT_CACHE: &str = "apt index cache";
