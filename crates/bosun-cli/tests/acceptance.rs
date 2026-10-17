//! Cucumber acceptance tests (§11): each scenario runs the real `bosun` binary
//! against the hermetic fixture seam (`BOSUN_FIXTURE` + `BOSUN_STATE_DIR` in a
//! temp dir), so `cargo test --workspace` exercises the user-visible behaviour
//! without touching the host. Real-backend scenarios are a separate tier
//! (slice 11-2, `just acceptance-real`).
//!
//! The feature files read as documentation: the machine, its backends, what
//! they publish, what is already installed and what bosun has already recorded
//! are all prose steps. The steps here build the fixture and state JSON the
//! seam consumes; backend metadata (purl/vers type, scope, hold support,
//! platform independence) comes from [`known_backend`], not the Gherkin.
//!
//! GitHub releases are the exception: bosun is their installer, so its real
//! github-release backend runs against releases the steps build — small
//! `.tar.gz` archives laid out like the real ones, served through the seam's
//! canned HTTP — and installs into a scratch bin dir.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use cucumber::gherkin::Step;
use cucumber::{given, then, when, World};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tempfile::TempDir;

/// The host bosun runs on.
#[derive(Debug, Clone)]
struct Machine {
    os: String,
    arch: String,
    /// The os-release facts of a named distro.
    distro: Option<MachineDistro>,
    /// The C library family (`gnu`, `musl`) and version, when known.
    libc: Option<(String, String)>,
    elevated: bool,
}

/// A machine's os-release facts: `ID`, `ID_LIKE`, `VERSION_CODENAME` and `VERSION_ID`.
#[derive(Debug, Clone)]
struct MachineDistro {
    id: String,
    id_like: Vec<String>,
    codename: Option<String>,
    version_id: Option<String>,
}

/// A backend's fixed metadata, as the real backend declares it.
#[derive(Debug, Clone, Copy)]
struct BackendMeta {
    purl_type: &'static str,
    vers_type: &'static str,
    platform_independent: bool,
    scope: &'static str,
    supports_hold: bool,
}

/// The metadata of each backend bosun compiles in.
fn known_backend(id: &str) -> BackendMeta {
    match id {
        "apk" => BackendMeta {
            purl_type: "apk",
            vers_type: "apk",
            platform_independent: false,
            scope: "system",
            supports_hold: true,
        },
        "apt" => BackendMeta {
            purl_type: "deb",
            vers_type: "deb",
            platform_independent: false,
            scope: "system",
            supports_hold: true,
        },
        "dnf" => BackendMeta {
            purl_type: "rpm",
            vers_type: "rpm",
            platform_independent: false,
            scope: "system",
            supports_hold: true,
        },
        "cargo" => BackendMeta {
            purl_type: "cargo",
            vers_type: "cargo",
            platform_independent: true,
            scope: "user",
            supports_hold: false,
        },
        "npm" => BackendMeta {
            purl_type: "npm",
            vers_type: "npm",
            platform_independent: true,
            scope: "user",
            supports_hold: false,
        },
        "uv" => BackendMeta {
            purl_type: "pypi",
            vers_type: "pypi",
            platform_independent: true,
            scope: "user",
            supports_hold: false,
        },
        "github-release" => BackendMeta {
            purl_type: "github",
            vers_type: "semver",
            platform_independent: false,
            scope: "user",
            supports_hold: false,
        },
        "winget" | "scoop" => BackendMeta {
            purl_type: if id == "winget" { "winget" } else { "scoop" },
            vers_type: "generic",
            platform_independent: false,
            scope: "user",
            supports_hold: true,
        },
        "choco" => BackendMeta {
            purl_type: "choco",
            vers_type: "generic",
            platform_independent: false,
            scope: "system",
            supports_hold: true,
        },
        "github-deb" | "github-rpm" => BackendMeta {
            purl_type: "github",
            vers_type: "semver",
            platform_independent: false,
            scope: "system",
            supports_hold: id == "github-deb",
        },
        other => panic!("unknown backend {other:?}; add it to known_backend"),
    }
}

/// A backend present on the machine, with what it publishes and what is
/// already installed through it outside bosun.
#[derive(Debug, Clone)]
struct Backend {
    id: String,
    version: String,
    meta: BackendMeta,
    available: BTreeMap<String, Vec<String>>,
    checksums: BTreeMap<String, String>,
    installed: BTreeMap<String, String>,
    /// What the package manager records as installed by the user.
    by_hand: Vec<String>,
    /// apt's priority per package, where it is not `optional`.
    priorities: BTreeMap<String, String>,
    /// bosun's own backend, run against [`BosunWorld::releases`].
    real: bool,
}

/// A GitHub release the steps publish: its assets, by name, in order.
#[derive(Debug, Clone, Default)]
struct Release {
    assets: Vec<(String, Vec<u8>)>,
}

#[derive(Debug, Default, World)]
struct BosunWorld {
    dir: Option<TempDir>,
    machine: Option<Machine>,
    backends: Vec<Backend>,
    /// Compiled-in backends whose program this machine lacks.
    absent: Vec<String>,
    fail_step: Option<String>,
    /// GitHub releases by `(owner/repo, tag)`.
    releases: BTreeMap<(String, String), Release>,
    /// What distro mirrors list, by `(distro, suite, arch)`: package and version.
    apt_mirror: BTreeMap<(String, String, String), Vec<(String, String)>>,
    /// What flat vendor apt repositories list, by URL: package and version.
    vendor_repos: BTreeMap<String, Vec<(String, String)>>,
    /// What the npm registry and PyPI answer, by URL.
    registry: BTreeMap<String, Value>,
    /// Seeded state packages per scope (`system` / `user`).
    seeded: BTreeMap<String, Vec<Value>>,
    /// The manifest as the scenario wrote it.
    manifest: Option<String>,
    code: i32,
    stdout: String,
    stderr: String,
}

impl BosunWorld {
    fn path(&mut self) -> PathBuf {
        self.dir
            .get_or_insert_with(|| tempfile::tempdir().unwrap())
            .path()
            .to_path_buf()
    }

    fn machine(&self) -> &Machine {
        self.machine
            .as_ref()
            .expect("describe the machine first (\"Given a Linux amd64 machine, ...\")")
    }

    fn backend_mut(&mut self, id: &str) -> &mut Backend {
        self.backends
            .iter_mut()
            .find(|b| b.id == id)
            .unwrap_or_else(|| {
                panic!("backend {id} is not available; say \"{id} <version> is available\"")
            })
    }

    /// The backend a package comes from: the one publishing it, else the only one.
    fn backend_for(&mut self, pkg: &str) -> &mut Backend {
        let idx = self
            .backends
            .iter()
            .position(|b| b.available.contains_key(pkg))
            .or_else(|| (self.backends.len() == 1).then_some(0))
            .unwrap_or_else(|| panic!("no backend publishes {pkg}"));
        &mut self.backends[idx]
    }

