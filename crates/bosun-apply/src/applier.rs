//! Applying a plan to the host (§4.8, §8). A [`Driver`] carries out a [`Plan`];
//! [`HostApplier`] is the M1 driver (the container renderer is a peer, out of
//! M1). It runs each action's steps through a [`StepRunner`] and records the
//! result in machine state inside **one** [`StateStore`] transaction, so a run
//! never interleaves a read-modify-write (§4.6).
//!
//! Three disciplines from Appendix B hold here:
//!
//! - **Per-backend isolation.** If a backend's action fails, that backend's
//!   later actions are skipped and reported; other backends carry on.
//! - **Verify before recording.** A package's state is written only after
//!   `verify_installed` confirms it (Appendix B #229/#130). github-release
//!   cannot read a version, so the resolved version is recorded instead (§4.6).
//! - **One transaction.** The whole apply runs inside a single state
//!   transaction, committed once at the end with the confirmed facts.
//!
//! Confirmation and `--yes` are the CLI's concern, not the driver's.

use std::collections::{BTreeMap, BTreeSet};

use bosun_core::record::{Package, Placement, StateDocument, StateLeaf};
use bosun_core::{BackendId, Claim, Installed, Origin, Ref, Scope, Target, Version};
use bosun_ports::{Installer, PortError, StateStore, StateTransaction, StepRunner};

use crate::{Action, ActionKind, Plan};

/// Carries out a [`Plan`]. The host applier runs it on this machine; a
/// container renderer (§8, out of M1) would be a peer implementation.
pub trait Driver {
    /// Apply `plan`, returning a per-action report. Individual action failures
    /// are isolated and reported (not returned as `Err`); `Err` is for a
    /// failure that stops the run, such as the state transaction itself.
    fn apply(&self, plan: &Plan) -> Result<ApplyReport, PortError>;
}

/// What became of each action (§4.8).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApplyReport {
    /// One entry per applied action, in plan order.
    pub outcomes: Vec<ActionOutcome>,
}

/// The result of one action.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActionOutcome {
    /// The package's logical name.
    pub name: String,
    /// The action attempted.
    pub kind: ActionKind,
    /// How it went.
    pub result: ApplyResult,
}

/// Whether an action succeeded, failed, or was skipped.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ApplyResult {
    /// The action ran and (for installs) verified.
    Applied,
    /// The action failed; `error` is the diagnostic.
    Failed {
        /// The failure detail.
        error: String,
    },
    /// Skipped because an earlier action on the same backend failed.
    Skipped,
}

/// The M1 driver: apply a plan on the local host, recording state in one scope
/// (§4.6). Built by the CLI with the host's step runner, the registered
/// installers, the state store, the machine's current target, and — for system
/// scope — the invoking user to stamp on claims.
pub struct HostApplier<'a, S: StateStore> {
    scope: Scope,
    target: Target,
    invoking_user: Option<String>,
    executor: &'a dyn StepRunner,
    installers: &'a BTreeMap<BackendId, &'a dyn Installer>,
    state: &'a S,
}

