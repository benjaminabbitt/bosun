//! The host step executor (§5.1): carries out the [`Step`] list an installer
//! produced. `Run` goes through the [`CommandRunner`] (a non-zero exit is a
//! failure); `Fetch` downloads through the [`HttpFetcher`] and verifies the
//! sha256; `Extract` pulls one member out of a `.tar.gz`. The container
//! renderer (§8) consumes the same steps instead of running them.

use std::fs;
use std::io::{Read, Write};
use std::path::Path;

use bosun_ports::{Command, CommandRunner, HttpFetcher, HttpRequest, PortError, Step};
use sha2::{Digest, Sha256};

/// Runs step lists on the local host.
pub struct StepExecutor<'a> {
    runner: &'a dyn CommandRunner,
    fetcher: &'a dyn HttpFetcher,
}

impl<'a> StepExecutor<'a> {
    /// Build an executor over the given runner and HTTP fetcher.
    pub fn new(runner: &'a dyn CommandRunner, fetcher: &'a dyn HttpFetcher) -> StepExecutor<'a> {
        StepExecutor { runner, fetcher }
    }

    /// Execute each step in order, stopping at the first failure.
    pub fn execute(&self, steps: &[Step]) -> Result<(), PortError> {
        for step in steps {
            self.one(step)?;
        }
        Ok(())
    }

    fn one(&self, step: &Step) -> Result<(), PortError> {
        match step {
            Step::Run {
                argv,
                env,
                scope: _,
            } => {
                let (program, args) = argv.split_first().ok_or_else(|| PortError::Backend {
                    detail: "empty argv".to_string(),
                })?;
                let command = Command {
                    program: program.clone(),
                    args: args.to_vec(),
                    env: env.clone(),
                };
                let out = self.runner.run(&command)?;
                if out.success() {
                    Ok(())
                } else {
                    Err(PortError::CommandFailed {
                        program: program.clone(),
                        code: out.code,
                        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
                    })
                }
            }
            Step::Fetch { url, sha256, dest } => {
                let resp = self.fetcher.fetch(&HttpRequest::get(url))?;
                if !(200..300).contains(&resp.status) {
                    return Err(PortError::HttpStatus {
                        url: url.clone(),
                        status: resp.status,
                    });
                }
                let got = hex(&Sha256::digest(&resp.body));
                if got != *sha256 {
                    return Err(PortError::Backend {
                        detail: format!(
                            "checksum mismatch for {url}: expected {sha256}, got {got}"
                        ),
                    });
                }
                write_file(dest, &resp.body, None)
            }
            Step::Extract {
                archive,
                member,
                dest,
                mode,
            } => extract_member(archive, member, dest, *mode),
        }
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Atomically write `bytes` to `dest`: a temp file in dest's own directory,
/// fsync'd, given `mode` (if any) **before** the rename over `dest`. Rename —
/// not an in-place write — avoids `ETXTBSY` when replacing a running
/// executable (bosun replacing itself, or ctxloom while it runs), is
/// crash-safe, and leaves no temp behind on failure (the temp is removed when
/// the `NamedTempFile` drops).
fn write_file(dest: &Path, bytes: &[u8], mode: Option<u32>) -> Result<(), PortError> {
    let dir = dest.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(dir).map_err(|e| io_err(dir, e))?;
    let mut tmp = tempfile::NamedTempFile::new_in(dir).map_err(|e| io_err(dir, e))?;
    tmp.write_all(bytes).map_err(|e| io_err(tmp.path(), e))?;
    tmp.as_file()
        .sync_all()
        .map_err(|e| io_err(tmp.path(), e))?;
    if let Some(mode) = mode {
        set_mode(tmp.as_file(), mode)?;
    }
    // On a persist failure the returned NamedTempFile drops, cleaning the temp.
    tmp.persist(dest).map_err(|e| io_err(dest, e.error))?;
    Ok(())
}

/// Extract one **regular-file** `.tar.gz` member to `dest` with unix mode
/// `mode`. A leading `./` on either side is ignored (goreleaser archives store
/// members both ways). A symlink/hardlink/dir member is refused, naming its
/// type, rather than silently written as a 0-byte executable.
fn extract_member(archive: &Path, member: &str, dest: &Path, mode: u32) -> Result<(), PortError> {
    let file = fs::File::open(archive).map_err(|e| io_err(archive, e))?;
    let mut tar = tar::Archive::new(flate2::read::GzDecoder::new(file));
    for entry in tar.entries().map_err(|e| io_err(archive, e))? {
        let mut entry = entry.map_err(|e| io_err(archive, e))?;
        let path = entry.path().map_err(|e| io_err(archive, e))?;
        if norm(&path.to_string_lossy()) != norm(member) {
            continue;
        }
        let kind = entry.header().entry_type();
        if !kind.is_file() {
            return Err(PortError::Backend {
                detail: format!(
                    "{member} in {} is not a regular file ({kind:?})",
                    archive.display()
                ),
            });
        }
        let mut bytes = Vec::new();
        entry
            .read_to_end(&mut bytes)
            .map_err(|e| io_err(archive, e))?;
        return write_file(dest, &bytes, Some(mode));
    }
    Err(PortError::Backend {
        detail: format!("{member} not found in {}", archive.display()),
    })
}

/// Strip a single leading `./` for member comparison.
fn norm(p: &str) -> &str {
    p.strip_prefix("./").unwrap_or(p)
}

#[cfg(unix)]
fn set_mode(file: &fs::File, mode: u32) -> Result<(), PortError> {
    use std::os::unix::fs::PermissionsExt;
    file.set_permissions(fs::Permissions::from_mode(mode))
        .map_err(|e| PortError::Backend {
            detail: format!("set mode {mode:o}: {e}"),
        })
}

#[cfg(not(unix))]
fn set_mode(_file: &fs::File, _mode: u32) -> Result<(), PortError> {
    Ok(())
}

fn io_err(path: &Path, e: std::io::Error) -> PortError {
    PortError::Backend {
        detail: format!("{}: {e}", path.display()),
    }
}

impl bosun_ports::StepRunner for StepExecutor<'_> {
    fn execute(&self, steps: &[Step]) -> Result<(), PortError> {
        // Delegate to the inherent method (unambiguous by type path).
        StepExecutor::execute(self, steps)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bosun_core::Scope;
    use bosun_ports::testing::{FakeCommandRunner, FakeHttpFetcher};
    use bosun_ports::{HttpResponse, Output};
    use flate2::write::GzEncoder;
    use flate2::Compression;

    fn exec_with(
        runner: &FakeCommandRunner,
        fetcher: &FakeHttpFetcher,
        steps: &[Step],
    ) -> Result<(), PortError> {
        StepExecutor::new(runner, fetcher).execute(steps)
    }

    #[test]
    fn hex_encodes_bytes_lowercase() {
        // Direct, so a mutation of `hex` is caught (the Fetch test computes its
        // expected sha with `hex`, which would be tautological on its own).
        assert_eq!(hex(&[0x00, 0xff, 0x2f, 0x0a]), "00ff2f0a");
    }

    #[test]
    fn run_passes_argv_and_env_to_the_runner() {
        let runner = FakeCommandRunner::new().with_stdout("");
        let fetcher = FakeHttpFetcher::new();
        let steps = vec![Step::Run {
            argv: vec![
                "apt-get".into(),
                "install".into(),
                "-y".into(),
                "jq=1.7.1".into(),
            ],
            env: vec![("DEBIAN_FRONTEND".into(), "noninteractive".into())],
            scope: Scope::System,
        }];
        exec_with(&runner, &fetcher, &steps).unwrap();
        let calls = runner.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].program, "apt-get");
        assert_eq!(calls[0].args, vec!["install", "-y", "jq=1.7.1"]);
        assert_eq!(
            calls[0].env,
            vec![("DEBIAN_FRONTEND".to_string(), "noninteractive".to_string())]
        );
    }

    #[test]
    fn run_nonzero_exit_is_a_failure() {
        let runner = FakeCommandRunner::new().with_output(Output {
            code: Some(100),
            stdout: vec![],
            stderr: b"E: boom".to_vec(),
        });
        let fetcher = FakeHttpFetcher::new();
        let steps = vec![Step::Run {
            argv: vec!["apt-get".into()],
            env: vec![],
            scope: Scope::System,
        }];
        assert!(matches!(
            exec_with(&runner, &fetcher, &steps),
            Err(PortError::CommandFailed {
                code: Some(100),
                ..
            })
        ));
    }

    #[test]
    fn fetch_verifies_checksum_and_writes() {
        let body = b"hello bosun";
        let sha = hex(&Sha256::digest(body));
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("sub/out.bin");
        let runner = FakeCommandRunner::new();
        let fetcher = FakeHttpFetcher::new().with_body("https://x/a", body.to_vec());

        exec_with(
            &runner,
            &fetcher,
            &[Step::Fetch {
                url: "https://x/a".into(),
                sha256: sha,
                dest: dest.clone(),
            }],
        )
        .unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), body);
    }

    #[test]
    fn fetch_rejects_a_checksum_mismatch() {
        let fetcher = FakeHttpFetcher::new().with_body("https://x/a", b"actual".to_vec());
        let runner = FakeCommandRunner::new();
        let err = exec_with(
            &runner,
            &fetcher,
            &[Step::Fetch {
                url: "https://x/a".into(),
                sha256: "00".repeat(32),
                dest: std::env::temp_dir().join("bosun-never"),
            }],
        )
        .unwrap_err();
        assert!(
            matches!(err, PortError::Backend { detail } if detail.contains("checksum mismatch"))
        );
    }

    #[test]
    fn fetch_rejects_error_status() {
        let fetcher = FakeHttpFetcher::new().with_response(
            "https://x/a",
            HttpResponse {
                status: 404,
                body: vec![],
            },
        );
        let runner = FakeCommandRunner::new();
        assert!(matches!(
            exec_with(
                &runner,
                &fetcher,
                &[Step::Fetch {
                    url: "https://x/a".into(),
                    sha256: "x".into(),
                    dest: std::env::temp_dir().join("bosun-never"),
                }]
            ),
            Err(PortError::HttpStatus { status: 404, .. })
        ));
    }

    #[test]
    fn extract_pulls_a_member_and_sets_mode() {
        let dir = tempfile::tempdir().unwrap();
        let archive = dir.path().join("rel.tar.gz");
        // Build a .tar.gz containing `bin/jq`.
        {
            let f = std::fs::File::create(&archive).unwrap();
            let mut tar = tar::Builder::new(GzEncoder::new(f, Compression::default()));
            let content = b"#!/bin/sh\necho jq\n";
            let mut header = tar::Header::new_gnu();
            header.set_size(content.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            tar.append_data(&mut header, "bin/jq", &content[..])
                .unwrap();
            tar.into_inner().unwrap().finish().unwrap();
        }
        let dest = dir.path().join("jq");
        let runner = FakeCommandRunner::new();
        let fetcher = FakeHttpFetcher::new();
        exec_with(
            &runner,
            &fetcher,
            &[Step::Extract {
                archive: archive.clone(),
                member: "bin/jq".into(),
                dest: dest.clone(),
                mode: 0o755,
            }],
        )
        .unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), b"#!/bin/sh\necho jq\n");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&dest).unwrap().permissions().mode() & 0o777,
                0o755
            );
        }
    }

    #[test]
    fn extract_errors_when_member_absent() {
        let dir = tempfile::tempdir().unwrap();
        let archive = dir.path().join("empty.tar.gz");
        {
            let f = std::fs::File::create(&archive).unwrap();
            let tar = tar::Builder::new(GzEncoder::new(f, Compression::default()));
            tar.into_inner().unwrap().finish().unwrap();
        }
        let runner = FakeCommandRunner::new();
        let fetcher = FakeHttpFetcher::new();
        assert!(exec_with(
            &runner,
            &fetcher,
            &[Step::Extract {
                archive,
                member: "bin/jq".into(),
                dest: dir.path().join("jq"),
                mode: 0o755,
            }]
        )
        .is_err());
    }

    fn targz(path: &Path, entries: &[(&str, &[u8])]) {
        let f = std::fs::File::create(path).unwrap();
        let mut tar = tar::Builder::new(GzEncoder::new(f, Compression::default()));
        for (name, content) in entries {
            let mut h = tar::Header::new_gnu();
            h.set_size(content.len() as u64);
            h.set_mode(0o644);
            h.set_cksum();
            tar.append_data(&mut h, name, *content).unwrap();
        }
        tar.into_inner().unwrap().finish().unwrap();
    }

    fn list(dir: &Path) -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        v.sort();
        v
    }

    fn extract(archive: &Path, member: &str, dest: &Path) -> Result<(), PortError> {
        exec_with(
            &FakeCommandRunner::new(),
            &FakeHttpFetcher::new(),
            &[Step::Extract {
                archive: archive.to_path_buf(),
                member: member.to_string(),
                dest: dest.to_path_buf(),
                mode: 0o755,
            }],
        )
    }

    #[test]
    fn extract_replaces_a_held_open_file_atomically() {
        let dir = tempfile::tempdir().unwrap();
        let archive = dir.path().join("r.tar.gz");
        targz(&archive, &[("bin/jq", b"NEW")]);
        let dest = dir.path().join("jq");
        std::fs::write(&dest, b"OLD").unwrap();
        // Hold the old inode open, as a running executable would be.
        let mut held = std::fs::File::open(&dest).unwrap();

        extract(&archive, "bin/jq", &dest).unwrap();

        assert_eq!(std::fs::read(&dest).unwrap(), b"NEW"); // new content via a new inode
        let mut old = String::new();
        held.read_to_string(&mut old).unwrap();
        assert_eq!(old, "OLD"); // the held handle still sees the old inode (rename, not in-place)
    }

    #[test]
    fn write_leaves_no_temp_behind_on_failure() {
        let dir = tempfile::tempdir().unwrap();
        let archive = dir.path().join("r.tar.gz");
        targz(&archive, &[("bin/jq", b"NEW")]);
        // dest is a directory, so the final rename fails *after* the temp is
        // written — exercising cleanup.
        let dest = dir.path().join("occupied");
        std::fs::create_dir(&dest).unwrap();
        let before = list(dir.path());

        assert!(extract(&archive, "bin/jq", &dest).is_err());
        assert_eq!(list(dir.path()), before, "a temp file survived the failure");
    }

    #[test]
    fn extract_rejects_a_symlink_member() {
        let dir = tempfile::tempdir().unwrap();
        let archive = dir.path().join("r.tar.gz");
        {
            let f = std::fs::File::create(&archive).unwrap();
            let mut tar = tar::Builder::new(GzEncoder::new(f, Compression::default()));
            let mut h = tar::Header::new_gnu();
            h.set_entry_type(tar::EntryType::Symlink);
            h.set_size(0);
            tar.append_link(&mut h, "bin/jq", "/somewhere/else")
                .unwrap();
            tar.into_inner().unwrap().finish().unwrap();
        }
        let err = extract(&archive, "bin/jq", &dir.path().join("jq")).unwrap_err();
        assert!(
            matches!(err, PortError::Backend { detail } if detail.contains("not a regular file")),
            "a symlink member must be refused"
        );
    }

    #[test]
    fn extract_matches_across_a_dot_slash_prefix() {
        let dir = tempfile::tempdir().unwrap();
        let archive = dir.path().join("r.tar.gz");
        targz(&archive, &[("./bin/jq", b"X")]); // stored with a leading ./
        let dest = dir.path().join("jq");
        extract(&archive, "bin/jq", &dest).unwrap(); // requested without ./
        assert_eq!(std::fs::read(&dest).unwrap(), b"X");
    }
}