    fn fixture(&self) -> Value {
        let m = self.machine();
        let mut fx = json!({
            "os": m.os,
            "arch": m.arch,
            "privilege": if m.elevated { "elevated" } else { "unelevated" },
            "user": "ben",
            "backends": self.backends.iter().map(|b| json!({
                "id": b.id,
                "version": b.version,
                "purl_type": b.meta.purl_type,
                "vers_type": b.meta.vers_type,
                "platform_independent": b.meta.platform_independent,
                "scope": b.meta.scope,
                "supports_hold": b.meta.supports_hold,
                "available": b.available,
                "checksums": b.checksums,
                "installed": b.installed,
                "by_hand": b.by_hand,
                "priorities": b.priorities,
                "real": b.real,
            })).collect::<Vec<_>>(),
            "absent": self.absent,
            "http": self.http(),
            "home": self.home(),
        });
        if let Some(d) = &m.distro {
            fx["distro"] = json!({
                "id": d.id,
                "id_like": d.id_like,
                "codename": d.codename,
                "version_id": d.version_id,
            });
        }
        if let Some((family, version)) = &m.libc {
            fx["libc"] = json!({"family": family, "version": version});
        }
        if let Some(f) = &self.fail_step {
            fx["fail_step"] = json!(f);
        }
        fx
    }

    /// The scratch home directory of the user bosun runs as.
    fn home(&self) -> PathBuf {
        self.dir.as_ref().expect("a temp dir").path().join("home")
    }

    /// Where bosun's own installer puts binaries: `~/.local/bin`.
    fn bin_dir(&self) -> PathBuf {
        self.home().join(".local").join("bin")
    }

    /// The canned HTTP for the published releases: each repo's release list,
    /// each tag's release, and each asset's download, written to files.
    fn http(&self) -> BTreeMap<String, PathBuf> {
        let root = self.dir.as_ref().expect("a temp dir").path().join("http");
        fs::create_dir_all(&root).unwrap();
        let mut http = BTreeMap::new();
        let mut serve = |url: String, body: &[u8]| {
            let file = root.join(format!("{}", http.len()));
            fs::write(&file, body).unwrap();
            http.insert(url, file);
        };
        let mut lists: BTreeMap<&str, Vec<Value>> = BTreeMap::new();
        for ((repo, tag), release) in &self.releases {
            let assets: Vec<Value> = release
                .assets
                .iter()
                .map(|(name, _)| {
                    json!({
                        "name": name,
                        "browser_download_url":
                            format!("https://github.com/{repo}/releases/download/{tag}/{name}"),
                    })
                })
                .collect();
            let doc = json!({"tag_name": tag, "assets": assets});
            serve(
                format!("https://api.github.com/repos/{repo}/releases/tags/{tag}"),
                doc.to_string().as_bytes(),
            );
            lists.entry(repo).or_default().push(doc);
            for (name, body) in &release.assets {
                serve(
                    format!("https://github.com/{repo}/releases/download/{tag}/{name}"),
                    body,
                );
            }
        }
        for (repo, list) in lists {
            serve(
                format!("https://api.github.com/repos/{repo}/releases?per_page=100&page=1"),
                Value::Array(list).to_string().as_bytes(),
            );
        }
        for (url, body) in self.apt_mirror_files() {
            serve(url, &body);
        }
        for (url, packages) in &self.vendor_repos {
            let mut index = String::new();
            for (pkg, version) in packages {
                index.push_str(&format!("Package: {pkg}\nVersion: {version}\n\n"));
            }
            let xz = compressed("xz", index.as_bytes());
            let sha: String = Sha256::digest(&xz)
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect();
            let release = format!("Origin: vendor\nSHA256:\n {sha} {} Packages.xz\n", xz.len());
            serve(format!("{url}/InRelease"), release.as_bytes());
            serve(format!("{url}/Packages.xz"), &xz);
        }
        for (url, doc) in &self.registry {
            serve(url.clone(), doc.to_string().as_bytes());
        }
        http
    }

    /// The release files and package indexes of each distro release a step
    /// listed packages for: its own, updates and security suites, each with
    /// a `main` index per arch named (empty where no step listed any), at the
    /// URLs apt's remote index reads them from.
    fn apt_mirror_files(&self) -> Vec<(String, Vec<u8>)> {
        let mut releases: BTreeMap<(&str, &str), Vec<&str>> = BTreeMap::new();
        for (distro, suite, arch) in self.apt_mirror.keys() {
            let codename = suite.split('-').next().unwrap();
            let arches = releases.entry((distro, codename)).or_default();
            if !arches.contains(&arch.as_str()) {
                arches.push(arch);
            }
        }
        let mut files = Vec::new();
        for ((distro, codename), arches) in releases {
            for suffix in ["", "-updates", "-security"] {
                let suite = format!("{codename}{suffix}");
                // One release file per mirror, listing each arch's index.
                let mut listed: BTreeMap<&str, String> = BTreeMap::new();
                for arch in &arches {
                    let mut packages = String::new();
                    let key = (distro.to_string(), suite.clone(), arch.to_string());
                    for (pkg, version) in self.apt_mirror.get(&key).into_iter().flatten() {
                        packages.push_str(&format!(
                            "Package: {pkg}\nVersion: {version}\nArchitecture: {arch}\n\n"
                        ));
                    }
                    let xz = compressed("xz", packages.as_bytes());
                    let sha: String = Sha256::digest(&xz)
                        .iter()
                        .map(|b| format!("{b:02x}"))
                        .collect();
                    let mirror = apt_mirror_url(distro, suffix, arch);
                    listed.entry(mirror).or_default().push_str(&format!(
                        " {sha} {} main/binary-{arch}/Packages.xz\n",
                        xz.len()
                    ));
                    files.push((
                        format!("{mirror}/dists/{suite}/main/binary-{arch}/by-hash/SHA256/{sha}"),
                        xz,
                    ));
                }
                for (mirror, sums) in listed {
                    let release = format!(
                        "Suite: {suite}\nCodename: {suite}\nAcquire-By-Hash: yes\nSHA256:\n{sums}"
                    );
                    files.push((
                        format!("{mirror}/dists/{suite}/InRelease"),
                        release.into_bytes(),
                    ));
                }
            }
        }
        files
    }

    fn release_mut(&mut self, repo: &str, tag: &str) -> &mut Release {
        self.releases
            .entry((repo.to_string(), tag.to_string()))
            .or_default()
    }

    fn state_text(&mut self) -> String {
        let p = self.path();
        let mut all = String::new();
        for scope in ["system", "user"] {
            let f = p.join("state").join(scope).join("state.json");
            if let Ok(text) = fs::read_to_string(&f) {
                all.push_str(&text);
            }
        }
        all
    }

    fn state_docs(&mut self) -> Vec<Value> {
        let p = self.path();
        ["system", "user"]
            .iter()
            .filter_map(|scope| {
                fs::read_to_string(p.join("state").join(scope).join("state.json")).ok()
            })
            .map(|t| serde_json::from_str(&t).expect("state is JSON"))
            .collect()
    }

