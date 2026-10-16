//! C library detection (§5.4): the machine's libc family and version, read
//! from the system rather than from bosun itself. bosun ships as a static musl
//! binary, so its own libc says nothing about the machine.
//!
//! The family comes from the ELF `PT_INTERP` of `/bin/sh`, the dynamic loader
//! every dynamically linked program on the machine uses: `ld-musl-*` is musl,
//! `ld-linux-*` glibc. The version comes from running that loader through the
//! [`CommandRunner`] port: glibc's prints `… stable release version 2.36.` for
//! `--version`; musl's prints its banner, `Version 1.2.5` included, to stderr
//! and exits 1.
//!
//! No `/bin/sh`, a statically linked one, or an unfamiliar loader: libc
//! unknown, never an error. A loader that won't run or prints no version
//! leaves the family known and the version unknown.

use std::fs;
use std::path::Path;

use bosun_core::{Libc, LibcFamily};
use bosun_ports::{Command, CommandRunner};
use tracing::debug;

use crate::logmsg;

/// The shell whose loader names the machine's libc.
const SHELL: &str = "/bin/sh";

/// The ELF program header type of the interpreter (loader) path.
const PT_INTERP: u32 = 3;

/// The path in `elf`'s `PT_INTERP` program header: the loader it asks for.
/// `None` when `elf` is not an ELF file, has no interpreter (statically
/// linked), or is truncated before the path ends.
pub fn interpreter(elf: &[u8]) -> Option<String> {
    if elf.get(..4)? != b"\x7fELF" {
        return None;
    }
    let class64 = match elf.get(4)? {
        1 => false,
        2 => true,
        _ => return None,
    };
    let big_endian = match elf.get(5)? {
        1 => false,
        2 => true,
        _ => return None,
    };
    // An unsigned field of `width` bytes at `at`, in the file's byte order.
    let field = |at: usize, width: usize| -> Option<usize> {
        let bytes = elf.get(at..at.checked_add(width)?)?;
        let mut word = [0u8; 8];
        let n = if big_endian {
            word[8 - width..].copy_from_slice(bytes);
            u64::from_be_bytes(word)
        } else {
            word[..width].copy_from_slice(bytes);
            u64::from_le_bytes(word)
        };
        usize::try_from(n).ok()
    };
    // Offsets of e_phoff, e_phentsize, e_phnum, and within a program header
    // of p_offset and p_filesz, with the address width, per ELF class.
    let (phoff_at, phentsize_at, phnum_at, offset_at, filesz_at, word) = if class64 {
        (32, 54, 56, 8, 32, 8)
    } else {
        (28, 42, 44, 4, 16, 4)
    };
    let phoff = field(phoff_at, word)?;
    let phentsize = field(phentsize_at, 2)?;
    (0..field(phnum_at, 2)?).find_map(|i| {
        let ph = phoff.checked_add(i.checked_mul(phentsize)?)?;
        if field(ph, 4)? != PT_INTERP as usize {
            return None;
        }
        let start = field(ph + offset_at, word)?;
        let path = elf.get(start..start.checked_add(field(ph + filesz_at, word)?)?)?;
        let path = path.split(|b| *b == 0).next().unwrap_or(path);
        Some(String::from_utf8_lossy(path).into_owned())
    })
}

/// The libc family a loader path names, by its file name: `ld-musl-*` is
/// musl, `ld-linux-*` (and 32-bit x86's `ld-linux.so.2`) glibc. `None` for
/// any other loader.
pub fn family_of(loader: &str) -> Option<LibcFamily> {
    let name = loader.rsplit('/').next()?;
    if name.starts_with("ld-musl-") {
        Some(LibcFamily::Musl)
    } else if name.starts_with("ld-linux-") || name.starts_with("ld-linux.") {
        Some(LibcFamily::Gnu)
    } else {
        None
    }
}

/// The version a loader reported for `--version`: glibc's `stable release
/// version X.Y.` on stdout, musl's `Version X.Y.Z` line on stderr.
pub fn version_from(family: LibcFamily, stdout: &str, stderr: &str) -> Option<String> {
    let rest = match family {
        LibcFamily::Gnu => stdout
            .lines()
            .find_map(|line| line.split_once("stable release version "))
            .map(|(_, rest)| rest),
        LibcFamily::Musl => stderr
            .lines()
            .find_map(|line| line.strip_prefix("Version ")),
    }?;
    let version = rest
        .split(|c: char| !(c.is_ascii_digit() || c == '.'))
        .next()?
        .trim_end_matches('.');
    (!version.is_empty()).then(|| version.to_string())
}

/// Detect the machine's libc from `/bin/sh` (see the module docs).
pub fn detect(runner: &dyn CommandRunner) -> Option<Libc> {
    detect_from(Path::new(SHELL), runner)
}

