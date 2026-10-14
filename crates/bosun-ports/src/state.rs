//! The machine-state port (§4.6). Reading and writing the per-scope state
//! file is delegated so the core logic is driven by an in-memory fake in
//! tests. The JSON format and file locations live in bosun-io/bosun-host.

use bosun_core::{Scope, StateDocument};

use crate::error::PortError;

/// Loads and stores the machine state, one file per install scope (§4.6).
pub trait StateStore {
    /// Load the state for `scope`. A machine with nothing recorded yet yields
    /// an empty document, not an error.
    fn load(&self, scope: Scope) -> Result<StateDocument, PortError>;
    /// Persist `state` for `scope`. Written only after installs are verified
    /// (Appendix B #229/#130), so this records confirmed facts.
    fn save(&self, scope: Scope, state: &StateDocument) -> Result<(), PortError>;
}