    fn lock(&mut self) -> (String, Value) {
        let files = lock_files(&self.path());
        let [file] = files.as_slice() else {
            panic!("want one lock, found {files:?}");
        };
        let text = fs::read_to_string(file).expect("read the lock");
        let doc = serde_json::from_str(&text).expect("the lock is JSON");
        (text, doc)
    }
}

/// The mirror a stock image of `distro` reads the suite with `suffix` from.
fn apt_mirror_url(distro: &str, suffix: &str, arch: &str) -> &'static str {
    match (distro, suffix, arch) {
        ("debian", "-security", _) => "https://deb.debian.org/debian-security",
        ("debian", _, _) => "https://deb.debian.org/debian",
        ("ubuntu", _, "arm64") => "https://ports.ubuntu.com/ubuntu-ports",
        ("ubuntu", "-security", _) => "https://security.ubuntu.com/ubuntu",
        ("ubuntu", _, _) => "https://archive.ubuntu.com/ubuntu",
        _ => panic!("no apt mirror for {distro}"),
    }
}

/// The locks this machine holds: in the user state dir, never the project.
fn lock_files(project: &Path) -> Vec<PathBuf> {
    let dir = project.join("state").join("user").join("locks");
    match fs::read_dir(&dir) {
        Ok(entries) => entries.map(|e| e.unwrap().path()).collect(),
        Err(_) => Vec::new(),
    }
}

fn versions(list: &str) -> Vec<String> {
    list.replace(" and ", ", ")
        .split(", ")
        .map(|v| v.trim().to_string())
        .collect()
}

// ---- the machine and its backends -----------------------------------------

#[given(
    regex = r"^an? (Linux|Debian \S+|Ubuntu \S+|Fedora \S+|Rocky \S+|Alpine(?: \S+)?|Windows) (\S+) machine, (not elevated|elevated)$"
)]
fn a_machine(world: &mut BosunWorld, os: String, arch: String, privilege: String) {
    let distro = match os.split_once(' ') {
        Some(("Debian", codename)) => Some(MachineDistro {
            id: "debian".into(),
            id_like: Vec::new(),
            codename: Some(codename.into()),
            version_id: None,
        }),
        Some(("Ubuntu", codename)) => Some(MachineDistro {
            id: "ubuntu".into(),
            id_like: vec!["debian".into()],
            codename: Some(codename.into()),
            version_id: None,
        }),
        Some(("Fedora", release)) => Some(MachineDistro {
            id: "fedora".into(),
            id_like: Vec::new(),
            codename: None,
            version_id: Some(release.into()),
        }),
        Some(("Rocky", release)) => Some(MachineDistro {
            id: "rocky".into(),
            id_like: vec!["rhel".into(), "centos".into(), "fedora".into()],
            codename: None,
            version_id: Some(release.into()),
        }),
        Some(("Alpine", version_id)) => Some(MachineDistro {
            id: "alpine".into(),
            id_like: Vec::new(),
            codename: None,
            version_id: Some(version_id.into()),
        }),
        _ if os == "Alpine" => Some(MachineDistro {
            id: "alpine".into(),
            id_like: Vec::new(),
            codename: None,
            version_id: None,
        }),
        _ => None,
    };
    world.machine = Some(Machine {
        os: if os == "Windows" { "windows" } else { "linux" }.into(),
        arch,
        distro,
        libc: None,
        elevated: privilege == "elevated",
    });
}

/// The C library the loader of the machine's /bin/sh reports.
#[given(regex = r"^the machine's C library is (glibc|musl) (\S+)$")]
fn machine_libc(world: &mut BosunWorld, name: String, version: String) {
    let family = if name == "glibc" { "gnu" } else { "musl" };
    world
        .machine
        .as_mut()
        .expect("describe the machine first")
        .libc = Some((family.to_string(), version));
}

#[given(regex = r"^a Debian (\S+) machine with no release codename, not elevated$")]
fn a_debian_machine_without_release(world: &mut BosunWorld, arch: String) {
    world.machine = Some(Machine {
        os: "linux".into(),
        arch,
        distro: Some(MachineDistro {
            id: "debian".into(),
            id_like: Vec::new(),
            codename: None,
            version_id: None,
        }),
        libc: None,
        elevated: false,
    });
}

#[given(regex = r"^(\S+) (\S+) is available$")]
fn backend_available(world: &mut BosunWorld, id: String, version: String) {
    let meta = known_backend(&id);
    world.backends.push(Backend {
        id,
        version,
        meta,
        available: BTreeMap::new(),
        checksums: BTreeMap::new(),
        installed: BTreeMap::new(),
        by_hand: Vec::new(),
        priorities: BTreeMap::new(),
        real: false,
    });
}

/// A compiled-in backend whose program is not on this machine: bosun probes
/// for it and finds nothing.
#[given(regex = r"^the machine has no (\S+)$")]
fn backend_absent(world: &mut BosunWorld, id: String) {
    world.absent.push(id);
}

// ---- GitHub releases --------------------------------------------------------

#[given("bosun installs GitHub releases itself")]
fn github_release_itself(world: &mut BosunWorld) {
    world.path();
    world.backends.push(Backend {
        id: "github-release".into(),
        version: "1.0.0".into(),
        meta: known_backend("github-release"),
        available: BTreeMap::new(),
        checksums: BTreeMap::new(),
        installed: BTreeMap::new(),
        by_hand: Vec::new(),
        priorities: BTreeMap::new(),
        real: true,
    });
}

/// bosun's own github-deb or github-rpm backend, its package manager answered
/// from the fixture's package db.
#[given(regex = r"^bosun installs release (\.deb|\.rpm) packages$")]
fn github_package_itself(world: &mut BosunWorld, kind: String) {
    world.path();
    let id = if kind == ".deb" {
        "github-deb"
    } else {
        "github-rpm"
    };
    world.backends.push(Backend {
        id: id.into(),
        version: "1.0.0".into(),
        meta: known_backend(id),
        available: BTreeMap::new(),
        checksums: BTreeMap::new(),
        installed: BTreeMap::new(),
        by_hand: Vec::new(),
        priorities: BTreeMap::new(),
        real: true,
    });
}

/// The bytes of the binary shipped in `asset`, so an installed file says which
/// archive it came from.
fn binary_bytes(asset: &str) -> Vec<u8> {
    format!("#!/bin/sh\necho {asset}\n").into_bytes()
}

/// A plain tar holding `entries`.
fn tar_of(entries: &[(String, Vec<u8>)]) -> Vec<u8> {
    let mut tar = tar::Builder::new(Vec::new());
    for (name, body) in entries {
        let mut header = tar::Header::new_gnu();
        header.set_size(body.len() as u64);
        header.set_mode(0o755);
        header.set_cksum();
        tar.append_data(&mut header, name, body.as_slice()).unwrap();
    }
    tar.into_inner().unwrap()
}

fn zip_of(entries: &[(String, Vec<u8>)]) -> Vec<u8> {
    use std::io::Write;
    let mut w = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    for (name, body) in entries {
        w.start_file(name.as_str(), zip::write::SimpleFileOptions::default())
            .unwrap();
        w.write_all(body).unwrap();
    }
    w.finish().unwrap().into_inner()
}

