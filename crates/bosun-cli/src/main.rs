//! bosun CLI entry point. Parses commands, assembles the host environment,
//! runs one command to a typed value, and renders it through one formatter
//! (§4.3). Orchestration only — the logic lives in the library crates.

mod cli;
mod commands;
mod env;
mod error;
mod fixture;
mod output;

use std::io::{IsTerminal, Write};

use bosun_apply::{check_privilege, Plan};
use bosun_core::Scope;
use clap::Parser;
use tracing_subscriber::EnvFilter;

use crate::cli::{Cli, Command};
use crate::commands::InstallOpts;
use crate::env::Env;
use crate::error::CliError;
use crate::output::{render, render_error, render_privilege_error, Format, Render};

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
    match &cli.command {
        Command::Backends => {
            let value = commands::backends(&env);
            emit(&value, format, &mut out)
        }
        Command::Status { path } => {
            let value = commands::status(&env, path.clone(), &pref)?;
            emit(&value, format, &mut out)
        }
        Command::Lock { path, check } => {
            let value = commands::lock(&env, path.clone(), *check, &pref)?;
            emit(&value, format, &mut out)
        }
        Command::Install {
            path,
            plan,
            scope,
            yes,
            frozen,
        } => {
            let built = commands::install_plan(
                &env,
                InstallOpts {
                    path: path.clone(),
                    frozen: *frozen,
                },
                &pref,
            )?;
            run_mutation(
                &env,
                Mutation {
                    command: "install",
                    plan_only: *plan,
                    scope: scope.map(Into::into),
                    yes: *yes,
                },
                built,
                format,
                &mut out,
            )
        }
        Command::Uninstall {
            path,
            plan,
            scope,
            yes,
        } => {
            let built = commands::uninstall_plan(&env, path.clone())?;
            run_mutation(
                &env,
                Mutation {
                    command: "uninstall",
                    plan_only: *plan,
                    scope: scope.map(Into::into),
                    yes: *yes,
                },
                built,
                format,
                &mut out,
            )
        }
        Command::Prune { plan, scope, yes } => {
            let built = commands::prune_plan(&env)?;
            run_mutation(
                &env,
                Mutation {
                    command: "prune",
                    plan_only: *plan,
                    scope: scope.map(Into::into),
                    yes: *yes,
                },
                built,
                format,
                &mut out,
            )
        }
    }
}

/// Render `value` and return its exit code.
fn emit<T: Render>(value: &T, format: Format, out: &mut impl Write) -> Result<i32, CliError> {
    render(value, format, out).map_err(io_err)?;
    Ok(value.exit_code())
}

/// A mutating command and its flags.
struct Mutation {
    /// The command name, for the privilege remedy text.
    command: &'static str,
    /// `--plan`: stop after planning.
    plan_only: bool,
    /// `--scope`: restrict to one install scope.
    scope: Option<Scope>,
    /// `--yes`: skip the confirmation prompt.
    yes: bool,
}

/// The plan → privilege → confirm → apply flow for a mutating command (§4.8).
fn run_mutation(
    env: &Env,
    m: Mutation,
    plan: Plan,
    format: Format,
    out: &mut impl Write,
) -> Result<i32, CliError> {
    // --plan stops here; it needs no root and changes nothing. Rendered
    // through the same path as any value, so its exit code is the plan's.
    if m.plan_only {
        return emit(&plan, format, out);
    }
    // A blocking diagnostic (unsatisfiable, empty intersection) refuses apply.
    if plan.has_errors() {
        render(&plan, format, out).map_err(io_err)?;
        return Ok(1);
    }
    // Privilege is checked against what the plan changes (§5.5).
    if let Err(e) = check_privilege(&plan, env.machine(), m.scope) {
        return Ok(render_privilege_error(
            &e,
            env.machine().elevation(),
            m.command,
            format,
            out,
        ));
    }
    // Confirm (§4.8): --yes skips; a TTY prompts; no TTY and no --yes refuses.
    if !m.yes {
        if !std::io::stdin().is_terminal() {
            return Err(CliError::Environment(
                "refusing to apply without a terminal; re-run with --yes".to_string(),
            ));
        }
        eprintln!("{}", Render::text(&plan));
        eprint!("Apply? [y/N] ");
        std::io::stderr().flush().ok();
        let mut line = String::new();
        std::io::stdin().read_line(&mut line).map_err(io_err)?;
        if !matches!(line.trim(), "y" | "Y" | "yes" | "Yes") {
            eprintln!("Aborted; nothing changed.");
            return Ok(0);
        }
    }
    // Apply each scope the plan touches (a mixed plan was already refused unless
    // --scope narrowed it, §5.5), each in its own state transaction.
    let mut report = bosun_apply::ApplyReport {
        outcomes: Vec::new(),
    };
    for s in apply_scopes(&plan, m.scope) {
        let part = env
            .apply(s, &plan)
            .map_err(|e| CliError::Resolve(e.to_string()))?;
        report.outcomes.extend(part.outcomes);
    }
    let any_failed = report
        .outcomes
        .iter()
        .any(|o| matches!(o.result, bosun_apply::ApplyResult::Failed { .. }));
    render(&report, format, out).map_err(io_err)?;
    Ok(if any_failed { 1 } else { 0 })
}

/// The scopes to apply: the requested one, else every scope the plan touches.
fn apply_scopes(plan: &Plan, requested: Option<Scope>) -> Vec<Scope> {
    if let Some(s) = requested {
        return vec![s];
    }
    let mut scopes = Vec::new();
    for action in &plan.actions {
        if !scopes.contains(&action.scope) {
            scopes.push(action.scope);
        }
    }
    scopes
}

fn io_err(e: std::io::Error) -> CliError {
    CliError::Io {
        path: std::path::PathBuf::from("<stdout>"),
        detail: e.to_string(),
    }
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