impl<'a, S: StateStore> HostApplier<'a, S> {
    /// Build a host applier for one install scope.
    pub fn new(
        scope: Scope,
        target: Target,
        invoking_user: Option<String>,
        executor: &'a dyn StepRunner,
        installers: &'a BTreeMap<BackendId, &'a dyn Installer>,
        state: &'a S,
    ) -> HostApplier<'a, S> {
        HostApplier {
            scope,
            target,
            invoking_user,
            executor,
            installers,
            state,
        }
    }

    fn installer(&self, backend: &BackendId) -> Result<&dyn Installer, PortError> {
        self.installers
            .get(backend)
            .copied()
            .ok_or_else(|| PortError::Backend {
                detail: format!("no installer registered for {}", backend.as_str()),
            })
    }

    /// Carry out one action against the working state `doc`.
    fn apply_one(&self, action: &Action, doc: &mut StateDocument) -> Result<(), PortError> {
        match action.kind {
            ActionKind::Install | ActionKind::Upgrade | ActionKind::Downgrade => {
                self.executor.execute(&action.steps)?;
                let installer = self.installer(&action.backend)?;
                // Verify before recording (Appendix B): never write a version
                // the manager did not confirm.
                let version = match installer.verify_installed(&Ref::new(action.purl.clone()))? {
                    Installed::Present { version: Some(v) } => v,
                    // github-release cannot read a version; record the resolved.
                    Installed::Present { version: None } => {
                        action.to.clone().ok_or_else(|| PortError::Backend {
                            detail: format!(
                                "{}: verified present but no version to record",
                                action.name
                            ),
                        })?
                    }
                    Installed::Absent => {
                        return Err(PortError::Backend {
                            detail: format!(
                                "{}: did not verify as installed after its steps ran",
                                action.name
                            ),
                        })
                    }
                };
                self.record(doc, action, version, Origin::Installed);
                Ok(())
            }
            ActionKind::Hold => {
                // The held package was recorded by its install this run; a hold
                // pins the version, it does not change what is recorded.
                self.executor.execute(&action.steps)?;
                Ok(())
            }
            ActionKind::Accept => {
                let version = action.from.clone().ok_or_else(|| PortError::Backend {
                    detail: format!("{}: accept without an installed version", action.name),
                })?;
                self.record(doc, action, version, Origin::Accepted);
                Ok(())
            }
            ActionKind::Remove => {
                self.executor.execute(&action.steps)?;
                remove(doc, &action.name, self.target);
                Ok(())
            }
            ActionKind::DropClaim => {
                drop_claim(doc, &action.name, self.target, &action.claims);
                Ok(())
            }
        }
    }

    /// Upsert the package's leaf at this machine's target with a confirmed
    /// version, origin, and the claims that drove it (system-scope claims
    /// stamped with the invoking user, §4.6).
    fn record(&self, doc: &mut StateDocument, action: &Action, version: Version, origin: Origin) {
        let leaf = StateLeaf {
            placement: Placement {
                backend: action.backend.clone(),
                purl: action.purl.clone(),
            },
            installed: version,
            origin,
            claimed_by: self.stamp(&action.claims),
        };
        match doc.packages.iter_mut().find(|p| p.name == action.name) {
            Some(p) => {
                p.targets.insert(self.target, leaf);
            }
            None => doc.packages.push(Package {
                name: action.name.clone(),
                meta: (),
                targets: BTreeMap::from([(self.target, leaf)]),
            }),
        }
    }

    /// Stamp each claim with the invoking user when recording in system scope
    /// and the claim has no user yet (§4.6); user-scope state stays unstamped.
    fn stamp(&self, claims: &[Claim]) -> Vec<Claim> {
        claims
            .iter()
            .map(|c| {
                let mut c = c.clone();
                if self.scope == Scope::System && c.user.is_none() {
                    c.user = self.invoking_user.clone();
                }
                c
            })
            .collect()
    }
}

/// Remove the package's leaf at `target`, dropping the package when it has no
/// leaves left.
fn remove(doc: &mut StateDocument, name: &str, target: Target) {
    if let Some(p) = doc.packages.iter_mut().find(|p| p.name == name) {
        p.targets.remove(&target);
    }
    doc.packages.retain(|p| !p.targets.is_empty());
}

/// Drop `dropped` from the package's claims at `target`, leaving it installed.
fn drop_claim(doc: &mut StateDocument, name: &str, target: Target, dropped: &[Claim]) {
    if let Some(p) = doc.packages.iter_mut().find(|p| p.name == name) {
        if let Some(leaf) = p.targets.get_mut(&target) {
            leaf.claimed_by.retain(|c| !dropped.contains(c));
        }
    }
}

impl<S: StateStore> Driver for HostApplier<'_, S> {
    fn apply(&self, plan: &Plan) -> Result<ApplyReport, PortError> {
        let mut txn = self.state.begin(self.scope)?;
        let mut doc = txn.state().clone();
        let mut outcomes = Vec::new();
        let mut failed: BTreeSet<BackendId> = BTreeSet::new();

        for action in &plan.actions {
            if action.scope != self.scope {
                continue; // another scope's run applies these
            }
            let result = if failed.contains(&action.backend) {
                ApplyResult::Skipped
            } else {
                match self.apply_one(action, &mut doc) {
                    // Persist after every confirmed action, lock still held, so
                    // a crash or kill mid-run keeps what already landed and the
                    // state never diverges from reality (Appendix B).
                    Ok(()) => {
                        txn.checkpoint(&doc)?;
                        ApplyResult::Applied
                    }
                    Err(e) => {
                        // Isolate the backend: skip its remaining actions (Appendix B).
                        failed.insert(action.backend.clone());
                        ApplyResult::Failed {
                            error: e.to_string(),
                        }
                    }
                }
            };
            outcomes.push(ActionOutcome {
                name: action.name.clone(),
                kind: action.kind,
                result,
            });
        }

        // Final write releases the lock; checkpoints already hold the facts.
        txn.commit(doc)?;
        Ok(ApplyReport { outcomes })
    }
}
