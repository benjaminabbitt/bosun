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
