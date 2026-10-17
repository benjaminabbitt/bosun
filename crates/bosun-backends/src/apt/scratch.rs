//! apt's remote index through this machine's apt (§4.7). When `apt-get` is
//! here, a release's sources are written into a scratch root under bosun's
//! cache, with its own configuration, an empty dpkg status and the target's
//! architecture; `apt-get update` fetches them there and `apt-cache policy`
//! answers, so apt's own priorities, pins and phasing choose. Nothing of this
//! machine's apt is read or written: `APT_CONFIG` points at the scratch
//! configuration, which apt reads before any other.
//!
//! What the scratch root carries from the target's root: its source entries,
//! its `preferences` and `preferences.d` (pins), and the keys it trusts
//! (`trusted.gpg`, `trusted.gpg.d`, and each `Signed-By` keyring), so apt
//! checks each release file's signature as the target's apt would. Its
//! `apt.conf.d` is not carried: it may hold hooks (`APT::Update::Post-Invoke`)
//! that would run here. An entry with no key to check it against (stock
//! sources, or a root without the keyring) is marked `Trusted: yes` in the
//! scratch configuration only, and read over HTTPS: bosun trusts a release
//! file for being fetched over HTTPS, its package indexes for matching the
//! hashes it lists, which apt still checks.

use std::fs;
use std::path::{Path, PathBuf};

use bosun_core::Arch;
use bosun_ports::{Command, CommandRunner, PortError};
use sha2::{Digest, Sha256};
use tracing::{debug, warn};

use super::remote::{https, RELEASE_TTL};
use super::sources::SourceEntry;
use crate::{errmsg, logmsg};

/// One release's sources in a scratch root, ready for `apt-get update`.
#[derive(Debug, PartialEq, Eq)]
pub struct Scratch {
    dir: PathBuf,
}

fn io_err(path: &Path, e: std::io::Error) -> PortError {
    PortError::Backend {
        detail: format!("{}: {}: {e}", errmsg::APT_CACHE, path.display()),
    }
}

fn write(path: &Path, bytes: &[u8]) -> Result<(), PortError> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).map_err(|e| io_err(dir, e))?;
    }
    fs::write(path, bytes).map_err(|e| io_err(path, e))
}

/// The regular files in `dir`, in name order; none when it can't be read.
fn files_in(dir: &Path) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = fs::read_dir(dir)
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.is_file())
        .collect();
    files.sort();
    files
}

/// One deb822 field: continuation lines indented, an empty one as ` .`.
fn field(out: &mut String, name: &str, value: &str) {
    let mut lines = value.split('\n');
    out.push_str(name);
    out.push(':');
    if let Some(first) = lines.next() {
        if !first.is_empty() {
            out.push(' ');
            out.push_str(first);
        }
    }
    for line in lines {
        out.push_str(if line.is_empty() { "\n ." } else { "\n " });
        out.push_str(line);
    }
    out.push('\n');
}

/// What the scratch root is made of, before it is written: its files by
/// path relative to the scratch directory, with `@SCRATCH@` standing for
/// that directory's path.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Plan {
    /// Relative path → contents.
    pub files: Vec<(String, Vec<u8>)>,
}

const HERE: &str = "@SCRATCH@";

impl Plan {
    fn add(&mut self, path: String, bytes: Vec<u8>) {
        self.files.push((path, bytes));
    }

    /// A short name for `parts`, each length-prefixed so no two lists of
    /// parts run together alike.
    fn digest(parts: &[&[u8]]) -> String {
        let mut h = Sha256::new();
        for part in parts {
            h.update((part.len() as u64).to_le_bytes());
            h.update(part);
        }
        h.finalize()[..8]
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }

    /// The scratch directory's name: a digest of every file.
    fn key(&self) -> String {
        let parts: Vec<&[u8]> = self
            .files
            .iter()
            .flat_map(|(path, bytes)| [path.as_bytes(), bytes.as_slice()])
            .collect();
        Plan::digest(&parts)
    }
}

/// The keyring paths and the fingerprints `signed_by` names; `None` for an
/// inline key block, which travels as it is.
fn keyrings(signed_by: &str) -> Option<(Vec<&str>, Vec<&str>)> {
    if signed_by.contains("-----BEGIN PGP") {
        return None;
    }
    Some(
        signed_by
            .split([' ', '\t', ',', '\n'])
            .filter(|t| !t.is_empty())
            .partition(|t| t.starts_with('/')),
    )
}

