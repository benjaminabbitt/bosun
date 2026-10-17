//! Log message text for bosun-apply, as constants (house rule), emitted with
//! `tracing`.

/// bosun holds a package whose spec is no longer exact, so the plan releases
/// the hold and lets the package manager update it between runs.
pub const RELEASING_HOLD: &str = "spec is not exact; releasing bosun's hold";

/// The package manager moved a package bosun recorded (its own updates under a
/// range); bosun follows the version it reports.
pub const INSTALLED_MOVED: &str =
    "the package manager moved this package since bosun recorded it; following its version";

/// The package manager could not say what is installed; the recorded version
/// stands.
pub const INSTALLED_UNREAD: &str =
    "could not read the installed version; keeping the one bosun recorded";
