//! `bosun render` (§8.3): the versions a container build installs, as build
//! args, env lines or a JSON versions file. For this machine they come from
//! its lock, which must be there and current. For a machine bosun isn't
//! running on (another platform, an image or root filesystem given with
//! `--image` / `--rootfs`, or one described by `--distro` / `--libc`) they
//! are resolved through remote indexes (§4.7), never from this machine's
//! installed packages, package sources or lock (apt's index may run this
//! machine's apt, in a scratch root of its own).

use std::path::{Path, PathBuf};

use bosun_apply::render::{render, Rendered};
use bosun_backends::apt::remote::TargetRoot;
use bosun_core::platform::Selector;
use bosun_core::record::LockDocument;
use bosun_core::{Distro, Libc, LibcFamily, Manifest, Target};
use bosun_host::ProcessCommandRunner;
use bosun_io::manifest_hash;
use bosun_resolve::{resolve, Host, NotApplicable, Outcome, ResolveError};

use crate::commands::{
    canonical_manifest, load, load_checked, manifest_file, remote_preference, this_lock,
};
use crate::env::{self, Env, RemoteEnv};
use crate::error::CliError;
use crate::target_root::{From, Root};
use crate::{errmsg, logmsg};

/// What to render, and for which machines.
pub struct Request {
    /// The manifest directory or file.
    pub path: Option<PathBuf>,
    /// The target platforms as written; none means this machine's.
    pub targets: Vec<String>,
    /// Where the target is read from: an image or a root filesystem.
    pub from: Option<From>,
    /// `--distro`, as written.
    pub distro: Option<String>,
    /// `--like`: the families `--distro` is like.
    pub like: Vec<String>,
    /// `--libc`, as written.
    pub libc: Option<String>,
    /// `--installer`, the preferred order.
    pub installer: Vec<String>,
}

/// The manifest rendered for each requested target, with the targets in the
/// order asked.
pub fn rendered(req: Request) -> Result<(Vec<Target>, Rendered), CliError> {
    let flag_distro = req
        .distro
        .as_deref()
        .map(|d| parse_distro(d, &req.like))
        .transpose()?;
    let libc = req.libc.as_deref().map(parse_libc).transpose()?;
    let (os, arch) = env::this_platform()?;
    let here = Target::Platform { os, arch };
    let targets = if req.targets.is_empty() {
        vec![here]
    } else {
        req.targets
            .iter()
            .map(|t| parse_target(t))
            .collect::<Result<Vec<_>, _>>()?
    };
    let foreign =
        |t: &Target| req.from.is_some() || flag_distro.is_some() || libc.is_some() || *t != here;
    // This machine, when a target is it: its run reads the user's files as
    // whoever it does them as (§4.5), as render for another machine does.
    let mut local: Option<Env> = match targets.iter().all(foreign) {
        true => None,
        false => Some(env::assemble()?),
    };
    let files = match &local {
        Some(env) => env.user_files(),
        None => env::user_files()?,
    };
    let file = manifest_file(files.as_ref(), req.path)?;

    let mut manifest: Option<Manifest> = None;
    let mut locks: Vec<(Target, LockDocument)> = Vec::new();
    for target in &targets {
        let lock = if foreign(target) {
            let root = req
                .from
                .as_ref()
                .map(|from| Root::read(from, target, &ProcessCommandRunner))
                .transpose()?;
            let distro = match &root {
                Some(root) => root.distro(),
                None => flag_distro.clone(),
            };
            let apt_root = root.as_ref().and_then(|r| r.apt(distro.as_ref()));
            let remote = remote_env(target, libc.as_ref(), apt_root)?;
            let m = loaded(&mut manifest, || {
                load_checked(remote.option_keys(), files.as_ref(), &file)
            })?;
            if libc.is_none() && has_selector(m, |s| s.libc().is_some()) {
                tracing::warn!(message = logmsg::RENDER_NO_LIBC, target = %target);
            }
            if distro.is_none() && has_selector(m, |s| s.distro().is_some()) {
                tracing::warn!(message = logmsg::RENDER_NO_DISTRO, target = %target);
            }
            crate::commands::warn_of_own_codenames(m, distro.as_ref());
            let machine = Machine {
                target: *target,
                distro: distro.as_ref(),
                libc: libc.as_ref(),
            };
            foreign_lock(&remote, m, &machine, &req.installer)?
        } else {
            let env = match &mut local {
                Some(env) => env,
                None => local.insert(env::assemble()?),
            };
            let m = loaded(&mut manifest, || load(env, &file))?;
            this_machines_lock(env, &file, m)?
        };
        locks.push((*target, lock));
    }
    let manifest = manifest.expect("at least one target loads the manifest");
    let by_target: Vec<(Target, &LockDocument)> = locks.iter().map(|(t, l)| (*t, l)).collect();
    let rendered = render(&manifest, &by_target).map_err(|e| CliError::Resolve(e.to_string()))?;
    Ok((targets, rendered))
}

