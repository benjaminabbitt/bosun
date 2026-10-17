//! The machine `render` targets, read from its root filesystem (§8.3): the
//! files of an image (`--image`), copied out with podman or docker without
//! running it, or a directory (`--rootfs`). Its `os-release` gives the
//! distro, and its `etc/apt` the sources apt's remote index reads (§4.7).

use std::fs;
use std::path::{Path, PathBuf};

use bosun_backends::apt::remote::TargetRoot;
use bosun_core::{Distro, Target};
use bosun_ports::{Command, CommandRunner, PortError};
use tracing::{debug, info};

use crate::error::CliError;
use crate::{errmsg, logmsg};

/// The container engines `--image` copies files out with, in the order tried.
const ENGINES: &[&str] = &["podman", "docker"];

/// The paths copied out of an image, each into the directory of the same
/// name under the root: its os-release (both places it may be) and apt's
/// configuration and keyrings.
const EXPORTED: &[&str] = &[
    "/etc/os-release",
    "/usr/lib/os-release",
    "/etc/apt",
    "/usr/share/keyrings",
];

/// Where `render` reads the target from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum From {
    /// An image, by reference (`debian:12`, `ghcr.io/o/app@sha256:…`).
    Image(String),
    /// A directory holding the target's root filesystem.
    Rootfs(PathBuf),
}

/// A target's root filesystem, read for one platform.
pub struct Root {
    path: PathBuf,
    /// The copy of an image's files, removed when the root is dropped.
    _copy: Option<tempfile::TempDir>,
}

impl Root {
    /// The root of `target` from `from`: an image's files copied out for
    /// `target`'s platform through `runner`, or the directory as it is.
    pub fn read(
        from: &From,
        target: &Target,
        runner: &dyn CommandRunner,
    ) -> Result<Root, CliError> {
        match from {
            From::Rootfs(dir) => {
                info!(message = logmsg::RENDER_FROM_ROOTFS, path = %dir.display(), target = %target);
                Ok(Root {
                    path: dir.clone(),
                    _copy: None,
                })
            }
            From::Image(image) => {
                let copy = tempfile::tempdir().map_err(|e| CliError::Io {
                    path: std::env::temp_dir(),
                    detail: e.to_string(),
                })?;
                export(runner, image, target, copy.path())?;
                Ok(Root {
                    path: copy.path().to_path_buf(),
                    _copy: Some(copy),
                })
            }
        }
    }

    /// The target's distro, from its os-release; `None` without one.
    pub fn distro(&self) -> Option<Distro> {
        let etc = self.path.join("etc/os-release");
        let lib = self.path.join("usr/lib/os-release");
        bosun_host::os_release::detect_from(&[&etc, &lib])
    }

    /// What apt's remote index reads of this root: the release the distro
    /// names, and the root itself. `None` without a codename (no deb
    /// release to read).
    pub fn apt(&self, distro: Option<&Distro>) -> Option<TargetRoot> {
        let distro = distro?;
        Some(TargetRoot {
            distro: distro.id.clone(),
            codename: distro.release("deb")?,
            path: self.path.clone(),
        })
    }
}

fn engine_err(engine: &str, what: &str, detail: impl std::fmt::Display) -> CliError {
    CliError::Resolve(format!(
        "{} ({engine} {what}): {detail}",
        errmsg::RENDER_IMAGE
    ))
}

fn run(runner: &dyn CommandRunner, program: &str, args: &[&str]) -> Result<String, PortError> {
    let out = runner.run(&Command::new(program, args.iter().map(|a| a.to_string())))?;
    if out.success() {
        Ok(out.stdout_utf8().into_owned())
    } else {
        Err(PortError::CommandFailed {
            program: program.to_string(),
            code: out.code,
            stderr: String::from_utf8_lossy(&out.stderr).trim().to_string(),
        })
    }
}

