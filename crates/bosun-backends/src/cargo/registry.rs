//! Which index cargo installs crates.io crates from (§5.6): crates.io's own
//! sparse index, unless cargo's configuration replaces it.
//!
//! cargo has no stable command that prints its configuration (`cargo config
//! get` is nightly-only on bosun's toolchain), so the files cargo reads are
//! read as cargo reads them: `.cargo/config.toml` (or the older
//! `.cargo/config`, which cargo prefers when both exist) in the working
//! directory and each of its ancestors, then `$CARGO_HOME/config.toml`
//! (`~/.cargo` by default); a deeper file's value wins. `[source.crates-io]
//! replace-with` names the replacement, followed through further
//! `replace-with`s to a `[source.<name>] registry` or a `[registries.<name>]
//! index`. A `sparse+` index is read as crates.io's is; a git index, local
//! registry or vendored directory is not one bosun can read, and querying
//! says so instead of reading crates.io behind cargo's back.

use std::path::{Path, PathBuf};

/// crates.io's sparse index, cargo's default.
pub(crate) const CRATES_IO: &str = "https://index.crates.io";

/// crates.io's git index, which names crates.io itself.
const CRATES_IO_GIT: &str = "https://github.com/rust-lang/crates.io-index";

/// How many `replace-with`s are followed before giving up on a loop.
const MAX_REPLACEMENTS: usize = 16;

/// The config files cargo reads for a command run in `cwd`, most specific
/// first, each once.
pub(crate) fn config_files(cwd: &Path, cargo_home: &Path) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = cwd.ancestors().map(|d| d.join(".cargo")).collect();
    dirs.push(cargo_home.to_path_buf());
    let mut files: Vec<PathBuf> = Vec::new();
    for dir in dirs {
        let legacy = dir.join("config");
        let current = dir.join("config.toml");
        let file = if legacy.is_file() { legacy } else { current };
        if file.is_file() && !files.contains(&file) {
            files.push(file);
        }
    }
    files
}

/// The value at `path` in the most specific table that has one.
fn lookup<'a>(tables: &'a [toml::Table], path: &[&str]) -> Option<&'a toml::Value> {
    tables.iter().find_map(|t| {
        let (first, rest) = path.split_first()?;
        rest.iter()
            .try_fold(t.get(*first)?, |v, key| v.as_table()?.get(*key))
    })
}

fn string<'a>(tables: &'a [toml::Table], path: &[&str]) -> Option<&'a str> {
    lookup(tables, path).and_then(toml::Value::as_str)
}

/// The sparse index URL cargo installs crates.io crates from, given its
/// config `tables` (most specific first), or why bosun cannot read it.
pub(crate) fn crates_io_index(tables: &[toml::Table]) -> Result<String, String> {
    let mut name = "crates-io".to_string();
    for _ in 0..MAX_REPLACEMENTS {
        if let Some(next) = string(tables, &["source", &name, "replace-with"]) {
            name = next.to_string();
            continue;
        }
        if name == "crates-io" {
            return Ok(CRATES_IO.to_string());
        }
        let index = string(tables, &["source", &name, "registry"])
            .or_else(|| string(tables, &["registries", &name, "index"]));
        if let Some(index) = index {
            return sparse(index).ok_or_else(|| format!("`{name}` is the git index {index}"));
        }
        for kind in ["local-registry", "directory", "git"] {
            if lookup(tables, &["source", &name, kind]).is_some() {
                return Err(format!("`{name}` is a {kind} source"));
            }
        }
        return Err(format!("`{name}` is defined in no cargo config"));
    }
    Err(format!("`{name}` replaces itself"))
}

/// The base URL of a sparse index, or `None` for a git one; crates.io's git
/// index is crates.io.
fn sparse(index: &str) -> Option<String> {
    if let Some(url) = index.strip_prefix("sparse+") {
        return Some(url.trim_end_matches('/').to_string());
    }
    (index.trim_end_matches('/') == CRATES_IO_GIT).then(|| CRATES_IO.to_string())
}