/// Detect the libc from the loader `shell` asks for, running it through
/// `runner` for the version. Split out so tests can point it at a fixture.
pub fn detect_from(shell: &Path, runner: &dyn CommandRunner) -> Option<Libc> {
    let loader = fs::read(shell).ok().and_then(|elf| interpreter(&elf));
    let Some((loader, family)) = loader.and_then(|l| family_of(&l).map(|f| (l, f))) else {
        debug!(message = logmsg::LIBC_UNKNOWN, shell = %shell.display());
        return None;
    };
    let version = runner
        .run(&Command::new(loader.as_str(), ["--version".to_string()]))
        .ok()
        .and_then(|out| {
            version_from(
                family,
                &out.stdout_utf8(),
                &String::from_utf8_lossy(&out.stderr),
            )
        });
    match &version {
        Some(v) => debug!(message = logmsg::LIBC_DETECTED, %family, version = %v, %loader),
        None => debug!(message = logmsg::LIBC_VERSION_UNKNOWN, %family, %loader),
    }
    Some(Libc { family, version })
}

#[cfg(test)]
mod tests {
    use super::*;
    use bosun_ports::testing::FakeCommandRunner;
    use bosun_ports::Output;

    const DEBIAN12_SH: &[u8] = include_bytes!("../tests/fixtures/libc/debian12-sh.head");
    const DEBIAN13_SH: &[u8] = include_bytes!("../tests/fixtures/libc/debian13-sh.head");
    const UBUNTU2204_SH: &[u8] = include_bytes!("../tests/fixtures/libc/ubuntu2204-sh.head");
    const UBUNTU2404_SH: &[u8] = include_bytes!("../tests/fixtures/libc/ubuntu2404-sh.head");
    const ALPINE320_SH: &[u8] = include_bytes!("../tests/fixtures/libc/alpine320-sh.head");

    const DEBIAN12_LD: &str = include_str!("../tests/fixtures/libc/debian12-ld-version.stdout");
    const DEBIAN13_LD: &str = include_str!("../tests/fixtures/libc/debian13-ld-version.stdout");
    const UBUNTU2204_LD: &str = include_str!("../tests/fixtures/libc/ubuntu2204-ld-version.stdout");
    const UBUNTU2404_LD: &str = include_str!("../tests/fixtures/libc/ubuntu2404-ld-version.stdout");
    const ALPINE320_LD: &str = include_str!("../tests/fixtures/libc/alpine320-ld-version.stderr");

    const GLIBC_X86_64: &str = "/lib64/ld-linux-x86-64.so.2";
    const MUSL_X86_64: &str = "/lib/ld-musl-x86_64.so.1";

    #[test]
    fn reads_the_loader_from_real_shells() {
        for (name, sh, want) in [
            ("debian 12", DEBIAN12_SH, GLIBC_X86_64),
            ("debian 13", DEBIAN13_SH, GLIBC_X86_64),
            ("ubuntu 22.04", UBUNTU2204_SH, GLIBC_X86_64),
            ("ubuntu 24.04", UBUNTU2404_SH, GLIBC_X86_64),
            ("alpine 3.20", ALPINE320_SH, MUSL_X86_64),
        ] {
            assert_eq!(interpreter(sh).as_deref(), Some(want), "{name}");
        }
    }

    /// A minimal ELF image: header plus program headers, each `(type, data)`
    /// placed after them, in the given class (32/64) and byte order.
    fn elf(class64: bool, big_endian: bool, segments: &[(u32, &[u8])]) -> Vec<u8> {
        let (ehsize, phentsize) = if class64 { (64, 56) } else { (52, 32) };
        let phoff = ehsize;
        let mut data_at = phoff + phentsize * segments.len();
        let mut out = vec![0u8; data_at];
        out[..4].copy_from_slice(b"\x7fELF");
        out[4] = if class64 { 2 } else { 1 };
        out[5] = if big_endian { 2 } else { 1 };
        out[6] = 1;
        let put = |buf: &mut Vec<u8>, at: usize, v: u64, width: usize| {
            let field = &mut buf[at..at + width];
            field.copy_from_slice(&v.to_le_bytes()[..width]);
            if big_endian {
                field.reverse();
            }
        };
        let (phoff_at, phentsize_at, phnum_at, word) = if class64 {
            (32, 54, 56, 8)
        } else {
            (28, 42, 44, 4)
        };
        put(&mut out, phoff_at, phoff as u64, word);
        put(&mut out, phentsize_at, phentsize as u64, 2);
        put(&mut out, phnum_at, segments.len() as u64, 2);
        let mut payload = Vec::new();
        for (i, (ty, bytes)) in segments.iter().enumerate() {
            let ph = phoff + i * phentsize;
            let (offset_at, filesz_at) = if class64 { (8, 32) } else { (4, 16) };
            put(&mut out, ph, u64::from(*ty), 4);
            put(&mut out, ph + offset_at, data_at as u64, word);
            put(&mut out, ph + filesz_at, bytes.len() as u64, word);
            payload.extend_from_slice(bytes);
            data_at += bytes.len();
        }
        out.extend(payload);
        out
    }