/// Copy [`EXPORTED`] out of `image` for `target`'s platform into `into`: a
/// container is created from the local image (never pulled, not started)
/// with the first engine of [`ENGINES`] present, the paths copied out, and
/// the container removed. A path the image lacks is left out; an image not
/// held locally for the platform is an error naming the pull for it.
pub fn export(
    runner: &dyn CommandRunner,
    image: &str,
    target: &Target,
    into: &Path,
) -> Result<(), CliError> {
    let platform = target.to_string();
    let create = [
        "create",
        "--pull=never",
        "--platform",
        platform.as_str(),
        image,
        "true",
    ];
    let mut found = None;
    for engine in ENGINES {
        match run(runner, engine, &create) {
            Err(PortError::CommandNotFound { .. }) => continue,
            Err(e) => {
                return Err(CliError::Resolve(format!(
                    "{} ({engine} create): {e}; {}: `{engine} pull --platform {platform} {image}`",
                    errmsg::RENDER_IMAGE,
                    errmsg::RENDER_PULL_IMAGE
                )))
            }
            Ok(out) => {
                found = Some((*engine, out));
                break;
            }
        }
    }
    let Some((engine, out)) = found else {
        return Err(CliError::Resolve(format!(
            "{}: {}",
            errmsg::RENDER_NO_ENGINE,
            ENGINES.join(", ")
        )));
    };
    let id = out.lines().last().unwrap_or_default().trim().to_string();
    info!(message = logmsg::RENDER_FROM_IMAGE, image = %image, engine = %engine, platform = %platform);
    let copied = copy_out(runner, engine, &id, into);
    let removed = run(runner, engine, &["rm", &id]);
    copied?;
    removed.map_err(|e| engine_err(engine, "rm", e))?;
    Ok(())
}

