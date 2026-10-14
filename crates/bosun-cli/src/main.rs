//! bosun CLI entry point. Parses commands, assembles the host environment,
//! runs one command to a typed value, and renders it through one formatter
//! (§4.3). Orchestration only — the logic lives in the library crates.

mod cli;
mod commands;
mod env;
mod error;
mod fixture;
mod output;

use std::io::Write;

use clap::Parser;
use tracing_subscriber::EnvFilter;

use crate::cli::{Cli, Command};
use crate::error::CliError;
use crate::output::{render, render_error, Format, Render};

fn main() {
    let cli = Cli::parse();
    init_tracing(cli.verbose);
    let format = Format::resolve(cli.format);
    let code = match dispatch(&cli, format) {
        Ok(code) => code,
        Err(err) => render_error(&err, format, &mut std::io::stdout().lock()),
    };
    std::process::exit(code);
}

/// Run the selected command, render its value, and return the exit code.
fn dispatch(cli: &Cli, format: Format) -> Result<i32, CliError> {
    let env = env::assemble()?;
    let pref = commands::preference(&cli.installer);
    let mut out = std::io::stdout().lock();
    let code = match &cli.command {
        Command::Backends => {
            let value = commands::backends(&env);
            emit(&value, format, &mut out)?
        }
        Command::Status { path } => {
            let value = commands::status(&env, path.clone(), &pref)?;
            emit(&value, format, &mut out)?
        }
        Command::Lock { path, check } => {
            let value = commands::lock(&env, path.clone(), *check, &pref)?;
            emit(&value, format, &mut out)?
        }
    };
    Ok(code)
}

/// Render `value` and return its exit code.
fn emit<T: Render>(value: &T, format: Format, out: &mut impl Write) -> Result<i32, CliError> {
    render(value, format, out).map_err(|e| CliError::Io {
        path: std::path::PathBuf::from("<stdout>"),
        detail: e.to_string(),
    })?;
    Ok(value.exit_code())
}

/// Log to stderr at a level driven by `-v` (or `RUST_LOG`).
fn init_tracing(verbose: u8) {
    let level = match verbose {
        0 => "warn",
        1 => "info",
        2 => "debug",
        _ => "trace",
    };
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(level));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .try_init();
}
