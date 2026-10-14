//! The clap command-line surface (§4.2, §4.3). M1 read/resolve commands only;
//! the mutating commands (`install`/`uninstall`/`prune`) are slice 10-2.

use std::path::PathBuf;

use bosun_core::Scope;
use clap::{Parser, Subcommand};

use crate::output::Format;

/// The `--scope` choice (§5.5).
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum ScopeArg {
    /// Machine-wide installs (apt, …); needs elevation.
    System,
    /// Per-user installs (cargo, …); must not run elevated.
    User,
}

impl From<ScopeArg> for Scope {
    fn from(s: ScopeArg) -> Scope {
        match s {
            ScopeArg::System => Scope::System,
            ScopeArg::User => Scope::User,
        }
    }
}

/// Pin and propagate tool versions across machines.
#[derive(Parser)]
#[command(name = "bosun", version, about)]
pub struct Cli {
    /// Output format; defaults to text on a terminal, json when piped (§4.3).
    #[arg(long, global = true, value_enum)]
    pub format: Option<Format>,

    /// Increase log verbosity (repeatable); logs go to stderr.
    #[arg(short, long, global = true, action = clap::ArgAction::Count)]
    pub verbose: u8,

    /// Preferred installer order, highest first (§5.3), e.g. `--installer brew,cargo`.
    #[arg(long, global = true, value_delimiter = ',')]
    pub installer: Vec<String>,

    /// The command to run.
    #[command(subcommand)]
    pub command: Command,
}

/// The M1 subcommands.
#[derive(Subcommand)]
pub enum Command {
    /// List registered backends and why the others were skipped.
    Backends,
    /// Report installed-vs-wanted versions for a manifest.
    Status {
        /// The manifest directory or file (default: current directory).
        path: Option<PathBuf>,
    },
    /// Resolve this platform and record versions in `bosun.lock`.
    Lock {
        /// The manifest directory or file (default: current directory).
        path: Option<PathBuf>,
        /// Report stale or missing targets instead of writing (a CI gate).
        #[arg(long)]
        check: bool,
    },
    /// Move the machine toward a manifest (install, upgrade, downgrade, hold).
    Install {
        /// The manifest directory or file (default: current directory).
        path: Option<PathBuf>,
        /// Show the plan and stop (needs no root, changes nothing).
        #[arg(long)]
        plan: bool,
        /// Restrict to one install scope (splits a mixed plan, §5.5).
        #[arg(long, value_enum)]
        scope: Option<ScopeArg>,
        /// Skip the confirmation prompt.
        #[arg(long)]
        yes: bool,
        /// Apply `bosun.lock` verbatim; never re-resolve (§7).
        #[arg(long)]
        frozen: bool,
    },
    /// Drop one manifest's claims and uninstall what it alone installed (§9).
    Uninstall {
        /// The manifest directory or file (default: current directory).
        path: Option<PathBuf>,
        /// Show the plan and stop.
        #[arg(long)]
        plan: bool,
        /// Restrict to one install scope.
        #[arg(long, value_enum)]
        scope: Option<ScopeArg>,
        /// Skip the confirmation prompt.
        #[arg(long)]
        yes: bool,
    },
    /// Drop claims of vanished manifests and uninstall the orphans (§9).
    Prune {
        /// Show the plan and stop.
        #[arg(long)]
        plan: bool,
        /// Restrict to one install scope.
        #[arg(long, value_enum)]
        scope: Option<ScopeArg>,
        /// Skip the confirmation prompt.
        #[arg(long)]
        yes: bool,
    },
}