/// `bytes` compressed the way the file-name suffix `suffix` says (`gz`,
/// `xz`, `zst`, `bz2`), or as they are for anything else.
fn compressed(suffix: &str, bytes: &[u8]) -> Vec<u8> {
    use std::io::Write;
    match suffix {
        "gz" | "tgz" => {
            let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
            e.write_all(bytes).unwrap();
            e.finish().unwrap()
        }
        "xz" => {
            let mut out = Vec::new();
            lzma_rs::xz_compress(&mut std::io::BufReader::new(bytes), &mut out).unwrap();
            out
        }
        "zst" => {
            ruzstd::encoding::compress_to_vec(bytes, ruzstd::encoding::CompressionLevel::Fastest)
        }
        "bz2" => {
            let mut e = bzip2::write::BzEncoder::new(Vec::new(), bzip2::Compression::default());
            e.write_all(bytes).unwrap();
            e.finish().unwrap()
        }
        _ => bytes.to_vec(),
    }
}

/// `entries` packed as the asset name `asset` says: a zip, or a tar under the
/// compression its suffix names (`.tar.gz`, `.tar.xz`, `.tar.zst`, …).
fn pack(asset: &str, entries: &[(String, Vec<u8>)]) -> Vec<u8> {
    if asset.ends_with(".zip") {
        return zip_of(entries);
    }
    let suffix = asset.rsplit('.').next().unwrap_or_default();
    compressed(suffix, &tar_of(entries))
}

/// An archive laid out like a goreleaser one: the binary, LICENSE and README,
/// at the root or under `dir/`.
fn release_archive(asset: &str, binary: &str, dir: Option<&str>) -> Vec<u8> {
    let at = |name: &str| match dir {
        Some(d) => format!("{d}/{name}"),
        None => name.to_string(),
    };
    pack(
        asset,
        &[
            (at("LICENSE"), b"MIT".to_vec()),
            (at("README.md"), b"# readme".to_vec()),
            (
                at(&format!("completions/{binary}.bash")),
                b"complete".to_vec(),
            ),
            (at(binary), binary_bytes(asset)),
        ],
    )
}

#[given(
    regex = r"^(\S+/\S+) (\S+) publishes (\S+\.(?:tar(?:\.\w+)?|zip)) with (\S+) (?:at its root|in (\S+)/)$"
)]
#[when(
    regex = r"^(\S+/\S+) (\S+) publishes (\S+\.(?:tar(?:\.\w+)?|zip)) with (\S+) (?:at its root|in (\S+)/)$"
)]
fn release_publishes(
    world: &mut BosunWorld,
    repo: String,
    tag: String,
    asset: String,
    binary: String,
    dir: String,
) {
    let dir = (!dir.is_empty()).then_some(dir.as_str());
    let archive = release_archive(&asset, &binary, dir);
    // Publishing an asset again replaces it, as re-uploading to a release does.
    let assets = &mut world.release_mut(&repo, &tag).assets;
    assets.retain(|(name, _)| *name != asset);
    assets.push((asset, archive));
}

/// A release asset that is the binary itself, or the binary alone compressed
/// (`restic_0.18.1_linux_amd64.bz2`).
#[given(regex = r"^(\S+/\S+) (\S+) publishes (\S+) holding just the binary$")]
fn release_publishes_binary(world: &mut BosunWorld, repo: String, tag: String, asset: String) {
    let suffix = asset.rsplit_once('.').map_or("", |(_, s)| s).to_string();
    let body = compressed(&suffix, &binary_bytes(&asset));
    world.release_mut(&repo, &tag).assets.push((asset, body));
}

#[given(regex = r"^(\S+/\S+) (v\S+) also publishes (\S+\.tar\.gz)$")]
fn release_also_publishes(world: &mut BosunWorld, repo: String, tag: String, asset: String) {
    let release = world.release_mut(&repo, &tag);
    let binary = repo.rsplit('/').next().unwrap().to_string();
    let archive = release_archive(&asset, &binary, None);
    release.assets.push((asset, archive));
}

#[given(regex = r"^(\S+/\S+) (v\S+) publishes checksums\.txt$")]
fn release_publishes_checksums(world: &mut BosunWorld, repo: String, tag: String) {
    let release = world.release_mut(&repo, &tag);
    let sums: String = release
        .assets
        .iter()
        .map(|(name, body)| {
            let sha: String = Sha256::digest(body)
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect();
            format!("{sha}  {name}\n")
        })
        .collect();
    release
        .assets
        .push(("checksums.txt".to_string(), sums.into_bytes()));
}

#[given(regex = r"^(\S+) publishes (\S+) (.+)$")]
#[when(regex = r"^(\S+) publishes (\S+) (.+)$")]
fn publishes(world: &mut BosunWorld, id: String, pkg: String, list: String) {
    world
        .backend_mut(&id)
        .available
        .insert(pkg, versions(&list));
}

#[given(regex = r"^the (\S+) release publishes checksum (\S+)$")]
fn release_checksum(world: &mut BosunWorld, pkg: String, checksum: String) {
    world.backend_for(&pkg).checksums.insert(pkg, checksum);
}

#[given(regex = r"^(\S+) (\S+) is installed outside bosun$")]
fn installed_outside(world: &mut BosunWorld, pkg: String, version: String) {
    world.backend_for(&pkg).installed.insert(pkg, version);
}

#[given(regex = r"^(\S+) (\S+) is installed by hand$")]
fn installed_by_hand(world: &mut BosunWorld, pkg: String, version: String) {
    let b = world.backend_for(&pkg);
    b.installed.insert(pkg.clone(), version);
    b.by_hand.push(pkg);
}

/// What the Debian installer leaves: the base system, marked manual.
#[given(
    regex = r"^(\S+) (\S+) is part of the base system \(priority (\S+)\), marked as installed by hand$"
)]
fn base_system(world: &mut BosunWorld, pkg: String, version: String, priority: String) {
    installed_by_hand(world, pkg.clone(), version);
    world.backend_for(&pkg).priorities.insert(pkg, priority);
}

/// What an Alpine image's world file lists before anyone adds to it.
#[given(regex = r"^(\S+) (\S+) is part of Alpine's base system, in the world file$")]
fn alpine_base(world: &mut BosunWorld, pkg: String, version: String) {
    installed_by_hand(world, pkg, version);
}

#[given(regex = r"^installing (\S+) fails$")]
fn installing_fails(world: &mut BosunWorld, pkg: String) {
    world.fail_step = Some(pkg);
}

