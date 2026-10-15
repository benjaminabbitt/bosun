//! The step list: typed, fully-determined mutations an [`Installer`](crate::Installer)
//! describes but does not perform (§5.1). One description drives every driver:
//! a host executor runs the steps, and the container renderer prints them as
//! Dockerfile instructions, so the values must be self-contained — argv is a
//! `Vec<String>`, never a shell string, so a renderer can quote it safely.

use std::path::PathBuf;

use bosun_core::Scope;

use crate::error::PortError;

/// Carries out a [`Steps`] list on the host (§5.1): runs each command, fetches
/// and checksum-verifies each download, extracts each archive member. The
/// adapter lives in bosun-host; the applier (bosun-apply) drives it through
/// this port so it never depends on the host crate, and a container renderer
/// can be a peer driver (§8).
pub trait StepRunner {
    /// Carry out `steps` in order, stopping at the first failure.
    fn execute(&self, steps: &[Step]) -> Result<(), PortError>;
}

/// An exit code besides 0 that a command uses to mean it succeeded, and what
/// it says beyond that, which the executor reports.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OkExit {
    /// The exit code.
    pub code: i32,
    /// What it means, said to the user when it happens.
    pub meaning: &'static str,
}

/// One mutation step.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Step {
    /// Run a command. Host: via [`CommandRunner`](crate::CommandRunner) at the
    /// given scope's privilege (§5.5). Container: a `RUN` with shell-quoted argv.
    Run {
        /// The program and its arguments; `argv[0]` is the program.
        argv: Vec<String>,
        /// Environment variables to set (e.g. `DEBIAN_FRONTEND=noninteractive`).
        env: Vec<(String, String)>,
        /// The install scope this runs under.
        scope: Scope,
        /// Exit codes besides 0 that this command uses for success, each with
        /// what else it says (msiexec's 3010: installed, reboot to finish).
        /// Usually empty. A container renderer must accept these too, e.g.
        /// `RUN cmd || [ $? -eq 3010 ]`.
        ok_exits: Vec<OkExit>,
    },
    /// Download `url` to `dest`, verifying its sha256 when one is known.
    /// Host: bosun fetches and checks. Container: `ADD --checksum=sha256:…
    /// <url> <dest>` (a plain `ADD` without one).
    Fetch {
        /// The source URL.
        url: String,
        /// The expected sha256, hex-encoded; `None` when the source publishes
        /// none, and the download is written unverified.
        sha256: Option<String>,
        /// Where to write it.
        dest: PathBuf,
    },
    /// Extract one member of an archive to `dest` with file mode `mode`. The
    /// member is found by file name at any depth (the archive root, a wrapping
    /// directory, a nested `bin/`); exactly one entry may carry that name. A
    /// single compressed file is decompressed, and a raw binary copied, to
    /// `dest` whole; `member` is unused for those.
    Extract {
        /// How the archive is packed.
        format: ArchiveFormat,
        /// The archive file.
        archive: PathBuf,
        /// The member's file name (not a path) within the archive.
        member: String,
        /// Where to write the extracted file.
        dest: PathBuf,
        /// The unix file mode to set (e.g. `0o755`).
        mode: u32,
    },
    /// Delete the file at `path` (a binary bosun installed); a file already
    /// gone is not an error. Host: bosun removes it, on every OS alike.
    /// Container: `RUN rm -f <path>`.
    Remove {
        /// The file to delete.
        path: PathBuf,
    },
}

/// How a downloaded release asset is packed, read from its file name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArchiveFormat {
    /// `.tar`
    Tar,
    /// `.tar.gz`, `.tgz`
    TarGz,
    /// `.tar.xz`, `.txz`
    TarXz,
    /// `.tar.zst`, `.tzst`
    TarZst,
    /// `.tar.bz2`, `.tbz2`, `.tbz`
    TarBz2,
    /// `.zip`
    Zip,
    /// One gzip-compressed file, `.gz`.
    Gz,
    /// One xz-compressed file, `.xz`.
    Xz,
    /// One zstd-compressed file, `.zst`.
    Zst,
    /// One bzip2-compressed file, `.bz2`.
    Bz2,
    /// The binary itself: no archive suffix (`kind-linux-amd64`,
    /// `direnv.linux-amd64`), `.exe` or `.appimage`.
    Raw,
}

/// Suffixes of files that are not a binary to install: OS packages and
/// installers (installed by other backends, or not at all), signatures,
/// checksums and metadata. Compared lower-cased.
const NOT_A_BINARY: &[&str] = &[
    ".deb",
    ".rpm",
    ".apk",
    ".msi",
    ".msix",
    ".dmg",
    ".pkg",
    ".snap",
    ".flatpak",
    ".7z",
    ".rar",
    ".sig",
    ".asc",
    ".minisig",
    ".pem",
    ".crt",
    ".cert",
    ".sha256",
    ".sha256sum",
    ".sha512",
    ".sha512sum",
    ".md5",
    ".sbom",
    ".spdx",
    ".json",
    ".txt",
    ".yaml",
    ".yml",
    ".sh",
    ".ps1",
];

