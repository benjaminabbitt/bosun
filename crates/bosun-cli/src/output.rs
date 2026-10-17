//! Output rendering (§4.3): every command produces one typed value, rendered
//! through a single formatter. `text` is the default on a TTY; piped output
//! defaults to `json`. `yaml` is available for config-shaped reading.

use std::io::{self, IsTerminal, Write};

use serde::Serialize;

use crate::error::CliError;

/// The output format (§4.3).
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum Format {
    /// Human-readable lines.
    Text,
    /// Pretty JSON.
    Json,
    /// YAML.
    Yaml,
}

impl Format {
    /// The effective format: the explicit `--format`, else text on a TTY and
    /// json when piped (§4.3).
    pub fn resolve(explicit: Option<Format>) -> Format {
        explicit.unwrap_or_else(|| {
            if io::stdout().is_terminal() {
                Format::Text
            } else {
                Format::Json
            }
        })
    }
}

/// A command's typed output: serialisable for json/yaml, with a text form.
pub trait Render: Serialize {
    /// The human-readable rendering.
    fn text(&self) -> String;
    /// The process exit code this result implies (§4.8): 0 none, 2 pending.
    fn exit_code(&self) -> i32 {
        0
    }
}

/// Render `value` in `format` to `out`.
pub fn render<T: Render>(value: &T, format: Format, out: &mut impl Write) -> io::Result<()> {
    match format {
        Format::Text => writeln!(out, "{}", value.text()),
        Format::Json => writeln!(
            out,
            "{}",
            serde_json::to_string_pretty(value).expect("output serialization is infallible")
        ),
        Format::Yaml => write!(
            out,
            "{}",
            serde_norway::to_string(value).expect("output serialization is infallible")
        ),
    }
}

/// Render an error as the typed error value (§4.3) and return exit code 1.
pub fn render_error(err: &CliError, format: Format, out: &mut impl Write) -> i32 {
    use annals_diagnostic::Diagnostic;
    let value = ErrorValue {
        error: err.code(),
        message: err.to_string(),
    };
    let _ = match format {
        Format::Text => writeln!(out, "error: {} [{}]", value.message, value.error),
        Format::Json => writeln!(out, "{}", serde_json::to_string_pretty(&value).unwrap()),
        Format::Yaml => write!(out, "{}", serde_norway::to_string(&value).unwrap()),
    };
    1
}

#[derive(Serialize)]
struct ErrorValue {
    error: &'static str,
    message: String,
}

/// Render a privilege failure with its §5.5 remedy and return exit code 1. The
/// remedy wording comes from the machine's `Elevation`.
pub fn render_privilege_error(
    err: &bosun_apply::PrivilegeError,
    elevation: bosun_ports::Elevation,
    command: &str,
    format: Format,
    out: &mut impl Write,
) -> i32 {
    use bosun_apply::PrivilegeError as P;
    let (code, packages) = match err {
        P::ElevationRequired { packages } | P::ReleaseNeedsElevation { packages } => {
            ("elevation_required", packages.as_slice())
        }
        P::MustNotBeElevated { packages } => ("must_not_be_elevated", packages.as_slice()),
        P::MixedScope { .. } => ("mixed_scope", [].as_slice()),
    };
    let value = PrivilegeErrorValue {
        error: code,
        message: err.to_string(),
        remedy: remedy_text(err, elevation, command),
        packages: packages
            .iter()
            .map(|p| PackageRefValue {
                name: p.name.clone(),
                backend: p.backend.as_str().to_string(),
            })
            .collect(),
    };
    let _ = match format {
        Format::Text => writeln!(out, "error: {}\n  {}", value.message, value.remedy),
        Format::Json => writeln!(out, "{}", serde_json::to_string_pretty(&value).unwrap()),
        Format::Yaml => write!(out, "{}", serde_norway::to_string(&value).unwrap()),
    };
    1
}

fn remedy_text(
    err: &bosun_apply::PrivilegeError,
    elevation: bosun_ports::Elevation,
    command: &str,
) -> String {
    use bosun_apply::PrivilegeError as P;
    use bosun_ports::Elevation as E;
    match (err, elevation) {
        (P::ElevationRequired { .. }, E::Sudo) => {
            format!("re-run as root: sudo bosun {command} --scope system")
        }
        (P::ElevationRequired { .. }, E::AdministratorTerminal) => {
            format!("open a terminal as Administrator and re-run: bosun {command} --scope system")
        }
        (P::ReleaseNeedsElevation { .. }, E::Sudo) => {
            format!("re-run as root: sudo bosun {command}")
        }
        (P::ReleaseNeedsElevation { .. }, E::AdministratorTerminal) => {
            format!("open a terminal as Administrator and re-run: bosun {command}")
        }
        (P::MustNotBeElevated { .. }, _) => {
            format!("re-run without elevation: bosun {command} --scope user")
        }
        (P::MixedScope { .. }, E::Sudo) => format!(
            "run each scope: `sudo bosun {command} --scope system`, then `bosun {command} --scope user`"
        ),
        (P::MixedScope { .. }, E::AdministratorTerminal) => format!(
            "run each scope: `bosun {command} --scope system` as Administrator, then `bosun {command} --scope user`"
        ),
    }
}

#[derive(Serialize)]
struct PrivilegeErrorValue {
    error: &'static str,
    message: String,
    remedy: String,
    packages: Vec<PackageRefValue>,
}

#[derive(Serialize)]
struct PackageRefValue {
    name: String,
    backend: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use bosun_apply::{PackageRef, PrivilegeError};
    use bosun_core::BackendId;
    use bosun_ports::Elevation;

    fn release() -> PrivilegeError {
        PrivilegeError::ReleaseNeedsElevation {
            packages: vec![PackageRef {
                name: "jq".to_string(),
                backend: BackendId::new("apt"),
            }],
        }
    }

    #[test]
    fn a_release_is_re_run_elevated_as_it_was_given_with_no_scope() {
        assert_eq!(
            remedy_text(&release(), Elevation::Sudo, "remove jq"),
            "re-run as root: sudo bosun remove jq"
        );
        assert_eq!(
            remedy_text(&release(), Elevation::AdministratorTerminal, "remove jq"),
            "open a terminal as Administrator and re-run: bosun remove jq"
        );
    }

    #[test]
    fn a_release_error_is_elevation_required_naming_its_packages() {
        let mut out = Vec::new();
        let code = render_privilege_error(
            &release(),
            Elevation::Sudo,
            "remove jq",
            Format::Json,
            &mut out,
        );
        assert_eq!(code, 1);
        let value: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(value["error"], "elevation_required");
        assert_eq!(value["packages"][0]["name"], "jq");
        assert_eq!(value["packages"][0]["backend"], "apt");
    }
}
