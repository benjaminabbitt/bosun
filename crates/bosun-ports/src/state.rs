//! The machine-state port (§4.6), transactional so two bosun runs never
//! interleave a read-modify-write and lose a claim.
//!
//! A mutating run calls [`StateStore::begin`]: it takes the exclusive lock,
//! loads the current state, and holds the lock across plan **and** apply. The
//! lock releases only on [`StateTransaction::commit`] (which writes the new
//! state atomically) or when the transaction is dropped. Read-only paths
//! (`--plan`, `status`) use [`StateStore::read`] under a shared lock. The JSON
//! format, file locations, and locking live in bosun-io/bosun-host.

use bosun_core::{Scope, StateDocument};

use crate::error::PortError;

/// Loads and stores the machine state, one file per install scope (§4.6).
pub trait StateStore {
    /// A held read-modify-write transaction over one scope's state.
    type Transaction: StateTransaction;

    /// Take the exclusive lock for `scope` and load its state, blocking until
    /// the lock is free. A machine with nothing recorded yet loads an empty
    /// document, not an error. The lock is held until commit or drop.
    fn begin(&self, scope: Scope) -> Result<Self::Transaction, PortError>;

    /// Like [`StateStore::begin`] but non-blocking: `Ok(None)` when another
    /// run already holds the lock.
    fn try_begin(&self, scope: Scope) -> Result<Option<Self::Transaction>, PortError>;

    /// Read the state for `scope` under a shared lock, for read-only paths. A
    /// machine with nothing recorded yet yields an empty document.
    fn read(&self, scope: Scope) -> Result<StateDocument, PortError>;
}

/// A read-modify-write transaction holding the exclusive lock for one scope.
pub trait StateTransaction {
    /// The state loaded when the transaction began.
    fn state(&self) -> &StateDocument;

    /// Atomically persist `state` **without** releasing the lock, so progress
    /// survives a crash or kill mid-apply (Appendix B): the applier calls this
    /// after each verified action, so a package already installed is already
    /// recorded even if the run never reaches [`commit`](Self::commit).
    fn checkpoint(&mut self, state: &StateDocument) -> Result<(), PortError>;

    /// Atomically persist `state` and release the lock. Written only after
    /// installs are verified (Appendix B #229/#130), so this records
    /// confirmed facts.
    fn commit(self, state: StateDocument) -> Result<(), PortError>;
}
