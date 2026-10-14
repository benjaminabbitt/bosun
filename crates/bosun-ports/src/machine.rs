//! The machine interface (§5.5): all OS-specific facts behind one trait, so
//! the core never branches on OS. `LinuxMachine`, `FakeMachine`, etc. live in
//! bosun-host; the privilege check in bosun-apply consumes this.
//!
//! `Machine` is orthogonal to backends: it knows nothing about package
//! managers. The only coupling is privilege — when an action's scope is
//! `system`, the `Machine` decides whether the current process may perform it
//! and how to word the remedy.

use std::path::PathBuf;

use bosun_core::{Arch, Distro, Os, Scope};

/// Whether the current process is running with elevated privilege (§5.5).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Privilege {
    /// Effective UID 0 (Unix) or an elevated token (Windows).
    Elevated,
    /// Ordinary privilege.
    Unelevated,
}

/// The human invoking bosun, resolved even under `sudo` (§5.5): claims in
/// system state are recorded for this user, not root.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct User {
    /// The user name (`SUDO_USER` under sudo, else the current user).
    pub name: String,
    /// The numeric UID where the OS has one (`SUDO_UID` under sudo).
    pub uid: Option<u32>,
}

/// How the current machine is elevated, which drives the remedy wording in an
/// elevation error (§5.5). The remedy text itself is built by the CLI.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Elevation {
    /// Re-run under `sudo` (Linux, macOS).
    Sudo,
    /// Open a terminal "as administrator" and re-run (Windows).
    AdministratorTerminal,
}

/// The polymorphic host interface. One implementation per OS, plus a fake for
/// tests.
pub trait Machine {
    /// The operating system.
    fn os(&self) -> Os;
    /// The CPU architecture.
    fn arch(&self) -> Arch;
    /// Linux distro facts from os-release, when available (§5.4).
    fn distro(&self) -> Option<&Distro>;
    /// Whether the process is elevated.
    fn privilege(&self) -> Privilege;
    /// The invoking human (§5.5).
    fn invoking_user(&self) -> User;
    /// Where the state file for `scope` lives on this machine (§4.6).
    fn state_path(&self, scope: Scope) -> PathBuf;
    /// How this machine is elevated (drives error text).
    fn elevation(&self) -> Elevation;
}
