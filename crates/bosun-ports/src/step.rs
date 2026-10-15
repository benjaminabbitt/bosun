//! The step list: typed, fully-determined mutations an [`Installer`](crate::Installer)
//! describes but does not perform (§5.1). One description drives every driver:
//! a host executor runs the steps, and the container renderer prints them as
//! Dockerfile instructions, so the values must be self-contained — argv is a
//! `Vec<String>`, never a shell string, so a renderer can quote it safely.

use std::path::PathBuf;

use bosun_core::Scope;

use crate::error::PortError;

/// Carries out a [`Steps`] list on the host (§5.1): runs each command, fetches
/// and checksum-verifies each download, extracts each archive member. The
/// adapter lives in bosun-host; the applier (bosun-apply) drives it through
/// this port so it never depends on the host crate, and a container renderer
/// can be a peer driver (§8).
pub trait StepRunner {
    /// Carry out `steps` in order, stopping at the first failure.
    fn execute(&self, steps: &[Step]) -> Result<(), PortError>;
}

/// One mutation step.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Step {
    /// Run a command. Host: via [`CommandRunner`](crate::CommandRunner) at the
    /// given scope's privilege (§5.5). Container: a `RUN` with shell-quoted argv.
    Run {
        /// The program and its arguments; `argv[0]` is the program.
        argv: Vec<String>,
        /// Environment variables to set (e.g. `DEBIAN_FRONTEND=noninteractive`).
        env: Vec<(String, String)>,
        /// The install scope this runs under.
        scope: Scope,
    },
    /// Download `url` to `dest`, verifying its sha256 when one is known.
    /// Host: bosun fetches and checks. Container: `ADD --checksum=sha256:…
    /// <url> <dest>` (a plain `ADD` without one).
    Fetch {
        /// The source URL.
        url: String,
        /// The expected sha256, hex-encoded; `None` when the source publishes
        /// none, and the download is written unverified.
        sha256: Option<String>,
        /// Where to write it.
        dest: PathBuf,
    },
    /// Extract one member of an archive to `dest` with file mode `mode`. The
    /// member is found by file name at any depth (the archive root, a wrapping
    /// directory, a nested `bin/`); exactly one entry may carry that name.
    Extract {
        /// The archive file.
        archive: PathBuf,
        /// The member's file name (not a path) within the archive.
        member: String,
        /// Where to write the extracted file.
        dest: PathBuf,
        /// The unix file mode to set (e.g. `0o755`).
        mode: u32,
    },
}

/// A sequence of [`Step`]s an installer operation expands to.
pub type Steps = Vec<Step>;