/// Read and parse `files`, in order; a file that does not parse is an error
/// naming it.
pub(crate) fn read(files: &[PathBuf]) -> Result<Vec<toml::Table>, String> {
    files
        .iter()
        .map(|f| {
            let text = std::fs::read_to_string(f).map_err(|e| format!("{}: {e}", f.display()))?;
            text.parse::<toml::Table>()
                .map_err(|e| format!("{}: {e}", f.display()))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tables(texts: &[&str]) -> Vec<toml::Table> {
        texts.iter().map(|t| t.parse().unwrap()).collect()
    }

    #[test]
    fn with_no_replacement_cargo_reads_crates_io() {
        assert_eq!(crates_io_index(&[]), Ok(CRATES_IO.to_string()));
        assert_eq!(
            crates_io_index(&tables(&["[build]\njobs = 4\n"])),
            Ok(CRATES_IO.to_string())
        );
    }

    #[test]
    fn a_sparse_mirror_replaces_crates_io() {
        let t = tables(&[r#"
            [source.crates-io]
            replace-with = "mirror"
            [source.mirror]
            registry = "sparse+https://mirror.example/index/"
        "#]);
        assert_eq!(
            crates_io_index(&t),
            Ok("https://mirror.example/index".to_string())
        );
    }

    #[test]
    fn a_replacement_may_name_a_registry_or_chain() {
        let t = tables(&[r#"
            [source.crates-io]
            replace-with = "corp"
            [source.corp]
            replace-with = "artifactory"
            [registries.artifactory]
            index = "sparse+https://art.example/api/cargo/crates/index/"
        "#]);
        assert_eq!(
            crates_io_index(&t),
            Ok("https://art.example/api/cargo/crates/index".to_string())
        );
    }

    #[test]
    fn the_most_specific_file_wins() {
        let project = r#"
            [source.crates-io]
            replace-with = "near"
            [source.near]
            registry = "sparse+https://near.example/"
        "#;
        let home = r#"
            [source.crates-io]
            replace-with = "far"
            [source.far]
            registry = "sparse+https://far.example/"
            [source.near]
            registry = "sparse+https://shadowed.example/"
        "#;
        assert_eq!(
            crates_io_index(&tables(&[project, home])),
            Ok("https://near.example".to_string())
        );
        // A key only the home file sets still counts.
        let project = "[source.crates-io]\nreplace-with = \"far\"\n";
        assert_eq!(
            crates_io_index(&tables(&[project, home])),
            Ok("https://far.example".to_string())
        );
    }

    #[test]
    fn what_bosun_cannot_read_is_named() {
        let git = tables(&[r#"
            [source.crates-io]
            replace-with = "g"
            [source.g]
            registry = "https://git.example/index.git"
        "#]);
        assert!(crates_io_index(&git).unwrap_err().contains("git index"));
        for kind in ["local-registry", "directory"] {
            let t = tables(&[&format!(
                "[source.crates-io]\nreplace-with = \"v\"\n[source.v]\n{kind} = \"/srv/v\"\n"
            )]);
            assert!(crates_io_index(&t).unwrap_err().contains(kind), "{kind}");
        }
        let t = tables(&[
            "[source.crates-io]\nreplace-with = \"g\"\n[source.g]\ngit = \"https://x\"\n",
        ]);
        assert!(crates_io_index(&t).unwrap_err().contains("git source"));
        let missing = tables(&["[source.crates-io]\nreplace-with = \"nowhere\"\n"]);
        assert!(crates_io_index(&missing).unwrap_err().contains("nowhere"));
        let looped = tables(&[
            "[source.crates-io]\nreplace-with = \"a\"\n[source.a]\nreplace-with = \"crates-io\"\n",
        ]);
        assert!(crates_io_index(&looped)
            .unwrap_err()
            .contains("replaces itself"));
    }

    #[test]
    fn crates_ios_git_index_is_crates_io() {
        let t = tables(&[r#"
            [source.crates-io]
            replace-with = "upstream"
            [source.upstream]
            registry = "https://github.com/rust-lang/crates.io-index"
        "#]);
        assert_eq!(crates_io_index(&t), Ok(CRATES_IO.to_string()));
    }

    #[test]
    fn config_files_are_the_working_directorys_ancestors_then_cargo_home() {
        let root = tempfile::tempdir().unwrap();
        let project = root.path().join("w").join("p");
        let home = root.path().join("cargo-home");
        for dir in [
            project.join(".cargo"),
            root.path().join("w/.cargo"),
            home.clone(),
        ] {
            std::fs::create_dir_all(&dir).unwrap();
        }
        std::fs::write(project.join(".cargo/config.toml"), "").unwrap();
        // The extensionless name wins where both exist.
        std::fs::write(root.path().join("w/.cargo/config"), "").unwrap();
        std::fs::write(root.path().join("w/.cargo/config.toml"), "").unwrap();
        std::fs::write(home.join("config.toml"), "").unwrap();
        assert_eq!(
            config_files(&project, &home),
            vec![
                project.join(".cargo/config.toml"),
                root.path().join("w/.cargo/config"),
                home.join("config.toml"),
            ]
        );
        // CARGO_HOME as an ancestor's .cargo is read once.
        assert_eq!(
            config_files(&project, &project.join(".cargo")),
            vec![
                project.join(".cargo/config.toml"),
                root.path().join("w/.cargo/config"),
            ]
        );
    }

    #[test]
    fn read_parses_each_file_and_names_one_that_does_not_parse() {
        let dir = tempfile::tempdir().unwrap();
        let good = dir.path().join("good.toml");
        let bad = dir.path().join("bad.toml");
        std::fs::write(&good, "[source.crates-io]\nreplace-with = \"m\"\n").unwrap();
        std::fs::write(&bad, "[source\n").unwrap();
        assert_eq!(read(std::slice::from_ref(&good)).unwrap().len(), 1);
        assert!(read(&[good, bad.clone()]).unwrap_err().contains("bad.toml"));
        assert!(read(&[dir.path().join("gone.toml")]).is_err());
    }
}