    const PT_LOAD: u32 = 1;

    #[test]
    fn reads_the_interpreter_in_each_class_and_byte_order() {
        let interp: &[u8] = b"/lib/ld-musl-aarch64.so.1\0";
        for class64 in [false, true] {
            for big in [false, true] {
                let image = elf(class64, big, &[(PT_LOAD, b"code"), (PT_INTERP, interp)]);
                assert_eq!(
                    interpreter(&image).as_deref(),
                    Some("/lib/ld-musl-aarch64.so.1"),
                    "64-bit {class64}, big-endian {big}"
                );
            }
        }
    }

    #[test]
    fn a_static_binary_has_no_interpreter() {
        assert_eq!(interpreter(&elf(true, false, &[(PT_LOAD, b"code")])), None);
        // A real shell with its PT_INTERP header retyped as PT_LOAD.
        let mut sh = DEBIAN12_SH.to_vec();
        let phoff = u64::from_le_bytes(sh[32..40].try_into().unwrap()) as usize;
        let phentsize = u16::from_le_bytes(sh[54..56].try_into().unwrap()) as usize;
        let phnum = u16::from_le_bytes(sh[56..58].try_into().unwrap()) as usize;
        for i in 0..phnum {
            let at = phoff + i * phentsize;
            if sh[at..at + 4] == PT_INTERP.to_le_bytes() {
                sh[at..at + 4].copy_from_slice(&PT_LOAD.to_le_bytes());
            }
        }
        assert_eq!(interpreter(&sh), None);
    }

    #[test]
    fn something_that_is_not_elf_has_no_interpreter() {
        assert_eq!(interpreter(b""), None);
        assert_eq!(interpreter(b"#!/bin/sh\necho hi\n"), None);
        assert_eq!(interpreter(b"\x7fELF"), None);
        // An unknown class or byte order.
        let mut bad_class = elf(true, false, &[(PT_INTERP, b"/lib/ld.so\0")]);
        bad_class[4] = 3;
        assert_eq!(interpreter(&bad_class), None);
        let mut bad_order = elf(true, false, &[(PT_INTERP, b"/lib/ld.so\0")]);
        bad_order[5] = 0;
        assert_eq!(interpreter(&bad_order), None);
    }

    #[test]
    fn a_truncated_image_has_no_interpreter() {
        let image = elf(true, false, &[(PT_INTERP, b"/lib/ld-musl-x86_64.so.1\0")]);
        // Cut inside the program headers, then inside the path.
        assert_eq!(interpreter(&image[..70]), None);
        assert_eq!(interpreter(&image[..image.len() - 3]), None);
        // The whole image still reads.
        assert_eq!(
            interpreter(&image).as_deref(),
            Some("/lib/ld-musl-x86_64.so.1")
        );
    }

    #[test]
    fn an_interpreter_without_its_nul_is_read_to_the_segment_end() {
        let image = elf(false, false, &[(PT_INTERP, b"/lib/ld-linux.so.2")]);
        assert_eq!(interpreter(&image).as_deref(), Some("/lib/ld-linux.so.2"));
    }

    #[test]
    fn the_loader_file_name_names_the_family() {
        for (loader, want) in [
            (GLIBC_X86_64, Some(LibcFamily::Gnu)),
            ("/lib/ld-linux-aarch64.so.1", Some(LibcFamily::Gnu)),
            ("/lib/ld-linux-armhf.so.3", Some(LibcFamily::Gnu)),
            ("/lib/ld-linux.so.2", Some(LibcFamily::Gnu)),
            (MUSL_X86_64, Some(LibcFamily::Musl)),
            ("/lib/ld-musl-aarch64.so.1", Some(LibcFamily::Musl)),
            ("ld-musl-x86_64.so.1", Some(LibcFamily::Musl)),
            ("/system/bin/linker64", None),
            ("/libexec/ld-elf.so.1", None),
            ("/lib/ld-musl", None),
            ("/opt/ld-linux-x86-64.so.2/sh", None),
            ("", None),
        ] {
            assert_eq!(family_of(loader), want, "{loader}");
        }
    }

    #[test]
    fn reads_the_glibc_version_from_real_loaders() {
        for (name, out, want) in [
            ("debian 12", DEBIAN12_LD, "2.36"),
            ("debian 13", DEBIAN13_LD, "2.41"),
            ("ubuntu 22.04", UBUNTU2204_LD, "2.35"),
            ("ubuntu 24.04", UBUNTU2404_LD, "2.39"),
        ] {
            assert_eq!(
                version_from(LibcFamily::Gnu, out, "").as_deref(),
                Some(want),
                "{name}"
            );
        }
    }

