//! Error and diagnostic message text for bosun-apply, as constants (house
//! rule). Privilege errors carry structured fields; the CLI renders the full
//! remedy from the machine's `Elevation` (§5.5), so these are the label text.

/// The plan changes system scope but the process is not elevated (§5.5).
pub const ELEVATION_REQUIRED: &str = "system-scope changes need elevation";

/// The plan changes user scope but the process is elevated (§5.5): user-scope
/// installs must not run as root.
pub const MUST_NOT_BE_ELEVATED: &str = "user-scope changes must not run elevated";

/// The plan changes both scopes and no `--scope` was given (§5.5): refuse
/// rather than apply half.
pub const MIXED_SCOPE: &str = "plan changes both system and user scope; re-run per scope";

/// A manifest entry is not applicable on this machine (§5.3): reported, not an
/// error.
pub const NOT_APPLICABLE: &str = "not applicable on this machine";

/// A manifest entry names no backend that is registered here (§5.3).
pub const MISSING_BACKEND: &str = "no registered backend";

/// No available version is in a spec's set (§7): a blocking plan error.
pub const UNSATISFIABLE: &str = "spec is unsatisfiable";

/// Claims on a package cannot be jointly satisfied (§4.6): a blocking error.
pub const EMPTY_INTERSECTION: &str = "claims cannot be jointly satisfied";

/// A package present outside bosun no longer satisfies its spec (§4.6): a
/// warning; bosun changes nothing and tells the user to upgrade it themselves.
pub const ACCEPTED_UNSATISFIED: &str = "installed outside bosun and no longer satisfies its spec";

/// A backend cannot durably hold the version it installed (§5.2): a warning;
/// the install still happens, just unpinned.
pub const UNSUPPORTED_HOLD: &str = "backend cannot hold; version not pinned";

/// A backend cannot move a package down to the resolved version (§5.2): an
/// error for that package; apply is blocked.
pub const UNSUPPORTED_DOWNGRADE: &str = "backend cannot downgrade to the resolved version";

/// Under `--frozen`, a package present but not bosun-owned (`accepted`) sits at
/// a different version than the lock pins (§4.6, §7): an error; bosun will not
/// change a package it does not own.
pub const FROZEN_ACCEPTED_CONFLICT: &str =
    "installed outside bosun at a different version than the lock; bosun will not change it";

/// A system-scope record left out of a run without elevation (§5.5).
pub const DEFERRED: &str = "recorded on the next elevated run";

/// Two manifest entries give `render` the same variable stem (§8.3).
pub const VAR_CLASH: &str = "two packages give the same render variable";

/// A logical name gives a stem that is not a variable name (§8.3), so the
/// entry must name one with `var`.
pub const BAD_DERIVED_VAR: &str = "a package name gives no valid render variable";