/// Seed bosun's own record of a package: `installed` (bosun put it there) or
/// `accepted` (found present and adopted), claimed by this project's manifest
/// or by another project's. The record says nothing of a hold, as state
/// written before bosun recorded its holds did, unless it says `unheld`.
#[given(
    regex = r#"^bosun (installed|accepted) (\S+) (\S+)( unheld)? for (this|another) project wanting "(.+)"$"#
)]
fn bosun_recorded(
    world: &mut BosunWorld,
    origin: String,
    pkg: String,
    version: String,
    unheld: String,
    whose: String,
    spec: String,
) {
    let p = world.path();
    let m = world.machine().clone();
    let b = world.backend_for(&pkg).clone();
    let purl = match b.meta.purl_type {
        "deb" => {
            let d = m.distro.clone().expect("a deb package needs a distro");
            let distro = d
                .codename
                .map(|c| format!("&distro={c}"))
                .unwrap_or_default();
            format!("pkg:deb/{}/{pkg}?arch={}{distro}", d.id, m.arch)
        }
        t => format!("pkg:{t}/{pkg}"),
    };
    let manifest = if whose == "this" {
        p.join("bosun.yaml").display().to_string()
    } else {
        "/other/bosun.yaml".to_string()
    };
    let target = format!("{}/{}", m.os, m.arch);
    let claim = json!({"manifest": manifest, "spec": format!("vers:{}/{spec}", b.meta.vers_type)});
    let mut leaf = json!({
        "backend": b.id,
        "purl": purl,
        "installed": version,
        "origin": origin,
        "claimedBy": [],
    });
    if !unheld.is_empty() {
        leaf["held"] = json!(false);
    }
    let entry = json!({"name": pkg, "targets": {&target: leaf}});
    let scope = b.meta.scope.to_string();
    let packages = world.seeded.entry(scope.clone()).or_default();
    // A package seeded again gains a claim, as state records one package per
    // name with every manifest that claims it.
    let at = match packages.iter().position(|p| p["name"] == pkg.as_str()) {
        Some(at) => at,
        None => {
            packages.push(entry);
            packages.len() - 1
        }
    };
    packages[at]["targets"][&target]["claimedBy"]
        .as_array_mut()
        .unwrap()
        .push(claim);
    let doc = json!({"stateVersion": 1, "packages": packages});
    let dir = p.join("state").join(scope);
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("state.json"), doc.to_string()).unwrap();
}

#[given("the project has a bosun.lock from an older bosun")]
fn old_project_lock(world: &mut BosunWorld) {
    let old = r#"{"lockfileVersion": 1, "targets": {}, "packages": []}"#;
    fs::write(world.path().join("bosun.lock"), old).unwrap();
}

#[when("the manifest is replaced by:")]
fn replace_manifest(world: &mut BosunWorld, step: &Step) {
    the_manifest(world, step);
}

#[given("the manifest:")]
fn the_manifest(world: &mut BosunWorld, step: &Step) {
    let p = world.path();
    let body = docstring(step);
    fs::write(p.join("bosun.yaml"), &body).unwrap();
    world.manifest = Some(body);
}

#[given("the config:")]
fn the_config(world: &mut BosunWorld, step: &Step) {
    let dir = world.path().join("config").join("bosun");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("config.yaml"), docstring(step)).unwrap();
}

/// A step's docstring, with cucumber's leading newline taken off.
fn docstring(step: &Step) -> String {
    let body = step.docstring.clone().expect("step needs a docstring");
    body.strip_prefix('\n').map(str::to_string).unwrap_or(body)
}

// ---- actions ----------------------------------------------------------------

#[when("the manifest is deleted")]
fn delete_manifest(world: &mut BosunWorld) {
    let p = world.path();
    fs::remove_file(p.join("bosun.yaml")).unwrap();
}

#[when(regex = r"^I run `bosun (.+)`$")]
fn i_run(world: &mut BosunWorld, cmdline: String) {
    // run from the project directory, so a relative path like
    // `./bosun.yaml` names this project's manifest.
    let p = world.path();
    run_bosun(world, &cmdline, &p);
}

#[when(regex = r"^I run `bosun (.+)` in an empty directory$")]
fn i_run_elsewhere(world: &mut BosunWorld, cmdline: String) {
    let empty = world.path().join("elsewhere");
    fs::create_dir_all(&empty).unwrap();
    run_bosun(world, &cmdline, &empty);
}

/// I run `bosun <cmdline>` from a terminal and type `answer` at its prompt.
/// A terminal has one output stream, so stdout and stderr arrive interleaved,
/// both as `stdout`.
#[cfg(unix)]
#[when(regex = r#"^I run `bosun (.+)` at a terminal and answer "(.*)"$"#)]
fn run_at_terminal(world: &mut BosunWorld, cmdline: String, answer: String) {
    use rexpect::process::WaitStatus;
    let p = world.path();
    let command = bosun_command(world, &cmdline, &p);
    let mut session =
        rexpect::session::spawn_command(command, Some(30_000)).expect("run bosun in a pty");
    let prompt = "[y/N] ";
    let asked = session
        .exp_string(prompt)
        .unwrap_or_else(|e| panic!("bosun asked nothing: {e}"));
    session.send_line(&answer).expect("answer the prompt");
    let rest = session.exp_eof().expect("bosun exits");
    world.code = match session.process().wait().expect("wait for bosun") {
        WaitStatus::Exited(_, code) => code,
        other => panic!("bosun ended {other:?}"),
    };
    world.stdout = format!("{asked}{prompt}{rest}");
    world.stderr = String::new();
}

/// The `bosun` invocation for `cmdline`, run from `cwd` against this
/// scenario's fixture, state and config.
fn bosun_command(world: &mut BosunWorld, cmdline: &str, cwd: &Path) -> Command {
    let p = world.path();
    fs::write(p.join("fixture.json"), world.fixture().to_string()).unwrap();
    let args: Vec<String> = words(cmdline)
        .into_iter()
        // a bare `.` means "this project directory".
        .map(|a| if a == "." { p.display().to_string() } else { a })
        .collect();
    let mut command = Command::new(env!("CARGO_BIN_EXE_bosun"));
    command
        .env("BOSUN_FIXTURE", p.join("fixture.json"))
        .env("BOSUN_STATE_DIR", p.join("state"))
        // the user config is this scenario's own, never the developer's.
        .env("XDG_CONFIG_HOME", p.join("config"))
        .env("APPDATA", p.join("config"))
        .current_dir(cwd)
        .args(&args);
    command
}

fn run_bosun(world: &mut BosunWorld, cmdline: &str, cwd: &Path) {
    let output = bosun_command(world, cmdline, cwd)
        .output()
        .expect("run bosun");
    world.code = output.status.code().unwrap_or(-1);
    world.stdout = String::from_utf8_lossy(&output.stdout).to_string();
    world.stderr = String::from_utf8_lossy(&output.stderr).to_string();
}

/// `cmdline` split at spaces, with a double-quoted word kept whole and its
/// quotes taken off, as a shell would.
fn words(cmdline: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut quoted = false;
    for c in cmdline.chars() {
        match c {
            '"' => quoted = !quoted,
            ' ' if !quoted => {
                if !word.is_empty() {
                    words.push(std::mem::take(&mut word));
                }
            }
            c => word.push(c),
        }
    }
    if !word.is_empty() {
        words.push(word);
    }
    words
}

// ---- outcomes ---------------------------------------------------------------

fn assert_exit(world: &BosunWorld, code: i32) {
    assert_eq!(
        world.code, code,
        "exit {}; stdout:\n{}\nstderr:\n{}",
        world.code, world.stdout, world.stderr
    );
}