    #[test]
    fn reads_the_musl_version_from_a_real_loader() {
        assert_eq!(
            version_from(LibcFamily::Musl, "", ALPINE320_LD).as_deref(),
            Some("1.2.5")
        );
    }

    #[test]
    fn reads_an_older_glibc_banner() {
        // glibc 2.17 (CentOS 7) ends the line with the authors.
        let out = "ld.so (GNU libc) stable release version 2.17, by Roland McGrath et al.\n";
        assert_eq!(
            version_from(LibcFamily::Gnu, out, "").as_deref(),
            Some("2.17")
        );
    }

    #[test]
    fn each_family_reads_only_its_own_banner() {
        assert_eq!(version_from(LibcFamily::Gnu, "", ALPINE320_LD), None);
        assert_eq!(version_from(LibcFamily::Musl, DEBIAN12_LD, ""), None);
        assert_eq!(version_from(LibcFamily::Gnu, "", DEBIAN12_LD), None);
        assert_eq!(version_from(LibcFamily::Musl, ALPINE320_LD, ""), None);
    }

    #[test]
    fn a_banner_without_a_version_number_gives_none() {
        assert_eq!(
            version_from(LibcFamily::Gnu, "stable release version .\n", ""),
            None
        );
        assert_eq!(
            version_from(LibcFamily::Musl, "", "musl libc (x86_64)\nVersion \n"),
            None
        );
        assert_eq!(version_from(LibcFamily::Gnu, "garbage\n", ""), None);
    }

    fn write_shell(dir: &Path, bytes: &[u8]) -> std::path::PathBuf {
        let path = dir.join("sh");
        std::fs::write(&path, bytes).unwrap();
        path
    }

    #[test]
    fn detects_glibc_and_its_version_by_running_the_loader() {
        let dir = tempfile::tempdir().unwrap();
        let sh = write_shell(dir.path(), DEBIAN12_SH);
        let runner = FakeCommandRunner::new().with_stdout(DEBIAN12_LD);
        assert_eq!(
            detect_from(&sh, &runner),
            Some(Libc {
                family: LibcFamily::Gnu,
                version: Some("2.36".into()),
            })
        );
        let calls = runner.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].program, GLIBC_X86_64);
        assert_eq!(calls[0].args, vec!["--version".to_string()]);
    }

    #[test]
    fn detects_musl_though_its_loader_exits_1() {
        let dir = tempfile::tempdir().unwrap();
        let sh = write_shell(dir.path(), ALPINE320_SH);
        let runner = FakeCommandRunner::new().with_output(Output {
            code: Some(1),
            stdout: Vec::new(),
            stderr: ALPINE320_LD.as_bytes().to_vec(),
        });
        assert_eq!(
            detect_from(&sh, &runner),
            Some(Libc {
                family: LibcFamily::Musl,
                version: Some("1.2.5".into()),
            })
        );
        assert_eq!(runner.calls()[0].program, MUSL_X86_64);
    }

    #[test]
    fn a_loader_that_will_not_run_leaves_the_version_unknown() {
        let dir = tempfile::tempdir().unwrap();
        let sh = write_shell(dir.path(), UBUNTU2404_SH);
        let missing = FakeCommandRunner::new();
        assert_eq!(
            detect_from(&sh, &missing),
            Some(Libc {
                family: LibcFamily::Gnu,
                version: None,
            })
        );
    }

    /// `detect` reads this host's own /bin/sh: its family is whatever that
    /// shell's loader names (none where /bin/sh is missing or static).
    #[test]
    fn detect_reads_the_hosts_own_shell() {
        let want = fs::read(SHELL)
            .ok()
            .and_then(|elf| interpreter(&elf))
            .and_then(|loader| family_of(&loader));
        let runner = FakeCommandRunner::new();
        assert_eq!(detect(&runner).map(|l| l.family), want);
        assert_eq!(runner.calls().len(), usize::from(want.is_some()));
    }

    #[test]
    fn no_shell_a_static_shell_or_an_unknown_loader_is_libc_unknown() {
        let dir = tempfile::tempdir().unwrap();
        let runner = FakeCommandRunner::new();
        assert_eq!(detect_from(&dir.path().join("nope"), &runner), None);
        let static_sh = write_shell(dir.path(), &elf(true, false, &[(PT_LOAD, b"x")]));
        assert_eq!(detect_from(&static_sh, &runner), None);
        let bionic = write_shell(
            dir.path(),
            &elf(true, false, &[(PT_INTERP, b"/system/bin/linker64\0")]),
        );
        assert_eq!(detect_from(&bionic, &runner), None);
        // Nothing was run when the family is unknown.
        assert!(runner.calls().is_empty());
    }
}
