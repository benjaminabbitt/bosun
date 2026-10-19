//! bosun CLI entry point. Parses commands, assembles the host environment,
//! runs one command to a typed value, and renders it through one formatter
//! (§4.3). Orchestration only — the logic lives in the library crates.

mod cli;
mod commands;
mod edit;
mod env;
mod errmsg;
mod error;
mod export;
mod fixture;
mod logmsg;
mod migrate;
mod output;
mod render;
mod target_root;

use std::io::{IsTerminal, Write};

use bosun_apply::{check_privilege, check_release_privilege, defer_system_records, Plan};
use bosun_core::Scope;
use clap::Parser;
use tracing_subscriber::EnvFilter;

use crate::cli::{Cli, Command, RenderStyle};
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
    refuse_remove_scope(&cli.command)?;
    // render may be for another machine, so it probes this one only when the
    // target is this machine (§4.7).
    if let Command::Render { style } = &cli.command {
        return render_command(style, &cli.installer);
    }
    let env = env::assemble()?;
    let pref = commands::preference(&env, &cli.installer)?;
    let mut out = std::io::stdout().lock();
    match &cli.command {
        Command::Add {
            name,
            backends,
            package,
            spec,
            selector,
            opts,
            manifest,
        } => {
            let opts = edit::AddOpts {
                name: name.clone(),
                backends: backends.clone(),
                package: package.clone(),
                spec: spec.clone(),
                selector: selector.clone(),
                opts: opts.clone(),
                manifest: manifest.clone(),
            };
            let value = edit::add(&env, opts, &pref)?;
            emit(&value, format, &mut out)
        }
        Command::Remove {
            name,
            manifest,
            plan,
            yes,
            ..
        } => {
            let removal = edit::prepare_remove(&env, name, manifest.clone())?;
            if *plan {
                return emit(&removal.planned(), format, &mut out);
            }
            // Release first: the manifest is edited only once the package is
            // uninstalled (or its claim dropped), so a refused or failed apply
            // leaves everything as it was.
            let command = format!("remove {name}");
            // A system-scope claim drop can't wait for a later run: once the
            // entry is gone, none would drop it.
            if let Err(e) = check_release_privilege(&removal.plan, env.machine()) {
                return Ok(render_privilege_error(
                    &e,
                    env.machine().elevation(),
                    &command,
                    format,
                    &mut out,
                ));
            }
            let report = if removal.plan.actions.is_empty() {
                None
            } else {
                let m = Mutation {
                    command: &command,
                    plan_only: false,
                    scope: None,
                    yes: *yes,
                };
                match apply_mutation(&env, m, &removal.plan, format, &mut out)? {
                    Applied::Done(report) => Some(report),
                    Applied::Stopped(code) => return Ok(code),
                    Applied::Failed(report) => {
                        render(&report, format, &mut out).map_err(io_err)?;
                        return Ok(1);
                    }
                }
            };
            let value = removal.finish(&env, report, &pref)?;
            emit(&value, format, &mut out)
        }
        Command::Export { path, plan, yes } => {
            let export = export::prepare(&env, path.clone(), &pref)?;
            if *plan || export.is_empty() {
                return emit(&export.planned(), format, &mut out);
            }
            if !*yes && !confirm(&export.planned().text(), "Write it?")? {
                return Ok(0);
            }
            let value = export.finish(&env, &pref)?;
            emit(&value, format, &mut out)
        }
        Command::Backends => {
            let value = commands::backends(&env);
            emit(&value, format, &mut out)
        }
        Command::Status { path: Some(path) } => {
            let value = commands::status(&env, path.clone(), &pref)?;
            emit(&value, format, &mut out)
        }
        Command::Status { path: None } => emit(&commands::machine_status(&env)?, format, &mut out),
        Command::Lock { path } => {
            let value = commands::lock(&env, path.clone(), &pref)?;
            emit(&value, format, &mut out)
        }
        Command::Upgrade { names, manifest } => {
            let value = commands::upgrade(&env, names, manifest.clone(), &pref)?;
            emit(&value, format, &mut out)
        }
        Command::Migrate { path, plan, yes } => migrate::run(
            env.user_files().as_ref(),
            path.clone(),
            *plan,
            *yes,
            format,
            &mut out,
        ),
        Command::Install {
            path,
            plan,
            scope,
            yes,
            frozen,
        } => {
            let (built, pending) = commands::install_plan(
                &env,
                InstallOpts {
                    path: path.clone(),
                    frozen: *frozen,
                },
                &pref,
            )?;
            let built = defer_system_records(built, env.machine().privilege());
            let m = Mutation {
                command: "install",
                plan_only: *plan,
                scope: scope.map(Into::into),
                yes: *yes,
            };
            if m.plan_only {
                return emit(&built, format, &mut out);
            }
            let requested = m.scope;
            match apply_mutation(&env, m, &built, format, &mut out)? {
                Applied::Stopped(code) => Ok(code),
                Applied::Done(report) => {
                    // The lock follows what was installed, and only then.
                    if let Some(mut pending) = pending {
                        pending.only_requested_scope(requested);
                        pending.write()?;
                    }
                    render(&report, format, &mut out).map_err(io_err)?;
                    Ok(0)
                }
                Applied::Failed(report) => {
                    render(&report, format, &mut out).map_err(io_err)?;
                    Ok(1)
                }
            }
        }
        Command::Uninstall {
            path,
            plan,
            scope,
            yes,
        } => {
            let built = defer_system_records(
                commands::uninstall_plan(&env, path.clone())?,
                env.machine().privilege(),
            );
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
        Command::Render { .. } => unreachable!("render is dispatched before the host is probed"),
        Command::Prune { plan, scope, yes } => {
            let built =
                defer_system_records(commands::prune_plan(&env)?, env.machine().privilege());
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

/// `remove --scope` is refused before anything runs: removing an entry cleans
/// up every install scope it touches, and a scope left behind would keep a
/// claim no entry stands for (§4.2).
fn refuse_remove_scope(command: &Command) -> Result<(), CliError> {
    if let Command::Remove { scope: Some(_), .. } = command {
        return Err(CliError::Usage(errmsg::REMOVE_NO_SCOPE.to_string()));
    }
    Ok(())
}

/// Where `render` reads the target from, if anywhere: `--image` or `--rootfs`.
fn from(machine: &cli::RenderMachine) -> Option<target_root::From> {
    match (&machine.image, &machine.rootfs) {
        (Some(image), _) => Some(target_root::From::Image(image.clone())),
        (None, Some(dir)) => Some(target_root::From::Rootfs(dir.clone())),
        (None, None) => None,
    }
}

/// `bosun render` (§8.3). The output style is the subcommand's, whatever
/// `--format` says: `$(bosun render args)` is piped, which would otherwise
/// mean JSON.
fn render_command(style: &RenderStyle, installer: &[String]) -> Result<i32, CliError> {
    let one = |t: &cli::RenderTarget| render::Request {
        path: t.path.clone(),
        targets: t.target.iter().cloned().collect(),
        from: from(&t.machine),
        distro: t.machine.distro.clone(),
        like: t.machine.like.clone(),
        libc: t.machine.libc.clone(),
        installer: installer.to_vec(),
    };
    let text = match style {
        RenderStyle::Args(t) => {
            let (targets, rendered) = render::rendered(one(t))?;
            rendered.build_args(&targets[0])
        }
        RenderStyle::Env(t) => {
            let (targets, rendered) = render::rendered(one(t))?;
            rendered.env_lines(&targets[0])
        }
        RenderStyle::Json(t) => {
            let req = render::Request {
                path: t.path.clone(),
                targets: t.targets.clone(),
                from: from(&t.machine),
                distro: t.machine.distro.clone(),
                like: t.machine.like.clone(),
                libc: t.machine.libc.clone(),
                installer: installer.to_vec(),
            };
            render::json(&render::rendered(req)?.1)
        }
    };
    writeln!(std::io::stdout().lock(), "{text}").map_err(io_err)?;
    Ok(0)
}

/// Render `value` and return its exit code.
fn emit<T: Render>(value: &T, format: Format, out: &mut impl Write) -> Result<i32, CliError> {
    render(value, format, out).map_err(io_err)?;
    Ok(value.exit_code())
}

/// A mutating command and its flags.
struct Mutation<'a> {
    /// The command and its operands, for the privilege remedy text.
    command: &'a str,
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
    m: Mutation<'_>,
    plan: Plan,
    format: Format,
    out: &mut impl Write,
) -> Result<i32, CliError> {
    // --plan stops here; it needs no root and changes nothing. Rendered
    // through the same path as any value, so its exit code is the plan's.
    if m.plan_only {
        return emit(&plan, format, out);
    }
    match apply_mutation(env, m, &plan, format, out)? {
        Applied::Stopped(code) => Ok(code),
        Applied::Done(report) => {
            render(&report, format, out).map_err(io_err)?;
            Ok(0)
        }
        Applied::Failed(report) => {
            render(&report, format, out).map_err(io_err)?;
            Ok(1)
        }
    }
}

/// How the privilege → confirm → apply flow ended.
enum Applied {
    /// Stopped before applying (blocking diagnostic, privilege, refused or
    /// declined confirmation), with the exit code; anything to say was said.
    Stopped(i32),
    /// Applied, every action succeeded.
    Done(bosun_apply::ApplyReport),
    /// Applied, and some action failed.
    Failed(bosun_apply::ApplyReport),
}

/// Privilege → confirm → apply for `plan` (§4.8); the caller renders the report.
fn apply_mutation(
    env: &Env,
    m: Mutation<'_>,
    plan: &Plan,
    format: Format,
    out: &mut impl Write,
) -> Result<Applied, CliError> {
    // A blocking diagnostic (unsatisfiable, empty intersection) refuses apply.
    if plan.has_errors() {
        render(plan, format, out).map_err(io_err)?;
        return Ok(Applied::Stopped(1));
    }
    // Privilege is checked against what the plan changes (§5.5).
    if let Err(e) = check_privilege(plan, env.machine(), m.scope) {
        return Ok(Applied::Stopped(render_privilege_error(
            &e,
            env.machine().elevation(),
            m.command,
            format,
            out,
        )));
    }
    // Confirm (§4.8): --yes skips; a TTY prompts; no TTY applies.
    if !m.yes && !confirm(&Render::text(plan), "Apply?")? {
        return Ok(Applied::Stopped(0));
    }
    // Apply each scope the plan touches (a mixed plan was already refused unless
    // --scope narrowed it, §5.5), each in its own state transaction.
    let mut report = bosun_apply::ApplyReport {
        outcomes: Vec::new(),
    };
    for s in apply_scopes(plan, m.scope) {
        let part = env
            .apply(s, plan)
            .map_err(|e| CliError::Resolve(e.to_string()))?;
        report.outcomes.extend(part.outcomes);
    }
    report.outcomes.extend(plan.deferred());
    let any_failed = report
        .outcomes
        .iter()
        .any(|o| matches!(o.result, bosun_apply::ApplyResult::Failed { .. }));
    Ok(if any_failed {
        Applied::Failed(report)
    } else {
        Applied::Done(report)
    })
}

/// Show `shown` and ask `question` on the terminal (stderr); anything but yes
/// is a decline, said as such. No terminal (CI, a container build, a pipe) is
/// yes: nobody is there to answer, so `shown` is still printed for the log.
pub(crate) fn confirm(shown: &str, question: &str) -> Result<bool, CliError> {
    eprintln!("{shown}");
    if !std::io::stdin().is_terminal() {
        eprintln!("{question} no terminal to ask; applying.");
        return Ok(true);
    }
    eprint!("{question} [y/N] ");
    std::io::stderr().flush().ok();
    let mut line = String::new();
    std::io::stdin().read_line(&mut line).map_err(io_err)?;
    let yes = matches!(line.trim(), "y" | "Y" | "yes" | "Yes");
    if !yes {
        eprintln!("Aborted; nothing changed.");
    }
    Ok(yes)
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