/// The `Signed-By` entry `e` gets in the scratch root, each keyring it names
/// copied from `root` into `plan`: `Some` when apt can check the entry's
/// release file there, `None` when there is no key to check it against.
/// `trusted_parts` is whether the root has keys in `trusted.gpg(.d)`, which
/// apt checks an entry without a keyring of its own against.
fn checking(
    e: &SourceEntry,
    root: Option<&Path>,
    trusted_parts: bool,
    plan: &mut Plan,
) -> Option<Option<String>> {
    let Some(signed_by) = &e.signed_by else {
        return trusted_parts.then_some(None);
    };
    let Some((paths, fingerprints)) = keyrings(signed_by) else {
        return Some(Some(signed_by.clone()));
    };
    if paths.is_empty() && !trusted_parts {
        return None;
    }
    let mut value = Vec::new();
    for p in &paths {
        let bytes = fs::read(root?.join(p.trim_start_matches('/'))).ok()?;
        let name = Path::new(p)
            .file_name()
            .unwrap_or_default()
            .to_string_lossy();
        let rel = format!("keys/{}-{name}", Plan::digest(&[&bytes]));
        value.push(format!("{HERE}/{rel}"));
        if !plan.files.iter().any(|(known, _)| *known == rel) {
            plan.add(rel, bytes);
        }
    }
    value.extend(fingerprints.iter().map(|f| f.to_string()));
    Some(Some(value.join(" ")))
}

/// The scratch root for `entries` read for `arch`, with what `root` (the
/// target's root filesystem, when given) holds of apt's configuration.
pub fn plan(root: Option<&Path>, entries: &[SourceEntry], arch: Arch) -> Plan {
    let mut plan = Plan::default();
    let mut trusted_parts = false;
    if let Some(apt) = root.map(|r| r.join("etc/apt")) {
        if let Ok(bytes) = fs::read(apt.join("trusted.gpg")) {
            plan.add("etc/apt/trusted.gpg".to_string(), bytes);
            trusted_parts = true;
        }
        for (dir, is_key) in [("trusted.gpg.d", true), ("preferences.d", false)] {
            for f in files_in(&apt.join(dir)) {
                if let Ok(bytes) = fs::read(&f) {
                    let name = f.file_name().unwrap_or_default().to_string_lossy();
                    plan.add(format!("etc/apt/{dir}/{name}"), bytes);
                    trusted_parts |= is_key;
                }
            }
        }
        if let Ok(bytes) = fs::read(apt.join("preferences")) {
            plan.add("etc/apt/preferences".to_string(), bytes);
        }
    }
    let mut sources = String::new();
    for e in entries {
        let (uri, signed_by, trusted) = if e.trusted {
            (e.uri.clone(), None, true)
        } else {
            match checking(e, root, trusted_parts, &mut plan) {
                Some(signed_by) => (e.uri.clone(), signed_by, false),
                None => {
                    debug!(message = logmsg::APT_SCRATCH_UNCHECKED, uri = %e.uri, suite = %e.suite);
                    (https(&e.uri), None, true)
                }
            }
        };
        if !sources.is_empty() {
            sources.push('\n');
        }
        field(&mut sources, "Types", "deb");
        field(&mut sources, "URIs", &uri);
        field(&mut sources, "Suites", &e.suite);
        if !e.components.is_empty() {
            field(&mut sources, "Components", &e.components.join(" "));
        }
        if !e.architectures.is_empty() {
            field(&mut sources, "Architectures", &e.architectures.join(" "));
        }
        if let Some(s) = &signed_by {
            field(&mut sources, "Signed-By", s);
        }
        if trusted {
            field(&mut sources, "Trusted", "yes");
        }
        for (name, value) in &e.options {
            field(&mut sources, name, value);
        }
    }
    plan.add(
        "etc/apt/sources.list.d/target.sources".to_string(),
        sources.into_bytes(),
    );
    plan.add("status".to_string(), Vec::new());
    let conf = format!(
        "Dir \"{HERE}/\";\n\
         Dir::State::status \"{HERE}/status\";\n\
         APT::Architecture \"{arch}\";\n\
         APT::Architectures {{ \"{arch}\"; }};\n\
         Acquire::Languages \"none\";\n\
         Acquire::GzipIndexes \"true\";\n"
    );
    plan.add("apt.conf".to_string(), conf.into_bytes());
    plan
}