#[then("it succeeds")]
fn succeeds(world: &mut BosunWorld) {
    assert_exit(world, 0);
}

#[then("it fails")]
fn fails(world: &mut BosunWorld) {
    assert_exit(world, 1);
}

/// The command line itself is refused (exit 2), before anything runs.
#[then("it refuses the command line")]
fn refuses_command_line(world: &mut BosunWorld) {
    assert_exit(world, 2);
}

#[then("it reports changes pending")]
fn changes_pending(world: &mut BosunWorld) {
    assert_exit(world, 2);
}

#[then(expr = "it says {string}")]
fn says(world: &mut BosunWorld, needle: String) {
    assert!(world.stdout.contains(&needle), "stdout:\n{}", world.stdout);
}

#[then(expr = "it does not say {string}")]
fn does_not_say(world: &mut BosunWorld, needle: String) {
    assert!(!world.stdout.contains(&needle), "stdout:\n{}", world.stdout);
}

#[then("it prints a JSON document")]
fn prints_json(world: &mut BosunWorld) {
    if let Err(e) = serde_json::from_str::<Value>(&world.stdout) {
        panic!("{e}; stdout:\n{}", world.stdout);
    }
}

#[then(regex = r"^bosun has recorded (\S+) (\S+)$")]
fn has_recorded(world: &mut BosunWorld, pkg: String, version: String) {
    let found = world.state_docs().iter().any(|doc| {
        doc["packages"].as_array().into_iter().flatten().any(|p| {
            p["name"] == pkg.as_str()
                && p["targets"]
                    .as_object()
                    .into_iter()
                    .flat_map(|t| t.values())
                    .any(|leaf| leaf["installed"] == version.as_str())
        })
    });
    let state = world.state_text();
    assert!(found, "want {pkg} {version} in state:\n{state}");
}

#[then(regex = r"^bosun records (\S+) as (held|not held)$")]
fn records_held(world: &mut BosunWorld, pkg: String, held: String) {
    let want = held == "held";
    let found = world.state_docs().iter().any(|doc| {
        doc["packages"].as_array().into_iter().flatten().any(|p| {
            p["name"] == pkg.as_str()
                && p["targets"]
                    .as_object()
                    .into_iter()
                    .flat_map(|t| t.values())
                    .any(|leaf| leaf["held"] == want)
        })
    });
    let state = world.state_text();
    assert!(found, "want {pkg} {held} in state:\n{state}");
}

/// The JSON plan's actions, each as `<action> <name>`, in order.
#[then(regex = r"^the plan's actions are (.*)$")]
fn plan_actions(world: &mut BosunWorld, want: String) {
    let plan: Value = serde_json::from_str(&world.stdout)
        .unwrap_or_else(|e| panic!("{e}; stdout:\n{}", world.stdout));
    let got: Vec<String> = plan["actions"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|a| {
            format!(
                "\"{} {}\"",
                a["action"].as_str().unwrap_or_default(),
                a["name"].as_str().unwrap_or_default()
            )
        })
        .collect();
    let got = if got.is_empty() {
        "none".to_string()
    } else {
        got.join(", ")
    };
    assert_eq!(got, want, "stdout:\n{}", world.stdout);
}

#[then(regex = r"^bosun has recorded nothing about (\S+)$")]
fn recorded_nothing(world: &mut BosunWorld, pkg: String) {
    let state = world.state_text();
    assert!(!state.contains(&pkg), "state:\n{state}");
}

/// This project's manifest as bosun records it in a claim: absolute, canonical.
fn this_manifest(world: &mut BosunWorld) -> String {
    let p = world.path();
    fs::canonicalize(p.join("bosun.yaml"))
        .unwrap()
        .display()
        .to_string()
}

#[then(regex = r#"^it says the claim of this project's manifest with spec "(.+)"$"#)]
fn says_this_claim(world: &mut BosunWorld, spec: String) {
    let line = format!("claimed by {} ({spec})", this_manifest(world));
    assert!(
        world.stdout.contains(&line),
        "want {line:?} in:\n{}",
        world.stdout
    );
}

/// The `claimed_by` array of package `pkg` in a JSON `status` output.
fn json_claims(world: &BosunWorld, pkg: &str) -> Vec<Value> {
    let doc: Value = serde_json::from_str(&world.stdout)
        .unwrap_or_else(|e| panic!("{e}; stdout:\n{}", world.stdout));
    let found = doc["packages"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|p| p["name"] == pkg)
        .unwrap_or_else(|| panic!("no {pkg} in:\n{}", world.stdout));
    found["claimed_by"]
        .as_array()
        .unwrap_or_else(|| panic!("{pkg} has no claimed_by array:\n{}", world.stdout))
        .clone()
}

fn assert_json_claim(world: &BosunWorld, pkg: &str, manifest: &str, spec: &str, exists: bool) {
    let claims = json_claims(world, pkg);
    let want = json!({"manifest": manifest, "spec": spec, "manifest_exists": exists});
    assert!(
        claims.contains(&want),
        "want {want} among {pkg}'s claims:\n{}",
        world.stdout
    );
}

#[then(regex = r#"^the JSON for (\S+) claims "(.+)" with "(.+)", manifest (gone|present)$"#)]
fn json_claims_other(
    world: &mut BosunWorld,
    pkg: String,
    manifest: String,
    spec: String,
    state: String,
) {
    assert_json_claim(world, &pkg, &manifest, &spec, state == "present");
}

#[then(
    regex = r#"^the JSON for (\S+) claims this project's manifest with "(.+)", manifest (gone|present)$"#
)]
fn json_claims_this(world: &mut BosunWorld, pkg: String, spec: String, state: String) {
    let manifest = this_manifest(world);
    assert_json_claim(world, &pkg, &manifest, &spec, state == "present");
}

#[then(regex = r"^the JSON for (\S+) has no claims$")]
fn json_no_claims(world: &mut BosunWorld, pkg: String) {
    assert_eq!(
        json_claims(world, &pkg),
        Vec::<Value>::new(),
        "{}",
        world.stdout
    );
}

#[then(regex = r#"^the JSON for (\S+) says scope "(\S+)", origin "(\S+)", installed "(\S+)"$"#)]
fn json_managed(
    world: &mut BosunWorld,
    pkg: String,
    scope: String,
    origin: String,
    installed: String,
) {
    let doc: Value = serde_json::from_str(&world.stdout).unwrap();
    let found = doc["packages"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|p| p["name"] == pkg.as_str())
        .unwrap_or_else(|| panic!("no {pkg} in:\n{}", world.stdout));
    assert_eq!(found["scope"], scope.as_str(), "{found}");
    assert_eq!(found["origin"], origin.as_str(), "{found}");
    assert_eq!(found["installed"], installed.as_str(), "{found}");
}