fn copy_out(
    runner: &dyn CommandRunner,
    engine: &str,
    id: &str,
    into: &Path,
) -> Result<(), CliError> {
    for path in EXPORTED {
        let rel = path.trim_start_matches('/');
        let parent = into
            .join(rel)
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_default();
        fs::create_dir_all(&parent).map_err(|e| CliError::Io {
            path: parent.clone(),
            detail: e.to_string(),
        })?;
        let src = format!("{id}:{path}");
        let dst = parent.to_string_lossy().into_owned();
        if let Err(e) = run(runner, engine, &["cp", &src, &dst]) {
            debug!(message = logmsg::RENDER_IMAGE_LACKS, path = %path, error = %e);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::Mutex;

    use bosun_core::{Arch, Os};
    use bosun_ports::Output;

    use super::*;

    /// Answers each command in turn: `Some(code, stdout)` as its output,
    /// `None` as a program that isn't there.
    struct Scripted {
        answers: Mutex<VecDeque<Option<(i32, &'static str)>>>,
        calls: Mutex<Vec<String>>,
    }

    impl Scripted {
        fn new(answers: Vec<Option<(i32, &'static str)>>) -> Scripted {
            Scripted {
                answers: Mutex::new(answers.into()),
                calls: Mutex::new(Vec::new()),
            }
        }

        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl CommandRunner for Scripted {
        fn run(&self, c: &Command) -> Result<Output, PortError> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("{} {}", c.program, c.args.join(" ")));
            match self.answers.lock().unwrap().pop_front().flatten() {
                Some((code, stdout)) => Ok(Output {
                    code: Some(code),
                    stdout: stdout.as_bytes().to_vec(),
                    stderr: b"no such path".to_vec(),
                }),
                None => Err(PortError::CommandNotFound {
                    program: c.program.clone(),
                }),
            }
        }
    }

    const ARM64: Target = Target::Platform {
        os: Os::Linux,
        arch: Arch::Arm64,
    };

    #[test]
    fn an_images_files_are_copied_out_of_a_container_never_started() {
        let dir = tempfile::tempdir().unwrap();
        let runner = Scripted::new(vec![
            Some((0, "Trying to pull...\nabc123\n")),
            Some((0, "")),
            Some((1, "")),
            Some((0, "")),
            Some((0, "")),
            Some((0, "")),
        ]);
        export(&runner, "debian:12", &ARM64, dir.path()).unwrap();
        let d = dir.path().display();
        assert_eq!(
            runner.calls(),
            vec![
                "podman create --pull=never --platform linux/arm64 debian:12 true".to_string(),
                format!("podman cp abc123:/etc/os-release {d}/etc"),
                format!("podman cp abc123:/usr/lib/os-release {d}/usr/lib"),
                format!("podman cp abc123:/etc/apt {d}/etc"),
                format!("podman cp abc123:/usr/share/keyrings {d}/usr/share"),
                "podman rm abc123".to_string(),
            ]
        );
        assert!(dir.path().join("usr/share").is_dir());
    }

    #[test]
    fn docker_is_used_when_podman_is_not_there() {
        let dir = tempfile::tempdir().unwrap();
        let runner = Scripted::new(vec![
            None,
            Some((0, "id\n")),
            Some((0, "")),
            Some((0, "")),
            Some((0, "")),
            Some((0, "")),
            Some((0, "")),
        ]);
        export(&runner, "debian:12", &ARM64, dir.path()).unwrap();
        let calls = runner.calls();
        assert!(calls[1].starts_with("docker create"), "{calls:?}");
        assert_eq!(calls.last().unwrap(), "docker rm id");
    }

    #[test]
    fn no_engine_or_a_failed_create_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let err = export(&Scripted::new(vec![]), "x", &ARM64, dir.path())
            .unwrap_err()
            .to_string();
        assert!(err.contains(errmsg::RENDER_NO_ENGINE), "{err}");
        assert!(err.contains("podman, docker"), "{err}");

        let runner = Scripted::new(vec![Some((125, ""))]);
        let err = export(&runner, "nope:1", &ARM64, dir.path())
            .unwrap_err()
            .to_string();
        assert!(err.contains(errmsg::RENDER_IMAGE), "{err}");
        assert!(
            err.contains("podman create") && err.contains("no such path"),
            "{err}"
        );
        assert!(
            err.contains("podman pull --platform linux/arm64 nope:1"),
            "{err}"
        );
        assert_eq!(runner.calls().len(), 1);

        let runner = Scripted::new(vec![None, Some((1, ""))]);
        let err = export(&runner, "nope:1", &ARM64, dir.path())
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("docker pull --platform linux/arm64 nope:1"),
            "{err}"
        );
    }

    #[test]
    fn a_failed_remove_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let runner = Scripted::new(vec![
            Some((0, "id")),
            Some((0, "")),
            Some((0, "")),
            Some((0, "")),
            Some((0, "")),
            Some((1, "")),
        ]);
        let err = export(&runner, "x", &ARM64, dir.path())
            .unwrap_err()
            .to_string();
        assert!(err.contains("podman rm"), "{err}");
    }

    #[test]
    fn a_root_gives_its_distro_and_the_release_apt_reads() {
        let dir = tempfile::tempdir().unwrap();
        let root = Root::read(
            &From::Rootfs(dir.path().to_path_buf()),
            &ARM64,
            &Scripted::new(vec![]),
        )
        .unwrap();
        assert_eq!(root.path, dir.path());
        assert_eq!(root.distro(), None);
        assert_eq!(root.apt(None), None);

        fs::create_dir_all(dir.path().join("usr/lib")).unwrap();
        fs::write(
            dir.path().join("usr/lib/os-release"),
            "ID=ubuntu\nID_LIKE=debian\nVERSION_ID=\"24.04\"\nVERSION_CODENAME=noble\n",
        )
        .unwrap();
        let distro = root.distro().unwrap();
        assert_eq!(distro.id_like, vec!["debian"]);
        assert_eq!(
            root.apt(Some(&distro)),
            Some(TargetRoot {
                distro: "ubuntu".to_string(),
                codename: "noble".to_string(),
                path: dir.path().to_path_buf(),
            })
        );
        let rolling = Distro {
            version_codename: None,
            base_codename: None,
            ..distro
        };
        assert_eq!(root.apt(Some(&rolling)), None);

        fs::create_dir_all(dir.path().join("etc")).unwrap();
        fs::write(dir.path().join("etc/os-release"), "ID=debian\n").unwrap();
        assert_eq!(root.distro().unwrap().id, "debian");
    }

    #[test]
    fn an_images_copy_is_removed_with_its_root() {
        let runner = Scripted::new(vec![Some((0, "id")); 6]);
        let root = Root::read(&From::Image("x".to_string()), &ARM64, &runner).unwrap();
        let path = root.path.clone();
        assert!(path.is_dir());
        drop(root);
        assert!(!path.exists());
    }
}
