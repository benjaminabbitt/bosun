//! CLI errors (§4.3): annals [`Diagnostic`]s with stable codes, so `--format
//! json` can render a machine-readable error and the process exits 1.

use std::path::PathBuf;

use annals_diagnostic::{Arg, Diagnostic};

/// A top-level CLI failure. Everything that stops a command before it produces
/// its typed value surfaces here; the process exits 1 (§4.8).
#[derive(Debug)]
pub enum CliError {
    /// A manifest file was not found at the resolved path.
    ManifestNotFound(PathBuf),
    /// A file could not be read or written.
    Io { path: PathBuf, detail: String },
    /// A manifest, lockfile, or fixture could not be parsed.
    Parse { what: String, detail: String },
    /// Resolution failed (bad spec, bad identity, index query).
    Resolve(String),
    /// The host environment could not be assembled (fixture/state/registry).
    Environment(String),
}

impl std::fmt::Display for CliError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CliError::ManifestNotFound(p) => write!(f, "no manifest at {}", p.display()),
            CliError::Io { path, detail } => write!(f, "{}: {detail}", path.display()),
            CliError::Parse { what, detail } => write!(f, "{what}: {detail}"),
            CliError::Resolve(d) => write!(f, "resolve failed: {d}"),
            CliError::Environment(d) => write!(f, "{d}"),
        }
    }
}

impl std::error::Error for CliError {}

impl Diagnostic for CliError {
    fn code(&self) -> &'static str {
        match self {
            CliError::ManifestNotFound(_) => "bosun.cli.manifest_not_found",
            CliError::Io { .. } => "bosun.cli.io",
            CliError::Parse { .. } => "bosun.cli.parse",
            CliError::Resolve(_) => "bosun.cli.resolve",
            CliError::Environment(_) => "bosun.cli.environment",
        }
    }

    fn args(&self) -> Vec<(&'static str, Arg)> {
        match self {
            CliError::ManifestNotFound(p) => vec![("path", p.display().to_string().into())],
            CliError::Io { path, detail } => {
                vec![
                    ("path", path.display().to_string().into()),
                    ("detail", detail.clone().into()),
                ]
            }
            CliError::Parse { what, detail } => {
                vec![
                    ("what", what.clone().into()),
                    ("detail", detail.clone().into()),
                ]
            }
            CliError::Resolve(d) | CliError::Environment(d) => vec![("detail", d.clone().into())],
        }
    }
}
