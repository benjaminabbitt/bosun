//! `bosun migrate`: rewrite an older-version manifest as the current version.
//! Loading migrates in memory on every run; this writes the result, so `add`
//! and `remove` (which edit the text) can work on the file. annals edits only
//! the entries the migration changes, so comments elsewhere stay; the rewrite
//! is still shown and confirmed first, and the original kept beside it as
//! `<file>.v<N>`.

use std::io::Write;
use std::path::{Path, PathBuf};

use bosun_io::{manifest_version, upgrade_manifest, MANIFEST_VERSION};
use bosun_ports::FileAccess;
use serde::Serialize;

use crate::commands;
use crate::edit::{io_error, read, write};
use crate::error::CliError;
use crate::output::{render, Format, Render};

/// What `bosun migrate` did, or would do with `--plan`.
#[derive(Serialize)]
pub struct MigrateOutput {
    manifest: String,
    from: u32,
    to: u32,
    /// The migrated text; `None` when the manifest was already current.
    migrated: Option<String>,
    /// Whether every comment outside the rewritten entries was kept (annals
    /// rewrites the whole file, without comments, when it can't edit in place).
    comments_kept: bool,
    /// Where the original was kept; `None` with `--plan` or nothing to do.
    backup: Option<String>,
}

impl Render for MigrateOutput {
    fn text(&self) -> String {
        match (&self.migrated, &self.backup) {
            (None, _) => format!("{} is already version {}", self.manifest, self.to),
            (Some(text), None) => format!(
                "{} would be rewritten from version {} to {} ({}):\n{text}",
                self.manifest,
                self.from,
                self.to,
                self.comments_note()
            ),
            (Some(_), Some(backup)) => format!(
                "Migrated {} from version {} to {} ({}); the original is {backup}",
                self.manifest,
                self.from,
                self.to,
                self.comments_note()
            ),
        }
    }
}

impl MigrateOutput {
    fn comments_note(&self) -> &'static str {
        if self.comments_kept {
            "comments kept"
        } else {
            "comments are not kept"
        }
    }
}

/// The original's backup path: `bosun.yaml` at version 1 → `bosun.yaml.v1`.
fn backup_path(file: &Path, version: u32) -> PathBuf {
    let mut name = file.as_os_str().to_owned();
    name.push(format!(".v{version}"));
    PathBuf::from(name)
}

/// Run `bosun migrate [path] [--plan] [--yes]`, reading and writing the
/// manifest, and the original kept beside it, through `files` (§4.5).
pub fn run(
    files: &dyn FileAccess,
    path: Option<PathBuf>,
    plan: bool,
    yes: bool,
    format: Format,
    out: &mut impl Write,
) -> Result<i32, CliError> {
    let file = commands::manifest_file(files, path)?;
    let text = read(files, &file)?;
    let from = manifest_version(&text, &file).map_err(io_error)?;
    let upgraded = upgrade_manifest(&text, &file).map_err(io_error)?;
    let comments_kept = upgraded.as_ref().is_some_and(|u| u.comments_kept);
    let migrated = upgraded.map(|u| u.text);
    let mut output = MigrateOutput {
        manifest: file.display().to_string(),
        from,
        to: MANIFEST_VERSION,
        migrated,
        comments_kept,
        backup: None,
    };
    let Some(new_text) = output.migrated.clone() else {
        return emit(&output, format, out);
    };
    if plan {
        return emit(&output, format, out);
    }
    if !yes && !crate::confirm(&output.text(), "Rewrite it?")? {
        return Ok(0);
    }
    let backup = backup_path(&file, from);
    write(files, &backup, &text)?;
    write(files, &file, &new_text)?;
    output.backup = Some(backup.display().to_string());
    emit(&output, format, out)
}

fn emit(value: &MigrateOutput, format: Format, out: &mut impl Write) -> Result<i32, CliError> {
    render(value, format, out).map_err(|e| CliError::Io {
        path: PathBuf::from("<stdout>"),
        detail: e.to_string(),
    })?;
    Ok(value.exit_code())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_backup_keeps_the_name_and_adds_the_version() {
        assert_eq!(
            backup_path(Path::new("proj/bosun.yaml"), 1),
            PathBuf::from("proj/bosun.yaml.v1")
        );
    }
}
