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
