//! The host step executor (§5.1): carries out the [`Step`] list an installer
//! produced. `Run` goes through the [`CommandRunner`] (a non-zero exit is a
//! failure); `Fetch` downloads through the [`HttpFetcher`] and verifies the
//! sha256 when the step carries one; `Extract` pulls the one member with a
//! given file name out of a tar (plain, gz, xz, zstd or bzip2) or zip archive,
//! wherever in the archive it sits, decompresses a single compressed file, or
//! copies a raw binary; `Remove` deletes a file. The container renderer (§8)
//! consumes the same steps instead of running them.

use std::fs;
use std::io::{Read, Write};
use std::path::Path;

use bosun_ports::{
    ArchiveFormat, Command, CommandRunner, HttpFetcher, HttpRequest, PortError, Step,
};
use sha2::{Digest, Sha256};

use crate::errmsg;

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
                if let Some(sha256) = sha256.as_ref().filter(|want| got != **want) {
                    return Err(PortError::Backend {
                        detail: format!(
                            "checksum mismatch for {url}: expected {sha256}, got {got}"
                        ),
                    });
                }
                write_file(dest, &resp.body, None)
            }
            Step::Extract {
                format,
                archive,
                member,
                dest,
                mode,
            } => extract(*format, archive, member, dest, *mode),
            Step::Remove { path } => match fs::remove_file(path) {
                Ok(()) => Ok(()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(e) => Err(io_err(path, e)),
            },
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

/// The compression around an asset's contents.
#[derive(Clone, Copy)]
enum Compression {
    None,
    Gz,
    Xz,
    Zst,
    Bz2,
}

/// Write the binary `format` packs in `archive` to `dest` with unix mode
/// `mode`: the member named `member` of a tar or zip, the whole of a single
/// compressed file, or a raw binary as it is.
fn extract(
    format: ArchiveFormat,
    archive: &Path,
    member: &str,
    dest: &Path,
    mode: u32,
) -> Result<(), PortError> {
    let single = |c| {
        let mut bytes = Vec::new();
        decoder(c, archive)?
            .read_to_end(&mut bytes)
            .map_err(|e| io_err(archive, e))?;
        write_file(dest, &bytes, Some(mode))
    };
    match format {
        ArchiveFormat::Raw => single(Compression::None),
        ArchiveFormat::Gz => single(Compression::Gz),
        ArchiveFormat::Xz => single(Compression::Xz),
        ArchiveFormat::Zst => single(Compression::Zst),
        ArchiveFormat::Bz2 => single(Compression::Bz2),
        ArchiveFormat::Zip => extract_zip_member(archive, member, dest, mode),
        ArchiveFormat::Tar => extract_tar_member(Compression::None, archive, member, dest, mode),
        ArchiveFormat::TarGz => extract_tar_member(Compression::Gz, archive, member, dest, mode),
        ArchiveFormat::TarXz => extract_tar_member(Compression::Xz, archive, member, dest, mode),
        ArchiveFormat::TarZst => extract_tar_member(Compression::Zst, archive, member, dest, mode),
        ArchiveFormat::TarBz2 => extract_tar_member(Compression::Bz2, archive, member, dest, mode),
    }
}

/// `archive`'s contents, decompressed as `c` says. xz is decoded whole, the
/// others as a stream.
fn decoder(c: Compression, archive: &Path) -> Result<Box<dyn Read>, PortError> {
    let file = fs::File::open(archive).map_err(|e| io_err(archive, e))?;
    Ok(match c {
        Compression::None => Box::new(file),
        Compression::Gz => Box::new(flate2::read::GzDecoder::new(file)),
        Compression::Bz2 => Box::new(bzip2::read::BzDecoder::new(file)),
        Compression::Zst => {
            Box::new(ruzstd::decoding::StreamingDecoder::new(file).map_err(|e| {
                PortError::Backend {
                    detail: format!("{}: {e}", archive.display()),
                }
            })?)
        }
        Compression::Xz => {
            let mut out = Vec::new();
            lzma_rs::xz_decompress(&mut std::io::BufReader::new(file), &mut out).map_err(|e| {
                PortError::Backend {
                    detail: format!("{}: {e}", archive.display()),
                }
            })?;
            Box::new(std::io::Cursor::new(out))
        }
    })
}

/// Extract the tar member whose **file name** is `member`, at any depth
/// (`gum`, `gum_2.0.2_Linux_x86_64/gum`, `gh_2.80.0/bin/gh` alike), to `dest`
/// with unix mode `mode`. Exactly one non-directory entry may carry that name;
/// none or several is an error listing what was found. A symlink/hardlink
/// member is refused, naming its type, rather than silently written as a
/// 0-byte executable.
fn extract_tar_member(
    c: Compression,
    archive: &Path,
    member: &str,
    dest: &Path,
    mode: u32,
) -> Result<(), PortError> {
    // Pass 1: the non-directory entries, in archive order.
    let entries = entry_names(c, archive)?;
    let index = locate_member(member, &entries, archive)?;

    // Pass 2: read that entry (directories are skipped the same way).
    let mut tar = tar::Archive::new(decoder(c, archive)?);
    let mut files = tar
        .entries()
        .map_err(|e| io_err(archive, e))?
        .filter(|entry| {
            entry
                .as_ref()
                .map_or(true, |e| !e.header().entry_type().is_dir())
        });
    let mut entry = files
        .nth(index)
        .ok_or_else(|| PortError::Backend {
            detail: format!("{} changed while reading", archive.display()),
        })?
        .map_err(|e| io_err(archive, e))?;
    let kind = entry.header().entry_type();
    if !kind.is_file() {
        return Err(not_a_file(&entries[index], archive, &format!("{kind:?}")));
    }
    let mut bytes = Vec::new();
    entry
        .read_to_end(&mut bytes)
        .map_err(|e| io_err(archive, e))?;
    write_file(dest, &bytes, Some(mode))
}

/// The paths of every non-directory entry in a tar, in archive order.
fn entry_names(c: Compression, archive: &Path) -> Result<Vec<String>, PortError> {
    let mut tar = tar::Archive::new(decoder(c, archive)?);
    let mut names = Vec::new();
    for entry in tar.entries().map_err(|e| io_err(archive, e))? {
        let entry = entry.map_err(|e| io_err(archive, e))?;
        if entry.header().entry_type().is_dir() {
            continue;
        }
        let path = entry.path().map_err(|e| io_err(archive, e))?;
        names.push(path.to_string_lossy().into_owned());
    }
    Ok(names)
}

/// The zip counterpart of [`extract_tar_member`]: the one non-directory entry
/// whose file name is `member`, refusing a symlink.
fn extract_zip_member(
    archive: &Path,
    member: &str,
    dest: &Path,
    mode: u32,
) -> Result<(), PortError> {
    let file = fs::File::open(archive).map_err(|e| io_err(archive, e))?;
    let zip_err = |e: zip::result::ZipError| PortError::Backend {
        detail: format!("{}: {e}", archive.display()),
    };
    let mut zip = zip::ZipArchive::new(file).map_err(zip_err)?;
    let mut entries = Vec::new();
    for i in 0..zip.len() {
        let entry = zip.by_index_raw(i).map_err(zip_err)?;
        if !entry.is_dir() {
            entries.push((i, entry.name().to_string()));
        }
    }
    let names: Vec<String> = entries.iter().map(|(_, n)| n.clone()).collect();
    let index = locate_member(member, &names, archive)?;
    let mut entry = zip.by_index(entries[index].0).map_err(zip_err)?;
    // zip's `is_file` is false for a directory and for a symlink.
    if !entry.is_file() {
        return Err(not_a_file(&names[index], archive, "Symlink"));
    }
    let mut bytes = Vec::new();
    entry
        .read_to_end(&mut bytes)
        .map_err(|e| io_err(archive, e))?;
    write_file(dest, &bytes, Some(mode))
}

fn not_a_file(entry: &str, archive: &Path, kind: &str) -> PortError {
    PortError::Backend {
        detail: format!(
            "{entry} in {} is not a regular file ({kind})",
            archive.display()
        ),
    }
}

/// The index in `entries` of the one entry whose file name (last path
/// component) is `name`. None or several is an error: a missing name lists the
/// archive's entries, an ambiguous one lists the matches.
fn locate_member(name: &str, entries: &[String], archive: &Path) -> Result<usize, PortError> {
    let matches: Vec<usize> = entries
        .iter()
        .enumerate()
        .filter(|(_, path)| file_name(path) == name)
        .map(|(i, _)| i)
        .collect();
    match matches.as_slice() {
        [one] => Ok(*one),
        [] => Err(PortError::Backend {
            detail: format!(
                "{} {name:?} ({}); found: {entries:?}",
                errmsg::MEMBER_NOT_FOUND,
                archive.display()
            ),
        }),
        several => {
            let found: Vec<&str> = several.iter().map(|&i| entries[i].as_str()).collect();
            Err(PortError::Backend {
                detail: format!(
                    "{} {name:?} ({}); found: {found:?}",
                    errmsg::MEMBER_AMBIGUOUS,
                    archive.display()
                ),
            })
        }
    }
}

/// The last component of an archive path, ignoring a leading `./`.
fn file_name(path: &str) -> &str {
    let path = norm(path);
    path.rsplit('/').next().unwrap_or(path)
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
    use bosun_ports::{ArchiveFormat, HttpResponse, Output};
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
                sha256: Some(sha),
                dest: dest.clone(),
            }],
        )
        .unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), body);
    }

    #[test]
    fn fetch_without_a_checksum_writes_unverified() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("out.bin");
        let fetcher = FakeHttpFetcher::new().with_body("https://x/a", b"unsigned".to_vec());
        exec_with(
            &FakeCommandRunner::new(),
            &fetcher,
            &[Step::Fetch {
                url: "https://x/a".into(),
                sha256: None,
                dest: dest.clone(),
            }],
        )
        .unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), b"unsigned");
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
                sha256: Some("00".repeat(32)),
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
                    sha256: Some("x".into()),
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
                format: ArchiveFormat::TarGz,
                archive: archive.clone(),
                member: "jq".into(),
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
                format: ArchiveFormat::TarGz,
                archive,
                member: "jq".into(),
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
                format: ArchiveFormat::of(&archive.to_string_lossy()).expect("a binary asset"),
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

        extract(&archive, "jq", &dest).unwrap();

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

        assert!(extract(&archive, "jq", &dest).is_err());
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
        let err = extract(&archive, "jq", &dir.path().join("jq")).unwrap_err();
        assert!(
            matches!(err, PortError::Backend { detail } if detail.contains("not a regular file")),
            "a symlink member must be refused"
        );
    }

    #[test]
    fn extract_matches_across_a_dot_slash_prefix() {
        let dir = tempfile::tempdir().unwrap();
        let archive = dir.path().join("r.tar.gz");
        // A decoy first entry whose name differs only once normalisation is
        // applied: a `norm` that collapses every name to a constant would
        // wrongly match this one, so the extracted bytes pin that `norm`
        // actually strips `./` on both sides rather than returning a constant.
        targz(&archive, &[("README", b"DECOY"), ("./jq", b"X")]);
        let dest = dir.path().join("jq");
        extract(&archive, "jq", &dest).unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), b"X");
    }

    #[test]
    fn execute_through_the_steprunner_trait_drives_the_runner() {
        // The inherent `execute` is covered above; this pins the trait-object
        // dispatch (the port the applier actually calls) delegates to it.
        let runner = FakeCommandRunner::new().with_stdout("");
        let fetcher = FakeHttpFetcher::new();
        let exec = StepExecutor::new(&runner, &fetcher);
        let runner_port: &dyn bosun_ports::StepRunner = &exec;
        runner_port
            .execute(&[Step::Run {
                argv: vec!["apt-get".into(), "update".into()],
                env: vec![],
                scope: Scope::System,
            }])
            .unwrap();
        assert_eq!(runner.calls().len(), 1);
        assert_eq!(runner.calls()[0].program, "apt-get");
    }

    // --- locating the member by name -----------------------------------------

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    fn locate_err(name: &str, list: &[&str]) -> String {
        match locate_member(name, &names(list), Path::new("a.tar.gz")).unwrap_err() {
            PortError::Backend { detail } => detail,
            other => panic!("unexpected error {other:?}"),
        }
    }

    #[test]
    fn locate_finds_the_binary_at_the_root() {
        // duf, lazygit, k9s, just: the binary sits next to LICENSE and README.
        let entries = names(&["LICENSE", "README.md", "duf"]);
        assert_eq!(locate_member("duf", &entries, Path::new("a")).unwrap(), 2);
        // goreleaser sometimes writes `./` on member names.
        let dotted = names(&["./LICENSE", "./duf"]);
        assert_eq!(locate_member("duf", &dotted, Path::new("a")).unwrap(), 1);
    }

    #[test]
    fn locate_finds_the_binary_in_a_wrapping_directory() {
        let entries = names(&[
            "gum_2.0.2_Linux_x86_64/LICENSE",
            "gum_2.0.2_Linux_x86_64/completions/gum.bash",
            "gum_2.0.2_Linux_x86_64/gum",
            "gum_2.0.2_Linux_x86_64/manpages/gum.1.gz",
        ]);
        assert_eq!(locate_member("gum", &entries, Path::new("a")).unwrap(), 2);
    }

    #[test]
    fn locate_finds_the_binary_under_a_nested_bin() {
        let entries = names(&[
            "gh_2.80.0_linux_amd64/LICENSE",
            "gh_2.80.0_linux_amd64/bin/gh",
            "gh_2.80.0_linux_amd64/share/man/man1/gh.1",
        ]);
        assert_eq!(locate_member("gh", &entries, Path::new("a")).unwrap(), 1);
    }

    #[test]
    fn locate_errors_when_missing_and_lists_the_entries() {
        let e = locate_err("gum", &["gum_2.0.2/LICENSE", "gum_2.0.2/gumx"]);
        assert!(e.contains(errmsg::MEMBER_NOT_FOUND), "{e}");
        assert!(e.contains("gum_2.0.2/gumx"), "{e}");
        assert!(e.contains("a.tar.gz"), "{e}");
    }

    #[test]
    fn locate_errors_on_duplicates_and_lists_them() {
        let e = locate_err("uv", &["uv-x/uv", "uv-x/uvx", "uv-x/bin/uv"]);
        assert!(e.contains(errmsg::MEMBER_AMBIGUOUS), "{e}");
        assert!(e.contains("uv-x/uv") && e.contains("uv-x/bin/uv"), "{e}");
        assert!(!e.contains("uvx"), "only the matches are listed: {e}");
    }

    #[test]
    fn locate_never_matches_a_path_or_a_prefix() {
        // A name, not a path: `bin/gh` names nothing; `gh` is not `gh.1`.
        assert!(locate_member("bin/gh", &names(&["x/bin/gh"]), Path::new("a")).is_err());
        assert!(locate_member("gh", &names(&["gh.1", "ghx"]), Path::new("a")).is_err());
    }

    #[test]
    fn extract_pulls_the_binary_out_of_a_wrapping_directory() {
        let dir = tempfile::tempdir().unwrap();
        let archive = dir.path().join("gum.tar.gz");
        targz(
            &archive,
            &[
                ("gum_2.0.2_Linux_x86_64/README.md", b"readme"),
                ("gum_2.0.2_Linux_x86_64/gum", b"GUM"),
            ],
        );
        let dest = dir.path().join("out").join("gum");
        extract(&archive, "gum", &dest).unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), b"GUM");
    }

    #[test]
    fn extract_refuses_an_archive_with_two_binaries_of_that_name() {
        let dir = tempfile::tempdir().unwrap();
        let archive = dir.path().join("r.tar.gz");
        targz(&archive, &[("a/jq", b"1"), ("b/jq", b"2")]);
        let dest = dir.path().join("jq");
        let err = extract(&archive, "jq", &dest).unwrap_err();
        assert!(
            matches!(&err, PortError::Backend { detail } if detail.contains(errmsg::MEMBER_AMBIGUOUS)),
            "{err:?}"
        );
        assert!(!dest.exists(), "nothing is written on an ambiguous archive");
    }

    // --- formats ---------------------------------------------------------------

    /// A plain tar holding `entries`.
    fn tar_bytes(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut tar = tar::Builder::new(Vec::new());
        for (name, content) in entries {
            let mut h = tar::Header::new_gnu();
            h.set_size(content.len() as u64);
            h.set_mode(0o644);
            h.set_cksum();
            tar.append_data(&mut h, name, *content).unwrap();
        }
        tar.into_inner().unwrap()
    }

    fn gz(bytes: &[u8]) -> Vec<u8> {
        let mut e = GzEncoder::new(Vec::new(), Compression::default());
        e.write_all(bytes).unwrap();
        e.finish().unwrap()
    }

    fn xz(bytes: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        lzma_rs::xz_compress(&mut std::io::BufReader::new(bytes), &mut out).unwrap();
        out
    }

    fn zst(bytes: &[u8]) -> Vec<u8> {
        ruzstd::encoding::compress_to_vec(bytes, ruzstd::encoding::CompressionLevel::Fastest)
    }

    fn bz2(bytes: &[u8]) -> Vec<u8> {
        let mut e = bzip2::write::BzEncoder::new(Vec::new(), bzip2::Compression::default());
        e.write_all(bytes).unwrap();
        e.finish().unwrap()
    }

    fn zip_bytes(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut w = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        for (name, content) in entries {
            w.start_file(*name, zip::write::SimpleFileOptions::default())
                .unwrap();
            w.write_all(content).unwrap();
        }
        w.finish().unwrap().into_inner()
    }

    const RELEASE: &[(&str, &[u8])] = &[
        ("tool-1.0/README.md", b"readme"),
        ("tool-1.0/tool", b"TOOL"),
    ];

    #[test]
    fn every_archive_format_yields_the_binary_found_by_name() {
        let tar = tar_bytes(RELEASE);
        for (name, bytes) in [
            ("r.tar", tar.clone()),
            ("r.tar.gz", gz(&tar)),
            ("r.tar.xz", xz(&tar)),
            ("r.tar.zst", zst(&tar)),
            ("r.tar.bz2", bz2(&tar)),
            ("r.zip", zip_bytes(RELEASE)),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let archive = dir.path().join(name);
            std::fs::write(&archive, bytes).unwrap();
            let dest = dir.path().join("out").join("tool");
            extract(&archive, "tool", &dest).unwrap_or_else(|e| panic!("{name}: {e}"));
            assert_eq!(std::fs::read(&dest).unwrap(), b"TOOL", "{name}");
        }
    }

    #[test]
    fn a_single_compressed_file_is_the_binary() {
        for (name, bytes) in [
            ("tool.gz", gz(b"TOOL")),
            ("tool.xz", xz(b"TOOL")),
            ("tool.zst", zst(b"TOOL")),
            ("tool.bz2", bz2(b"TOOL")),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let archive = dir.path().join(name);
            std::fs::write(&archive, bytes).unwrap();
            let dest = dir.path().join("tool");
            extract(&archive, "unused", &dest).unwrap_or_else(|e| panic!("{name}: {e}"));
            assert_eq!(std::fs::read(&dest).unwrap(), b"TOOL", "{name}");
        }
    }

    #[test]
    fn a_raw_binary_is_copied_with_its_mode() {
        let dir = tempfile::tempdir().unwrap();
        let archive = dir.path().join("kind-linux-amd64");
        std::fs::write(&archive, b"KIND").unwrap();
        let dest = dir.path().join("bin").join("kind");
        extract(&archive, "kind", &dest).unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), b"KIND");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&dest).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o755);
        }
    }

    #[test]
    fn a_zip_without_the_binary_lists_what_it_has() {
        let dir = tempfile::tempdir().unwrap();
        let archive = dir.path().join("r.zip");
        std::fs::write(&archive, zip_bytes(RELEASE)).unwrap();
        let err = extract(&archive, "other", &dir.path().join("other")).unwrap_err();
        assert!(
            matches!(&err, PortError::Backend { detail }
                if detail.contains(errmsg::MEMBER_NOT_FOUND) && detail.contains("tool-1.0/tool")),
            "{err:?}"
        );
    }

    #[test]
    fn a_zip_with_two_binaries_of_that_name_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let archive = dir.path().join("r.zip");
        std::fs::write(&archive, zip_bytes(&[("a/tool", b"1"), ("b/tool", b"2")])).unwrap();
        let dest = dir.path().join("tool");
        let err = extract(&archive, "tool", &dest).unwrap_err();
        assert!(
            matches!(&err, PortError::Backend { detail } if detail.contains(errmsg::MEMBER_AMBIGUOUS)),
            "{err:?}"
        );
        assert!(!dest.exists());
    }

    #[test]
    fn a_zip_symlink_member_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let archive = dir.path().join("r.zip");
        let mut w = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        w.add_symlink(
            "bin/tool",
            "/somewhere/else",
            zip::write::SimpleFileOptions::default(),
        )
        .unwrap();
        std::fs::write(&archive, w.finish().unwrap().into_inner()).unwrap();
        let err = extract(&archive, "tool", &dir.path().join("tool")).unwrap_err();
        assert!(
            matches!(&err, PortError::Backend { detail } if detail.contains("not a regular file")),
            "{err:?}"
        );
    }

    #[test]
    fn a_corrupt_archive_is_an_error_not_a_panic() {
        for name in [
            "r.tar.xz",
            "r.tar.zst",
            "r.tar.bz2",
            "r.zip",
            "tool.xz",
            "tool.gz",
        ] {
            let dir = tempfile::tempdir().unwrap();
            let archive = dir.path().join(name);
            std::fs::write(&archive, b"not an archive at all").unwrap();
            assert!(
                extract(&archive, "tool", &dir.path().join("tool")).is_err(),
                "{name}"
            );
        }
    }

    // --- remove ----------------------------------------------------------------

    fn remove(path: &Path) -> Result<(), PortError> {
        exec_with(
            &FakeCommandRunner::new(),
            &FakeHttpFetcher::new(),
            &[Step::Remove {
                path: path.to_path_buf(),
            }],
        )
    }

    #[test]
    fn remove_deletes_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("tool");
        std::fs::write(&file, b"x").unwrap();
        remove(&file).unwrap();
        assert!(!file.exists());
    }

    #[test]
    fn removing_a_file_already_gone_is_fine() {
        let dir = tempfile::tempdir().unwrap();
        remove(&dir.path().join("never-there")).unwrap();
    }

    #[test]
    fn remove_refuses_a_directory() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("sub");
        std::fs::create_dir(&sub).unwrap();
        assert!(remove(&sub).is_err());
        assert!(sub.exists());
    }
}
