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
    /// `(id, codename)` for a named distro.
    distro: Option<(String, String)>,
    elevated: bool,
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
        "apt" => BackendMeta {
            purl_type: "deb",
            vers_type: "deb",
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
    fail_step: Option<String>,
    /// GitHub releases by `(owner/repo, tag)`.
    releases: BTreeMap<(String, String), Release>,
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
                "real": b.real,
            })).collect::<Vec<_>>(),
            "http": self.http(),
            "bin_dir": self.bin_dir(),
        });
        if let Some((id, codename)) = &m.distro {
            fx["distro"] = json!({"id": id, "codename": codename});
        }
        if let Some(f) = &self.fail_step {
            fx["fail_step"] = json!(f);
        }
        fx
    }

    /// The scratch bin dir bosun's own installer writes into.
    fn bin_dir(&self) -> PathBuf {
        self.dir.as_ref().expect("a temp dir").path().join("bin")
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
        http
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
        let p = self.path();
        let text = fs::read_to_string(p.join("bosun.lock")).expect("read bosun.lock");
        let doc = serde_json::from_str(&text).expect("bosun.lock is JSON");
        (text, doc)
    }
}

fn versions(list: &str) -> Vec<String> {
    list.replace(" and ", ", ")
        .split(", ")
        .map(|v| v.trim().to_string())
        .collect()
}

// ---- the machine and its backends -----------------------------------------

#[given(regex = r"^an? (Linux|Debian \S+|Windows) (\S+) machine, (not elevated|elevated)$")]
fn a_machine(world: &mut BosunWorld, os: String, arch: String, privilege: String) {
    let distro = os
        .strip_prefix("Debian ")
        .map(|codename| ("debian".to_string(), codename.to_string()));
    world.machine = Some(Machine {
        os: if os == "Windows" { "windows" } else { "linux" }.into(),
        arch,
        distro,
        elevated: privilege == "elevated",
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
        real: false,
    });
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
    world.release_mut(&repo, &tag).assets.push((asset, archive));
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

#[given(regex = r"^installing (\S+) fails$")]
fn installing_fails(world: &mut BosunWorld, pkg: String) {
    world.fail_step = Some(pkg);
}

/// Seed bosun's own record of a package: `installed` (bosun put it there) or
/// `accepted` (found present and adopted), claimed by this project's manifest
/// or by another project's.
#[given(
    regex = r#"^bosun (installed|accepted) (\S+) (\S+) for (this|another) project wanting "(.+)"$"#
)]
fn bosun_recorded(
    world: &mut BosunWorld,
    origin: String,
    pkg: String,
    version: String,
    whose: String,
    spec: String,
) {
    let p = world.path();
    let m = world.machine().clone();
    let b = world.backend_for(&pkg).clone();
    let purl = match b.meta.purl_type {
        "deb" => {
            let (distro, codename) = m.distro.clone().expect("a deb package needs a distro");
            format!("pkg:deb/{distro}/{pkg}?arch={}&distro={codename}", m.arch)
        }
        t => format!("pkg:{t}/{pkg}"),
    };
    let manifest = if whose == "this" {
        p.join("bosun.yaml").display().to_string()
    } else {
        "/other/bosun.yaml".to_string()
    };
    let entry = json!({
        "name": pkg,
        "targets": {
            format!("{}/{}", m.os, m.arch): {
                "backend": b.id,
                "purl": purl,
                "installed": version,
                "origin": origin,
                "claimedBy": [{"manifest": manifest, "spec": format!("vers:{}/{spec}", b.meta.vers_type)}],
            }
        }
    });
    let scope = b.meta.scope.to_string();
    let packages = world.seeded.entry(scope.clone()).or_default();
    packages.push(entry);
    let doc = json!({"stateVersion": 1, "packages": packages});
    let dir = p.join("state").join(scope);
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("state.json"), doc.to_string()).unwrap();
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
    let p = world.path();
    fs::write(p.join("fixture.json"), world.fixture().to_string()).unwrap();
    let args: Vec<String> = words(&cmdline)
        .into_iter()
        // a bare `.` means "this project directory".
        .map(|a| if a == "." { p.display().to_string() } else { a })
        .collect();
    let output = Command::new(env!("CARGO_BIN_EXE_bosun"))
        .env("BOSUN_FIXTURE", p.join("fixture.json"))
        .env("BOSUN_STATE_DIR", p.join("state"))
        // the user config is this scenario's own, never the developer's.
        .env("XDG_CONFIG_HOME", p.join("config"))
        .env("APPDATA", p.join("config"))
        // run from the project directory, so a relative path like
        // `./bosun.yaml` names this project's manifest.
        .current_dir(&p)
        .args(&args)
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

#[then(regex = r"^bosun has recorded nothing about (\S+)$")]
fn recorded_nothing(world: &mut BosunWorld, pkg: String) {
    let state = world.state_text();
    assert!(!state.contains(&pkg), "state:\n{state}");
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

#[then(regex = r"^the lock does not mention (\S+)$")]
fn lock_omits(world: &mut BosunWorld, pkg: String) {
    let (text, doc) = world.lock();
    assert!(
        !lock_leaves(&doc).any(|(name, _)| name == pkg.as_str()),
        "{pkg} in bosun.lock:\n{text}"
    );
}

#[then("a lock is written")]
fn lock_written(world: &mut BosunWorld) {
    let p = world.path();
    assert!(p.join("bosun.lock").exists(), "no bosun.lock");
}

#[then("no lock is written")]
fn lock_absent(world: &mut BosunWorld) {
    let p = world.path();
    assert!(!p.join("bosun.lock").exists(), "bosun.lock exists");
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
        "want {pkg} {version} in bosun.lock:\n{text}"
    );
}

#[then(regex = r"^the lock records the checksum (\S+)$")]
fn lock_checksum(world: &mut BosunWorld, checksum: String) {
    let (text, doc) = world.lock();
    assert!(
        lock_leaves(&doc).any(|(_, leaf)| leaf["checksum"] == checksum.as_str()),
        "want checksum {checksum} in bosun.lock:\n{text}"
    );
}

fn main() {
    // An undefined step skips its scenario; that must fail the suite, not pass it.
    futures::executor::block_on(
        BosunWorld::cucumber()
            .fail_on_skipped()
            .run_and_exit("tests/acceptance/features"),
    );
}
