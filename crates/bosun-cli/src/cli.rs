//! The clap command-line surface (§4.2, §4.3).

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

/// The subcommands.
#[derive(Subcommand)]
pub enum Command {
    /// Add a package to the manifest, then lock it (never installs).
    Add {
        /// bosun's name for the package.
        name: String,
        /// A backend that may install it; repeat, or comma-separate, for
        /// several. Which one is used is this machine's preference
        /// (`--installer`, `BOSUN_INSTALLER`, config).
        #[arg(long = "backend", value_delimiter = ',')]
        backends: Vec<String>,
        /// The backend's own id for the package, where it differs from the
        /// name, for every --backend. Name one backend's own id with
        /// `--opt <backend>.package=<id>`.
        #[arg(long)]
        package: Option<String>,
        /// The version spec. Default: the newest version, up to the next one
        /// that may break it (`>=2.0.2|<3.0.0`).
        #[arg(long, allow_hyphen_values = true)]
        spec: Option<String>,
        /// The selector the install block applies to (`any`, `linux`,
        /// `linux:debian`). Default: `any` when every --backend is
        /// platform-independent (npm, cargo, …), else this machine's OS.
        #[arg(long = "on")]
        selector: Option<String>,
        /// A backend option: `key=value` for the one --backend, or
        /// `backend.key=value`. Repeatable.
        #[arg(long = "opt")]
        opts: Vec<String>,
        /// The manifest directory or file (default: current directory).
        #[arg(long)]
        manifest: Option<PathBuf>,
    },
    /// Remove a package from the manifest, uninstall it if bosun installed it
    /// and nothing else wants it, then lock.
    Remove {
        /// bosun's name for the package.
        name: String,
        /// The manifest directory or file (default: current directory).
        #[arg(long)]
        manifest: Option<PathBuf>,
        /// Show what would happen and stop (changes nothing).
        #[arg(long)]
        plan: bool,
        /// Restrict the uninstall to one install scope.
        #[arg(long, value_enum)]
        scope: Option<ScopeArg>,
        /// Skip the confirmation prompt.
        #[arg(long)]
        yes: bool,
    },
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
    },
    /// Rewrite an older-version manifest as the current version. The rewrite
    /// drops comments, so it shows the result and asks first.
    Migrate {
        /// The manifest directory or file (default: current directory).
        path: Option<PathBuf>,
        /// Show the migrated manifest and stop (changes nothing).
        #[arg(long)]
        plan: bool,
        /// Skip the confirmation prompt.
        #[arg(long)]
        yes: bool,
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
