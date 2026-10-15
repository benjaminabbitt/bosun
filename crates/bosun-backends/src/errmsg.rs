//! Error message text for bosun-backends, as constants (house rule).

/// A github-release `asset` template could not be rendered.
pub const ASSET_TEMPLATE: &str = "invalid asset template";

/// An asset template names a variable bosun does not provide.
pub const ASSET_UNKNOWN_VARIABLE: &str = "unknown variable";

/// An asset template names a variable that is documented but not available yet.
pub const ASSET_NOT_YET: &str = "variable not supported yet";

/// Why the not-yet variables are unavailable, and what to write instead.
pub const ASSET_NOT_YET_WHY: &str =
    "it needs host libc detection, which bosun does not have; write the libc into the template (e.g. musl)";

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