/// The manifest, loading it with `load` the first time.
fn loaded(
    slot: &mut Option<Manifest>,
    load: impl FnOnce() -> Result<Manifest, CliError>,
) -> Result<&Manifest, CliError> {
    if slot.is_none() {
        *slot = Some(load()?);
    }
    Ok(slot.as_ref().expect("just loaded"))
}

/// This machine's lock for the manifest (§8.3): a missing lock, or one the
/// manifest has moved on from, is an error naming `bosun lock`.
fn this_machines_lock(
    env: &Env,
    file: &Path,
    manifest: &Manifest,
) -> Result<LockDocument, CliError> {
    let lock =
        this_lock(env, &canonical_manifest(env.user_files().as_ref(), file))?.ok_or_else(|| {
            CliError::Resolve(format!(
                "{} {}; {}",
                errmsg::RENDER_NO_LOCK,
                file.display(),
                errmsg::RUN_LOCK
            ))
        })?;
    if lock.manifest_hash != manifest_hash(manifest) {
        return Err(CliError::Resolve(format!(
            "{} {}; {}",
            file.display(),
            errmsg::RENDER_STALE_LOCK,
            errmsg::RUN_LOCK
        )));
    }
    Ok(lock)
}

/// The remote indexes for `target`'s platform.
fn remote_env(
    target: &Target,
    libc: Option<&Libc>,
    root: Option<TargetRoot>,
) -> Result<RemoteEnv, CliError> {
    let Target::Platform { os, arch } = *target else {
        unreachable!("a render target is a platform; parse_target refuses `any`")
    };
    env::remote(os, arch, libc.map(|l| l.family), root)
}

/// A machine bosun isn't running on: its platform and what the command line
/// says of it.
struct Machine<'a> {
    target: Target,
    distro: Option<&'a Distro>,
    libc: Option<&'a Libc>,
}

/// The manifest resolved for `machine` through `remote` (§4.7): every entry
/// that applies gets the newest version its spec allows there. An entry
/// whose backends have no remote index, or nothing in its spec, is an error.
fn foreign_lock(
    remote: &RemoteEnv,
    manifest: &Manifest,
    machine: &Machine,
    installer: &[String],
) -> Result<LockDocument, CliError> {
    let Target::Platform { os, arch } = machine.target else {
        unreachable!("a render target is a platform; parse_target refuses `any`")
    };
    let host = Host {
        os,
        arch,
        distro: machine.distro,
        libc: machine.libc,
    };
    let ids: Vec<_> = remote.backends.iter().map(|b| b.id.clone()).collect();
    let pref = remote_preference(installer, &ids)?;
    let resolution = resolve(
        manifest,
        &manifest_hash(manifest),
        &host,
        &remote.resolvables(),
        &pref,
    )
    .map_err(|e| match e {
        // Only a machine with no distro lacks the facts a deb identity needs.
        ResolveError::NoDistro { .. } => CliError::Resolve(format!(
            "{}: {e}; {}",
            machine.target,
            errmsg::RENDER_GIVE_DISTRO
        )),
        e => CliError::Resolve(format!("{}: {e}", machine.target)),
    })?;
    let mut problems = Vec::new();
    for entry in &resolution.outcomes {
        match &entry.outcome {
            Outcome::Unsatisfiable { spec, available } => problems.push(format!(
                "{}: unsatisfiable {spec} (available: {available})",
                entry.name
            )),
            Outcome::NotApplicable(NotApplicable::NoRegisteredBackend { candidates }) => {
                let names: Vec<&str> = candidates.iter().map(|b| b.as_str()).collect();
                problems.push(format!(
                    "{}: {} {}",
                    entry.name,
                    errmsg::RENDER_NO_REMOTE_INDEX,
                    names.join(", ")
                ));
            }
            _ => {}
        }
    }
    if !problems.is_empty() {
        return Err(CliError::Resolve(format!(
            "{}: {}",
            machine.target,
            problems.join("; ")
        )));
    }
    Ok(resolution.lock)
}