#[then(regex = r#"^the claim names this project's manifest by its absolute path, not "(.+)"$"#)]
fn claim_is_absolute(world: &mut BosunWorld, relative: String) {
    let p = world.path();
    let abs = fs::canonicalize(p.join("bosun.yaml")).unwrap();
    let state = world.state_text();
    let quoted = format!("\"{}\"", abs.display());
    assert!(state.contains(&quoted), "want {quoted} in state:\n{state}");
    assert!(!state.contains(&relative), "state:\n{state}");
}

#[then(expr = "it warns {string}")]
fn warns(world: &mut BosunWorld, needle: String) {
    assert!(world.stderr.contains(&needle), "stderr:\n{}", world.stderr);
}

#[then(expr = "it does not warn {string}")]
fn does_not_warn(world: &mut BosunWorld, needle: String) {
    assert!(!world.stderr.contains(&needle), "stderr:\n{}", world.stderr);
}

#[then(expr = "it reports {string}")]
fn reports(world: &mut BosunWorld, needle: String) {
    let all = format!("{}{}", world.stdout, world.stderr);
    assert!(all.contains(&needle), "output:\n{all}");
}

#[then(expr = "it does not report {string}")]
fn does_not_report(world: &mut BosunWorld, needle: String) {
    let all = format!("{}{}", world.stdout, world.stderr);
    assert!(!all.contains(&needle), "output:\n{all}");
}

#[then(expr = "it asked {string}")]
fn asked(world: &mut BosunWorld, question: String) {
    assert!(
        world.stdout.contains(&question),
        "terminal:\n{}",
        world.stdout
    );
}

/// The words after `id` on its line of `bosun backends` text output.
fn backends_line(world: &BosunWorld, id: &str) -> String {
    world
        .stdout
        .lines()
        .find_map(|line| {
            let mut words = line.split_whitespace();
            (words.next() == Some(id)).then(|| words.collect::<Vec<_>>().join(" "))
        })
        .unwrap_or_else(|| panic!("{id} is not listed:\n{}", world.stdout))
}

/// The `libc` member of a `--format json` document.
fn json_libc(world: &BosunWorld) -> Value {
    let doc: Value = serde_json::from_str(&world.stdout)
        .unwrap_or_else(|e| panic!("{e}; stdout:\n{}", world.stdout));
    doc.get("libc")
        .cloned()
        .unwrap_or_else(|| panic!("no libc in:\n{}", world.stdout))
}

#[then(regex = r"^the JSON reports libc (\S+) (\S+)$")]
fn json_reports_libc(world: &mut BosunWorld, family: String, version: String) {
    assert_eq!(
        json_libc(world),
        json!({"family": family, "version": version}),
        "{}",
        world.stdout
    );
}

#[then("the JSON reports no libc")]
fn json_reports_no_libc(world: &mut BosunWorld) {
    assert_eq!(json_libc(world), Value::Null, "{}", world.stdout);
}

#[then(regex = r"^it lists (\S+) (\S+) as registered$")]
fn lists_registered(world: &mut BosunWorld, id: String, version: String) {
    assert_eq!(backends_line(world, &id), version, "{}", world.stdout);
}

#[then(regex = r"^it lists (\S+) as skipped: (.+)$")]
fn lists_skipped(world: &mut BosunWorld, id: String, reason: String) {
    assert_eq!(
        backends_line(world, &id),
        format!("skipped: {reason}"),
        "{}",
        world.stdout
    );
}

/// Every step of every action in a `--format json` plan.
fn plan_steps(world: &BosunWorld) -> Vec<Value> {
    let doc: Value = serde_json::from_str(&world.stdout)
        .unwrap_or_else(|e| panic!("{e}; stdout:\n{}", world.stdout));
    doc["actions"]
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|a| a["steps"].as_array().cloned().unwrap_or_default())
        .collect()
}

#[then(regex = r"^the plan downloads (\S+) checked against the sha256 (\S+) lists for it$")]
fn plan_downloads_checked(world: &mut BosunWorld, asset: String, sums: String) {
    let listed = world
        .releases
        .values()
        .flat_map(|r| &r.assets)
        .filter(|(name, _)| *name == sums)
        .flat_map(|(_, body)| {
            String::from_utf8_lossy(body)
                .lines()
                .filter_map(|l| l.split_once("  "))
                .filter(|(_, name)| *name == asset)
                .map(|(sha, _)| sha.to_string())
                .collect::<Vec<_>>()
        })
        .next()
        .unwrap_or_else(|| panic!("no {sums} lists {asset}"));
    let steps = plan_steps(world);
    let fetch = steps
        .iter()
        .find(|s| {
            s["step"] == "fetch"
                && s["url"]
                    .as_str()
                    .is_some_and(|u| u.ends_with(&format!("/{asset}")))
        })
        .unwrap_or_else(|| panic!("the plan downloads no {asset}:\n{}", world.stdout));
    assert_eq!(fetch["sha256"], listed.as_str(), "{fetch}");
}

#[then(regex = r"^the plan puts (\S+) in ~/(\S+)$")]
fn plan_puts_in(world: &mut BosunWorld, binary: String, dir: String) {
    let want = world.home().join(dir).join(&binary).display().to_string();
    let steps = plan_steps(world);
    assert!(
        steps.iter().any(|s| s["step"] == "extract"
            && s["member"] == binary.as_str()
            && s["dest"] == want.as_str()),
        "want {binary} extracted to {want}:\n{}",
        world.stdout
    );
}

#[then(regex = r"^(\S+) is installed from (\S+)$")]
fn installed_from(world: &mut BosunWorld, binary: String, asset: String) {
    let path = world.bin_dir().join(&binary);
    let got = fs::read(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    assert_eq!(
        String::from_utf8_lossy(&got),
        String::from_utf8_lossy(&binary_bytes(&asset))
    );
    assert_executable(&path);
}

#[then(regex = r"^(\S+) is not installed$")]
fn not_installed(world: &mut BosunWorld, binary: String) {
    let path = world.bin_dir().join(&binary);
    assert!(!path.exists(), "{} exists", path.display());
}

#[cfg(unix)]
fn assert_executable(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let mode = fs::metadata(path).unwrap().permissions().mode();
    assert_eq!(mode & 0o111, 0o111, "{} mode {mode:o}", path.display());
}

/// Windows has no mode bits; an installed `.exe` is executable as written.
#[cfg(not(unix))]
fn assert_executable(_path: &Path) {}

#[then("the manifest reads:")]
fn manifest_reads(world: &mut BosunWorld, step: &Step) {
    let p = world.path();
    let got = fs::read_to_string(p.join("bosun.yaml")).unwrap();
    assert_eq!(
        got.trim_end(),
        docstring(step).trim_end(),
        "bosun.yaml:\n{got}"
    );
}

#[then(regex = r"^the file (\S+) reads:$")]
fn file_reads(world: &mut BosunWorld, step: &Step, name: String) {
    let got =
        fs::read_to_string(world.path().join(&name)).unwrap_or_else(|e| panic!("{name}: {e}"));
    assert_eq!(got.trim_end(), docstring(step).trim_end(), "{name}:\n{got}");
}

#[then("the manifest is unchanged")]
fn manifest_unchanged(world: &mut BosunWorld) {
    let p = world.path();
    let got = fs::read_to_string(p.join("bosun.yaml")).unwrap();
    let want = world.manifest.clone().expect("a manifest was given");
    assert_eq!(got, want, "bosun.yaml changed");
}

#[then("no manifest is written")]
fn manifest_absent(world: &mut BosunWorld) {
    let p = world.path();
    assert!(!p.join("bosun.yaml").exists(), "bosun.yaml was written");
}

#[then(regex = r"^the lock does not mention (\S+)$")]
fn lock_omits(world: &mut BosunWorld, pkg: String) {
    let (text, doc) = world.lock();
    assert!(
        !lock_leaves(&doc).any(|(name, _)| name == pkg.as_str()),
        "{pkg} in the lock:\n{text}"
    );
}

#[then("a lock is written")]
fn lock_written(world: &mut BosunWorld) {
    let p = world.path();
    assert_eq!(lock_files(&p).len(), 1, "no lock in the state dir");
    assert!(
        !p.join("bosun.lock").exists(),
        "a lock was written into the project"
    );
}

#[then("no lock is written")]
fn lock_absent(world: &mut BosunWorld) {
    let p = world.path();
    assert!(lock_files(&p).is_empty(), "a lock was written");
}

fn lock_leaves(doc: &Value) -> impl Iterator<Item = (&Value, &Value)> {
    doc["packages"]
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|p| {
            p["targets"]
                .as_object()
                .into_iter()
                .flat_map(|t| t.values())
                .map(move |leaf| (&p["name"], leaf))
        })
}

