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

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;
use std::process::Command;

use cucumber::gherkin::Step;
use cucumber::{given, then, when, World};
use serde_json::{json, Value};
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
}

#[derive(Debug, Default, World)]
struct BosunWorld {
    dir: Option<TempDir>,
    machine: Option<Machine>,
    backends: Vec<Backend>,
    fail_step: Option<String>,
    /// Seeded state packages per scope (`system` / `user`).
    seeded: BTreeMap<String, Vec<Value>>,
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
            })).collect::<Vec<_>>(),
        });
        if let Some((id, codename)) = &m.distro {
            fx["distro"] = json!({"id": id, "codename": codename});
        }
        if let Some(f) = &self.fail_step {
            fx["fail_step"] = json!(f);
        }
        fx
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

#[given(regex = r"^an? (Linux|Debian \S+) (\S+) machine, (not elevated|elevated)$")]
fn a_machine(world: &mut BosunWorld, os: String, arch: String, privilege: String) {
    let distro = os
        .strip_prefix("Debian ")
        .map(|codename| ("debian".to_string(), codename.to_string()));
    world.machine = Some(Machine {
        os: "linux".into(),
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
    });
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
    let body = step.docstring.clone().expect("step needs a docstring");
    fs::write(p.join("bosun.yaml"), body).unwrap();
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
    let args: Vec<String> = cmdline
        .split_whitespace()
        // a bare `.` means "this project directory".
        .map(|a| {
            if a == "." {
                p.display().to_string()
            } else {
                a.to_string()
            }
        })
        .collect();
    let output = Command::new(env!("CARGO_BIN_EXE_bosun"))
        .env("BOSUN_FIXTURE", p.join("fixture.json"))
        .env("BOSUN_STATE_DIR", p.join("state"))
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
    futures::executor::block_on(BosunWorld::cucumber().run_and_exit("tests/acceptance/features"));
}