/// Whether any install block, the defaults' included, has a selector that
/// `matches`.
fn has_selector(manifest: &Manifest, matches: impl Fn(&Selector) -> bool) -> bool {
    manifest
        .packages
        .iter()
        .flat_map(|p| p.install.keys())
        .chain(manifest.defaults.keys())
        .any(matches)
}

/// A render target: a platform (`linux/arm64`), never `any`.
fn parse_target(s: &str) -> Result<Target, CliError> {
    match s.parse::<Target>() {
        Ok(t @ Target::Platform { .. }) => Ok(t),
        Ok(Target::Any) => Err(CliError::Usage(format!(
            "{}: {s}",
            errmsg::RENDER_BAD_TARGET
        ))),
        Err(e) => Err(CliError::Usage(format!(
            "{}: {e}",
            errmsg::RENDER_BAD_TARGET
        ))),
    }
}

/// A distro or family id as `--distro` and `--like` take it.
fn token(t: &str) -> bool {
    !t.is_empty() && !t.contains(['@', ':', '/', ' '])
}

/// `--distro <id>[@<release>]`: the distro's `ID`, and the release as its
/// packages' purl spells it (§5.4), so `debian@bookworm` is the codename and
/// `fedora@fedora-41` is `VERSION_ID` 41. It is like the families `like`
/// names (`--like`, its os-release `ID_LIKE`), and no others.
fn parse_distro(s: &str, like: &[String]) -> Result<Distro, CliError> {
    let bad = || CliError::Usage(format!("{}: {s:?}", errmsg::RENDER_BAD_DISTRO));
    let (id, release) = match s.split_once('@') {
        Some((id, release)) => (id, Some(release)),
        None => (s, None),
    };
    if !token(id) || !release.is_none_or(token) {
        return Err(bad());
    }
    if let Some(family) = like.iter().find(|f| !token(f)) {
        return Err(CliError::Usage(format!(
            "{}: {family:?}",
            errmsg::RENDER_BAD_LIKE
        )));
    }
    Ok(Distro {
        id: id.to_string(),
        id_like: like.to_vec(),
        version_id: release
            .and_then(|r| r.strip_prefix(&format!("{id}-")))
            .map(str::to_string),
        version_codename: release.map(str::to_string),
        base_codename: None,
    })
}

/// `--libc <family>[@<version>]`: `gnu`, `musl`, `gnu@2.36`.
fn parse_libc(s: &str) -> Result<Libc, CliError> {
    let bad = || CliError::Usage(format!("{}: {s:?}", errmsg::RENDER_BAD_LIBC));
    let (family, version) = match s.split_once('@') {
        Some((family, version)) if !version.is_empty() => (family, Some(version)),
        Some(_) => return Err(bad()),
        None => (s, None),
    };
    let family: LibcFamily = family.parse().map_err(|_| bad())?;
    Ok(Libc {
        family,
        version: version.map(str::to_string),
    })
}