impl ArchiveFormat {
    /// The format of a release asset by its name, or `None` for a file that
    /// is not a binary to install (a package, signature or checksum file).
    pub fn of(name: &str) -> Option<ArchiveFormat> {
        let lower = name.to_ascii_lowercase();
        let by_suffix: &[(&str, ArchiveFormat)] = &[
            (".tar.gz", ArchiveFormat::TarGz),
            (".tgz", ArchiveFormat::TarGz),
            (".tar.xz", ArchiveFormat::TarXz),
            (".txz", ArchiveFormat::TarXz),
            (".tar.zst", ArchiveFormat::TarZst),
            (".tzst", ArchiveFormat::TarZst),
            (".tar.bz2", ArchiveFormat::TarBz2),
            (".tbz2", ArchiveFormat::TarBz2),
            (".tbz", ArchiveFormat::TarBz2),
            (".tar", ArchiveFormat::Tar),
            (".zip", ArchiveFormat::Zip),
            (".gz", ArchiveFormat::Gz),
            (".xz", ArchiveFormat::Xz),
            (".zst", ArchiveFormat::Zst),
            (".bz2", ArchiveFormat::Bz2),
        ];
        if let Some((_, format)) = by_suffix.iter().find(|(s, _)| lower.ends_with(s)) {
            return Some(*format);
        }
        if NOT_A_BINARY.iter().any(|s| lower.ends_with(s)) {
            return None;
        }
        Some(ArchiveFormat::Raw)
    }

    /// The format's name as a file suffix (`tar.gz`), or `raw`.
    pub fn label(self) -> &'static str {
        match self {
            ArchiveFormat::Tar => "tar",
            ArchiveFormat::TarGz => "tar.gz",
            ArchiveFormat::TarXz => "tar.xz",
            ArchiveFormat::TarZst => "tar.zst",
            ArchiveFormat::TarBz2 => "tar.bz2",
            ArchiveFormat::Zip => "zip",
            ArchiveFormat::Gz => "gz",
            ArchiveFormat::Xz => "xz",
            ArchiveFormat::Zst => "zst",
            ArchiveFormat::Bz2 => "bz2",
            ArchiveFormat::Raw => "raw",
        }
    }

    /// Whether the asset holds many files, so the binary is found by name.
    pub fn is_archive(self) -> bool {
        matches!(
            self,
            ArchiveFormat::Tar
                | ArchiveFormat::TarGz
                | ArchiveFormat::TarXz
                | ArchiveFormat::TarZst
                | ArchiveFormat::TarBz2
                | ArchiveFormat::Zip
        )
    }
}

/// A sequence of [`Step`]s an installer operation expands to.
pub type Steps = Vec<Step>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn archives_are_known_by_their_suffix() {
        for (name, format) in [
            ("gum_2.0.2_Linux_x86_64.tar.gz", ArchiveFormat::TarGz),
            ("x.tgz", ArchiveFormat::TarGz),
            (
                "shellcheck-v0.11.0.linux.x86_64.tar.xz",
                ArchiveFormat::TarXz,
            ),
            ("mise-v2026.10.1-linux-x64.tar.zst", ArchiveFormat::TarZst),
            ("x.tar.bz2", ArchiveFormat::TarBz2),
            ("x.tbz", ArchiveFormat::TarBz2),
            ("x.tar", ArchiveFormat::Tar),
            ("deno-x86_64-unknown-linux-gnu.zip", ArchiveFormat::Zip),
            ("RCLONE.ZIP", ArchiveFormat::Zip),
        ] {
            assert_eq!(ArchiveFormat::of(name), Some(format), "{name}");
            assert!(format.is_archive(), "{name}");
        }
    }

    #[test]
    fn single_compressed_files_and_raw_binaries() {
        for (name, format) in [
            ("restic_0.18.1_linux_amd64.bz2", ArchiveFormat::Bz2),
            ("x.gz", ArchiveFormat::Gz),
            ("x.xz", ArchiveFormat::Xz),
            ("x.zst", ArchiveFormat::Zst),
            ("kind-linux-amd64", ArchiveFormat::Raw),
            ("direnv.linux-amd64", ArchiveFormat::Raw),
            ("jq-windows-amd64.exe", ArchiveFormat::Raw),
            ("nvim-linux-x86_64.appimage", ArchiveFormat::Raw),
        ] {
            assert_eq!(ArchiveFormat::of(name), Some(format), "{name}");
            assert!(!format.is_archive(), "{name}");
        }
    }

    #[test]
    fn a_format_is_labelled_by_its_suffix() {
        for name in [
            "a.tar",
            "a.tar.gz",
            "a.tar.xz",
            "a.tar.zst",
            "a.tar.bz2",
            "a.zip",
            "a.gz",
            "a.xz",
            "a.zst",
            "a.bz2",
        ] {
            let format = ArchiveFormat::of(name).unwrap();
            assert_eq!(format!("a.{}", format.label()), name);
        }
        assert_eq!(ArchiveFormat::Raw.label(), "raw");
    }

    #[test]
    fn packages_signatures_and_sums_are_not_binaries() {
        for name in [
            "bat_0.26.1_amd64.deb",
            "x.rpm",
            "x.MSI",
            "x.dmg",
            "checksums.txt",
            "x.tar.gz.sig",
            "x.tar.gz.sha256",
            "x.sbom.json",
            "install.sh",
        ] {
            assert_eq!(ArchiveFormat::of(name), None, "{name}");
        }
    }
}