#[then(regex = r"^the lock pins (\S+) (\S+)$")]
fn lock_pins(world: &mut BosunWorld, pkg: String, version: String) {
    let (text, doc) = world.lock();
    assert!(
        lock_leaves(&doc)
            .any(|(name, leaf)| name == pkg.as_str() && leaf["resolved"] == version.as_str()),
        "want {pkg} {version} in the lock:\n{text}"
    );
}

#[then(regex = r"^the lock identifies (\S+) as (\S+)$")]
fn lock_identifies(world: &mut BosunWorld, pkg: String, purl: String) {
    let (text, doc) = world.lock();
    assert!(
        lock_leaves(&doc).any(|(name, leaf)| name == pkg.as_str() && leaf["purl"] == purl.as_str()),
        "want {pkg} as {purl} in the lock:\n{text}"
    );
}

#[then(regex = r"^the lock records a checksum for (\S+)$")]
fn lock_has_checksum(world: &mut BosunWorld, pkg: String) {
    let (text, doc) = world.lock();
    assert!(
        lock_leaves(&doc).any(|(name, leaf)| name == pkg.as_str() && leaf["checksum"].is_string()),
        "want a checksum for {pkg} in the lock:\n{text}"
    );
}

#[then(regex = r"^the lock records the checksum (\S+)$")]
fn lock_checksum(world: &mut BosunWorld, checksum: String) {
    let (text, doc) = world.lock();
    assert!(
        lock_leaves(&doc).any(|(_, leaf)| leaf["checksum"] == checksum.as_str()),
        "want checksum {checksum} in the lock:\n{text}"
    );
}

// ---- render -----------------------------------------------------------------

/// A distro mirror's suite lists a package in its `main` index for an arch.
#[given(regex = r"^(Debian|Ubuntu) (\S+) lists (\S+) (\S+) for (amd64|arm64)$")]
fn mirror_lists(
    world: &mut BosunWorld,
    distro: String,
    suite: String,
    pkg: String,
    version: String,
    arch: String,
) {
    world
        .apt_mirror
        .entry((distro.to_lowercase(), suite, arch))
        .or_default()
        .push((pkg, version));
}

/// A flat apt repository (a vendor's, added by a Dockerfile) lists a package.
#[given(regex = r"^the vendor repository (\S+) lists (\S+) (\S+)$")]
fn vendor_lists(world: &mut BosunWorld, url: String, pkg: String, version: String) {
    world
        .vendor_repos
        .entry(url)
        .or_default()
        .push((pkg, version));
}

/// A file of the target machine's root filesystem, under `target/` in the
/// project directory (`render --rootfs target`).
#[given(regex = r"^the target's file (\S+):$")]
fn target_file(world: &mut BosunWorld, path: String, step: &Step) {
    let file = world.path().join("target").join(path);
    fs::create_dir_all(file.parent().unwrap()).unwrap();
    fs::write(file, docstring(step)).unwrap();
}

/// The npm registry's or PyPI's metadata for a package lists these versions,
/// served over the canned HTTP to the compiled-in backends' remote indexes.
#[given(regex = r"^(the npm registry|PyPI) lists (\S+) (.+)$")]
fn registry_lists(world: &mut BosunWorld, registry: String, pkg: String, list: String) {
    let listed = versions(&list);
    let (url, doc) = if registry == "PyPI" {
        let releases: serde_json::Map<String, Value> = listed
            .into_iter()
            .map(|v| (v, json!([{"yanked": false}])))
            .collect();
        (
            format!("https://pypi.org/pypi/{pkg}/json"),
            json!({ "releases": releases }),
        )
    } else {
        let versions: serde_json::Map<String, Value> =
            listed.into_iter().map(|v| (v, json!({}))).collect();
        (
            format!("https://registry.npmjs.org/{}", pkg.replace('/', "%2F")),
            json!({ "versions": versions }),
        )
    };
    world.registry.insert(url, doc);
}

#[then("it prints:")]
fn prints(world: &mut BosunWorld, step: &Step) {
    let want = docstring(step);
    assert_eq!(
        world.stdout.trim_end(),
        want.trim_end(),
        "stderr:\n{}",
        world.stderr
    );
}

/// The versions file `render json` prints: lock-shaped, each package's
/// rendered value per target platform.
#[then(regex = r"^the versions file gives (\S+) (\S+) as (\S+) from (\S+) for (\S+)$")]
fn versions_file_gives(
    world: &mut BosunWorld,
    name: String,
    version: String,
    package: String,
    backend: String,
    target: String,
) {
    let doc: Value = serde_json::from_str(&world.stdout).expect("render json prints JSON");
    let entry = doc["packages"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|p| p["name"] == name.as_str())
        .unwrap_or_else(|| panic!("no {name} in:\n{}", world.stdout));
    let leaf = &entry["targets"][target.as_str()];
    assert_eq!(leaf["version"], version.as_str(), "{}", world.stdout);
    assert_eq!(leaf["package"], package.as_str(), "{}", world.stdout);
    assert_eq!(leaf["backend"], backend.as_str(), "{}", world.stdout);
}

fn main() {
    // An undefined step skips its scenario; that must fail the suite, not pass it.
    // Scenarios tagged @pty drive a pseudo-terminal, which only unix has.
    futures::executor::block_on(
        BosunWorld::cucumber()
            .fail_on_skipped()
            .filter_run_and_exit("tests/acceptance/features", |feature, _, scenario| {
                cfg!(unix)
                    || !feature
                        .tags
                        .iter()
                        .chain(&scenario.tags)
                        .any(|t| t == "pty")
            }),
    );
}