/// The JSON versions file (§8.3).
pub fn json(rendered: &Rendered) -> String {
    serde_json::to_string_pretty(rendered).expect("output serialization is infallible")
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use bosun_core::manifest::{InstallBlock, ManifestEntry};
    use bosun_core::{Arch, Os};

    use super::*;

    #[test]
    fn a_distro_is_its_id_and_release_spelled_for_both_package_types() {
        let d = parse_distro("debian@bookworm", &[]).unwrap();
        assert_eq!(d.id, "debian");
        assert!(d.id_like.is_empty());
        assert_eq!(d.release("deb").as_deref(), Some("bookworm"));
        assert_eq!(d.version_id, None);

        let f = parse_distro("fedora@fedora-41", &[]).unwrap();
        assert_eq!(f.version_id.as_deref(), Some("41"));
        assert_eq!(f.release("rpm").as_deref(), Some("fedora-41"));

        let bare = parse_distro("alpine", &[]).unwrap();
        assert_eq!((bare.version_codename, bare.version_id), (None, None));
    }

    #[test]
    fn a_distro_is_like_the_families_given_and_no_others() {
        let like = |l: &[&str]| -> Vec<String> { l.iter().map(|s| s.to_string()).collect() };
        let mint = parse_distro("linuxmint@wilma", &like(&["ubuntu", "debian"])).unwrap();
        assert_eq!(mint.id_like, vec!["ubuntu", "debian"]);
        assert!(mint.satisfies_family("debian"));
        assert!(!parse_distro("ubuntu@noble", &[])
            .unwrap()
            .satisfies_family("debian"));
        for bad in ["", "linux:debian", "a b", "x@y", "a/b"] {
            let err = parse_distro("kali", &like(&["debian", bad]))
                .unwrap_err()
                .to_string();
            assert!(err.contains(errmsg::RENDER_BAD_LIKE), "{bad:?}: {err}");
        }
    }

    #[test]
    fn a_malformed_distro_is_a_usage_error() {
        for bad in [
            "",
            "@bookworm",
            "debian@",
            "debian@a@b",
            "linux:debian",
            "deb ian",
            "debian@a/b",
        ] {
            let err = parse_distro(bad, &[]).unwrap_err().to_string();
            assert!(err.contains(errmsg::RENDER_BAD_DISTRO), "{bad:?}: {err}");
        }
    }

    #[test]
    fn a_libc_is_a_family_and_maybe_a_version() {
        assert_eq!(
            parse_libc("gnu@2.36").unwrap(),
            Libc {
                family: LibcFamily::Gnu,
                version: Some("2.36".to_string())
            }
        );
        assert_eq!(parse_libc("musl").unwrap().version, None);
        for bad in ["glibc", "gnu@", "", "@2.36"] {
            let err = parse_libc(bad).unwrap_err().to_string();
            assert!(err.contains(errmsg::RENDER_BAD_LIBC), "{bad:?}: {err}");
        }
    }

    #[test]
    fn a_target_is_a_platform() {
        assert_eq!(
            parse_target("linux/arm64").unwrap(),
            Target::Platform {
                os: Os::Linux,
                arch: Arch::Arm64
            }
        );
        for bad in ["any", "linux", "linux/sparc"] {
            let err = parse_target(bad).unwrap_err().to_string();
            assert!(err.contains(errmsg::RENDER_BAD_TARGET), "{bad:?}: {err}");
        }
    }

    fn manifest(selectors: &[&str], defaults: &[&str]) -> Manifest {
        let blocks = |s: &[&str]| -> BTreeMap<_, _> {
            s.iter()
                .map(|s| (s.parse().unwrap(), InstallBlock::default()))
                .collect()
        };
        Manifest {
            version: 2,
            defaults: blocks(defaults),
            packages: vec![ManifestEntry {
                name: "x".to_string(),
                spec: None,
                formerly: Vec::new(),
                install: blocks(selectors),
                var: None,
            }],
        }
    }

    #[test]
    fn a_selector_anywhere_is_found() {
        let libc = |s: &Selector| s.libc().is_some();
        let distro = |s: &Selector| s.distro().is_some();
        let plain = manifest(&["linux", "any"], &["linux"]);
        assert!(!has_selector(&plain, libc));
        assert!(!has_selector(&plain, distro));
        assert!(has_selector(&manifest(&["linux:musl"], &[]), libc));
        assert!(has_selector(
            &manifest(&["linux"], &["linux:gnu@>=2.31"]),
            libc
        ));
        assert!(has_selector(&manifest(&["linux:debian"], &[]), distro));
    }
}