/// The versions of the package `apt-cache policy` describes, each with its
/// priority: the lines of its version table.
pub fn parse_policy(stdout: &str) -> Vec<(String, i32)> {
    let mut versions = Vec::new();
    let mut in_table = false;
    for line in stdout.lines() {
        let line = line.trim_start();
        if line == "Version table:" {
            in_table = true;
            continue;
        }
        if !in_table {
            continue;
        }
        let line = line.strip_prefix("*** ").unwrap_or(line);
        if let [version, priority] = line.split_whitespace().collect::<Vec<_>>()[..] {
            if let Ok(priority) = priority.parse() {
                versions.push((version.to_string(), priority));
            }
        }
    }
    versions
}

impl Scratch {
    /// Write `plan` under `cache`, in a directory named for its contents, so
    /// a release whose sources are unchanged reuses what apt fetched.
    pub fn write(cache: &Path, plan: &Plan) -> Result<Scratch, PortError> {
        let dir = cache.join("scratch").join(plan.key());
        let here = dir.to_string_lossy();
        for (rel, bytes) in &plan.files {
            let text = String::from_utf8_lossy(bytes);
            let bytes = if rel == "apt.conf" || rel.ends_with(".sources") {
                text.replace(HERE, &here).into_bytes()
            } else {
                bytes.clone()
            };
            write(&dir.join(rel), &bytes)?;
        }
        for d in [
            "var/lib/apt/lists/partial",
            "var/cache/apt/archives/partial",
        ] {
            fs::create_dir_all(dir.join(d)).map_err(|e| io_err(&dir, e))?;
        }
        Ok(Scratch { dir })
    }

    /// The scratch root's directory.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn command(&self, program: &str, args: &[&str]) -> Command {
        Command::new(program, args.iter().map(|a| a.to_string())).with_env(vec![
            (
                "APT_CONFIG".to_string(),
                self.dir.join("apt.conf").to_string_lossy().into_owned(),
            ),
            ("LC_ALL".to_string(), "C".to_string()),
        ])
    }

    fn stamp(&self) -> PathBuf {
        self.dir.join("updated")
    }

    /// `apt-get update` in the scratch root, unless it ran within
    /// [`RELEASE_TTL`] and `refetch` isn't set. When it fails after an
    /// earlier one succeeded, the lists that one fetched are used, with a
    /// warning. A missing `apt-get` is [`PortError::CommandNotFound`].
    pub fn update(&self, runner: &dyn CommandRunner, refetch: bool) -> Result<(), PortError> {
        let stamp = self.stamp();
        if !refetch && super::remote::fresh(&stamp, RELEASE_TTL) {
            debug!(message = logmsg::APT_SCRATCH_FRESH, dir = %self.dir.display());
            return Ok(());
        }
        let out = runner.run(&self.command("apt-get", &["-q", "update"]))?;
        if out.success() {
            return write(&stamp, b"");
        }
        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
        if stamp.exists() {
            warn!(message = logmsg::APT_SCRATCH_STALE, error = %stderr);
            return Ok(());
        }
        Err(PortError::Backend {
            detail: format!("{}: {stderr}", errmsg::APT_SCRATCH_UPDATE),
        })
    }

    /// `name`'s versions and their priorities, as `apt-cache policy` gives
    /// them in the scratch root.
    pub fn policy(
        &self,
        runner: &dyn CommandRunner,
        name: &str,
    ) -> Result<Vec<(String, i32)>, PortError> {
        let out = runner.run(&self.command("apt-cache", &["policy", name]))?;
        if !out.success() {
            return Err(PortError::CommandFailed {
                program: "apt-cache".to_string(),
                code: out.code,
                stderr: String::from_utf8_lossy(&out.stderr).trim().to_string(),
            });
        }
        Ok(parse_policy(&out.stdout_utf8()))
    }
}

#[cfg(test)]
mod tests;
