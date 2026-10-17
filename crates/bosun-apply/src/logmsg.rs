//! Log message text for bosun-apply, as constants (house rule), emitted with
//! `tracing`.

/// bosun holds a package whose spec is no longer exact, so the plan releases
/// the hold and lets the package manager update it between runs.
pub const RELEASING_HOLD: &str = "spec is not exact; releasing bosun's hold";
