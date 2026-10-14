//! The clap command-line surface (§4.2, §4.3). M1 read/resolve commands only;
//! the mutating commands (`install`/`uninstall`/`prune`) are slice 10-2.

use std::path::PathBuf;

use clap::{Parser, Subcommand};

use crate::output::Format;

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
}
