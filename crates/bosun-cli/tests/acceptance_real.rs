//! Real-backend acceptance (§11, tier 2): runs the real `bosun` binary against
//! real apt / dnf / apk / npm / uv / github-release, **mutating the container**. Guarded
//! twice — it refuses to run unless `BOSUN_ACCEPTANCE_REAL=1` *and* a container marker is
//! present — so `cargo test --workspace` (which builds and runs this binary)
//! stays hermetic and a developer cannot run it on a workstation by mistake.
//! Run it with `just acceptance-real` inside a disposable container.
//!
//! State is redirected via `HOME` (user scope + the github bin dir land in a
//! temp dir); system-scope apt/dnf/apk state lives at its real
//! `/var/lib/bosun` path, cleaned at the start of each system scenario. Each
//! tag names a program the scenario needs on `PATH` (`@apt-get`, `@dnf`,
//! `@dnf-3` for dnf4, `@apk`, `@npm`, `@uv`, `@useradd @runuser`); it runs
//! only where all are.

use std::fs;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use cucumber::gherkin::Step;
use cucumber::{given, then, when, World};
use tempfile::TempDir;

#[derive(Debug, Default, World)]
struct RealWorld {
    home: Option<TempDir>,
    /// Whether this scenario set up the local apt repository, which goes
    /// once it ends.
    local_repo: bool,
    /// Whether this scenario changed dnf's repositories, which are put back
    /// once it ends.
    dnf_repos: bool,
    code: i32,
    stdout: String,
    stderr: String,
}

impl RealWorld {
    fn home(&mut self) -> PathBuf {
        self.home
            .get_or_insert_with(|| tempfile::tempdir().unwrap())
            .path()
            .to_path_buf()
    }
    fn bin_dir(&mut self) -> PathBuf {
        self.home().join(".local").join("bin")
    }
}

fn run(cmd: &str, args: &[&str]) -> (i32, String) {
    let out = Command::new(cmd)
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("{cmd}: {e}"));
    let mut text = String::from_utf8_lossy(&out.stdout).to_string();
    text.push_str(&String::from_utf8_lossy(&out.stderr));
    (out.status.code().unwrap_or(-1), text)
}

#[given("the apt index is refreshed")]
fn apt_refresh(_w: &mut RealWorld) {
    let (code, out) = run("apt-get", &["update"]);
    assert_eq!(code, 0, "apt-get update failed: {out}");
}

#[given("the system state is clean")]
fn clean(_w: &mut RealWorld) {
    let _ = fs::remove_file("/var/lib/bosun/state.json");
    let _ = run("apt-mark", &["unhold", "hello"]);
    let _ = run("apt-get", &["remove", "-y", "hello"]);
}

#[given(expr = "the system state is clean and apt has no {word}")]
fn clean_apt(_w: &mut RealWorld, pkg: String) {
    let _ = fs::remove_file("/var/lib/bosun/state.json");
    let _ = run("apt-mark", &["unhold", &pkg]);
    let _ = run("apt-get", &["remove", "-y", &pkg]);
}

#[given("the dnf index is refreshed")]
fn dnf_refresh(_w: &mut RealWorld) {
    let (code, out) = run("dnf", &["makecache"]);
    assert_eq!(code, 0, "dnf makecache failed: {out}");
}

/// dnf4 ships versionlock as a plugin; dnf5 has it built in.
const VERSIONLOCK_PLUGIN: &str = "python3-dnf-plugin-versionlock";

#[given("dnf has a versionlock command")]
fn dnf_versionlock(_w: &mut RealWorld) {
    if run("dnf", &["versionlock", "list"]).0 != 0 {
        let (code, out) = run("dnf", &["install", "-y", VERSIONLOCK_PLUGIN]);
        assert_eq!(code, 0, "installing {VERSIONLOCK_PLUGIN}: {out}");
    }
}

#[given("dnf has no versionlock command")]
fn dnf_no_versionlock(_w: &mut RealWorld) {
    let _ = run("dnf", &["remove", "-y", VERSIONLOCK_PLUGIN]);
    assert_ne!(
        run("dnf", &["versionlock", "list"]).0,
        0,
        "versionlock remains"
    );
}

#[given(expr = "the system state is clean and dnf has no {word}")]
fn clean_dnf(_w: &mut RealWorld, pkg: String) {
    let _ = fs::remove_file("/var/lib/bosun/state.json");
    let _ = run("dnf", &["versionlock", "delete", &pkg]);
    let _ = run("dnf", &["remove", "-y", &pkg]);
}

#[given("the apk index is refreshed")]
fn apk_refresh(_w: &mut RealWorld) {
    let (code, out) = run("apk", &["update"]);
    assert_eq!(code, 0, "apk update failed: {out}");
}

#[given(expr = "the system state is clean of {word} installed through apk")]
fn apk_clean(_w: &mut RealWorld, pkg: String) {
    let _ = fs::remove_file("/var/lib/bosun/state.json");
    let _ = run("apk", &["del", &pkg]);
}

#[then(expr = "apk reports {word} installed")]
fn apk_installed(_w: &mut RealWorld, pkg: String) {
    let (code, out) = run("apk", &["info", "-e", &pkg]);
    assert_eq!(code, 0, "apk info -e {pkg}: {out}");
}

#[then(expr = "apk reports {word} not installed")]
fn apk_absent(_w: &mut RealWorld, pkg: String) {
    let (code, out) = run("apk", &["info", "-e", &pkg]);
    assert_ne!(code, 0, "apk still reports {pkg} installed: {out}");
}

/// The lines of apk's world file that name `pkg`, with or without a
/// constraint.
fn world_entries(pkg: &str) -> Vec<String> {
    let world = fs::read_to_string("/etc/apk/world").expect("read /etc/apk/world");
    world
        .lines()
        .filter(|l| {
            l.strip_prefix(pkg)
                .is_some_and(|rest| rest.is_empty() || rest.starts_with(['=', '<', '>', '~', '@']))
        })
        .map(str::to_string)
        .collect()
}

#[then(expr = "the apk world pins {word} to its installed version")]
fn world_pins(_w: &mut RealWorld, pkg: String) {
    let (code, installed) = run("apk", &["info", "-e", "-v", &pkg]);
    assert_eq!(code, 0, "apk info -e -v {pkg}: {installed}");
    let version = installed
        .trim()
        .strip_prefix(&format!("{pkg}-"))
        .unwrap_or_else(|| panic!("apk info -e -v {pkg}: {installed}"));
    assert_eq!(world_entries(&pkg), vec![format!("{pkg}={version}")]);
}

#[then(expr = "the apk world does not name {word}")]
fn world_lacks(_w: &mut RealWorld, pkg: String) {
    assert_eq!(world_entries(&pkg), Vec::<String>::new());
}

/// Where the local apt repository lives, and the source list naming it.
const LOCAL_REPO: &str = "/srv/bosun-apt";
const LOCAL_SOURCE: &str = "/etc/apt/sources.list.d/bosun-probe.list";

/// A flat apt repository at [`LOCAL_REPO`] offering `pkg` at `old` and
/// `new`: empty `Architecture: all` packages built with `dpkg-deb`, indexed
/// by hand, trusted, and the index refreshed. Whatever an earlier run left of
/// `pkg` is removed first; the repository goes when the scenario ends.
#[given(regex = r"^a local apt repository offers (\S+) (\S+) and (\S+)$")]
fn local_apt_repo(w: &mut RealWorld, pkg: String, old: String, new: String) {
    use sha2::Digest;
    w.local_repo = true;
    let _ = run("apt-mark", &["unhold", &pkg]);
    let _ = run("apt-get", &["remove", "-y", &pkg]);
    let repo = Path::new(LOCAL_REPO);
    let _ = fs::remove_dir_all(repo);
    fs::create_dir_all(repo).unwrap();
    let mut index = String::new();
    for version in [&old, &new] {
        let root = repo.join(format!("{pkg}-{version}"));
        fs::create_dir_all(root.join("DEBIAN")).unwrap();
        let control = format!(
            "Package: {pkg}\nVersion: {version}\nArchitecture: all\n\
             Maintainer: bosun <bosun@example.invalid>\nDescription: bosun test package\n"
        );
        fs::write(root.join("DEBIAN/control"), &control).unwrap();
        let file = format!("{pkg}_{version}_all.deb");
        let deb = repo.join(&file);
        let (code, out) = run(
            "dpkg-deb",
            &[
                "--root-owner-group",
                "--build",
                &root.display().to_string(),
                &deb.display().to_string(),
            ],
        );
        assert_eq!(code, 0, "dpkg-deb: {out}");
        let bytes = fs::read(&deb).unwrap();
        let sha256 = sha2::Sha256::digest(&bytes)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>();
        index.push_str(&format!(
            "{control}Filename: ./{file}\nSize: {}\nSHA256: {sha256}\n\n",
            bytes.len()
        ));
    }
    fs::write(repo.join("Packages"), index).unwrap();
    fs::write(
        LOCAL_SOURCE,
        format!("deb [trusted=yes] file:{LOCAL_REPO} ./\n"),
    )
    .unwrap();
    let (code, out) = run("apt-get", &["update"]);
    assert_eq!(code, 0, "apt-get update failed: {out}");
}

/// The apt preferences file a scenario pins the local repository's packages
/// in; it goes with the repository.
const LOCAL_PIN: &str = "/etc/apt/preferences.d/bosun-probe";

/// Pin `version` of `pkg` at `priority` (100 is what a NotAutomatic suite
/// such as backports gets; below 0 keeps apt from choosing it), and show
/// apt's resulting policy in the scenario's output.
#[given(regex = r"^apt pins (\S+) (\S+) at priority (-?\d+)$")]
fn apt_pin(w: &mut RealWorld, pkg: String, version: String, priority: i32) {
    w.local_repo = true;
    fs::write(
        LOCAL_PIN,
        format!("Package: {pkg}\nPin: version {version}\nPin-Priority: {priority}\n"),
    )
    .unwrap();
    let (code, out) = run("apt-cache", &["policy", &pkg]);
    assert_eq!(code, 0, "apt-cache policy: {out}");
    eprintln!("{out}");
}

/// Remove the local apt repository, its source and the index apt fetched
/// from it, so apt no longer offers its packages.
fn remove_local_repo() {
    let _ = fs::remove_file(LOCAL_PIN);
    let _ = fs::remove_file(LOCAL_SOURCE);
    let _ = fs::remove_dir_all(LOCAL_REPO);
    let lists = Path::new("/var/lib/apt/lists");
    let prefix = LOCAL_REPO.trim_start_matches('/').replace('/', "_");
    for entry in fs::read_dir(lists).into_iter().flatten().flatten() {
        if entry.file_name().to_string_lossy().starts_with(&prefix) {
            let _ = fs::remove_file(entry.path());
        }
    }
}

#[given("the manifest:")]
#[when("the manifest becomes:")]
fn a_manifest(w: &mut RealWorld, step: &Step) {
    let p = w.home();
    fs::write(
        p.join("bosun.yaml"),
        step.docstring.clone().expect("docstring"),
    )
    .unwrap();
}

#[when("the manifest is deleted")]
fn delete_manifest(w: &mut RealWorld) {
    let p = w.home();
    fs::remove_file(p.join("bosun.yaml")).unwrap();
}

#[given("a non-root user owns the project")]
fn make_user(w: &mut RealWorld) {
    let home = w.home();
    let _ = run("useradd", &["bosuntest"]); // ignore "already exists"
    let (code, out) = run("chown", &["-R", "bosuntest", &home.display().to_string()]);
    assert_eq!(code, 0, "chown: {out}");
}

/// Run bosun as a non-root user (user-scope backends refuse to run elevated,
/// §5.5), with `HOME` pointed at the chowned temp dir.
#[when(regex = r"^the non-root user runs `bosun (.+)`$")]
fn i_run_as_user(w: &mut RealWorld, cmdline: String) {
    let home = w.home();
    let bosun = std::path::PathBuf::from(env!("CARGO_BIN_EXE_bosun"));
    let mut args: Vec<String> = [
        "-u",
        "bosuntest",
        "--",
        "env",
        "-u",
        "XDG_STATE_HOME",
        "-u",
        "XDG_CONFIG_HOME",
        "-u",
        "XDG_DATA_HOME",
        "-u",
        "XDG_CACHE_HOME",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    args.push(format!("HOME={}", home.display()));
    args.push(bosun.display().to_string());
    for a in cmdline.split_whitespace() {
        args.push(if a == "." {
            home.display().to_string()
        } else {
            a.to_string()
        });
    }
    let out = Command::new("runuser")
        .args(&args)
        .output()
        .expect("runuser");
    w.code = out.status.code().unwrap_or(-1);
    w.stdout = String::from_utf8_lossy(&out.stdout).to_string();
    w.stderr = String::from_utf8_lossy(&out.stderr).to_string();
}

#[when(regex = r"^I run `bosun (.+)`$")]
fn i_run(w: &mut RealWorld, cmdline: String) {
    let home = w.home();
    let args: Vec<String> = cmdline
        .split_whitespace()
        .map(|a| {
            if a == "." {
                home.display().to_string()
            } else {
                a.to_string()
            }
        })
        .collect();
    let out = Command::new(env!("CARGO_BIN_EXE_bosun"))
        .current_dir(&home)
        .env("HOME", &home)
        // Keep XDG from redirecting user state away from HOME.
        .env_remove("XDG_STATE_HOME")
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("XDG_DATA_HOME")
        .env_remove("XDG_CACHE_HOME")
        .env_remove("BOSUN_FIXTURE")
        .env_remove("BOSUN_STATE_DIR")
        .args(&args)
        .output()
        .expect("run bosun");
    w.code = out.status.code().unwrap_or(-1);
    w.stdout = String::from_utf8_lossy(&out.stdout).to_string();
    w.stderr = String::from_utf8_lossy(&out.stderr).to_string();
}

fn assert_exit(w: &RealWorld, code: i32) {
    assert_eq!(w.code, code, "stdout:\n{}\nstderr:\n{}", w.stdout, w.stderr);
}

#[then("it succeeds")]
fn succeeds(w: &mut RealWorld) {
    assert_exit(w, 0);
}

#[then("it fails")]
fn fails(w: &mut RealWorld) {
    assert_exit(w, 1);
}

#[then(expr = "it reports {string}")]
fn reports(w: &mut RealWorld, needle: String) {
    let all = format!("{}{}", w.stdout, w.stderr);
    assert!(all.contains(&needle), "output:\n{all}");
}

#[then("it reports changes pending")]
fn changes_pending(w: &mut RealWorld) {
    assert_exit(w, 2);
}

#[then(expr = "it says {string}")]
fn stdout_contains(w: &mut RealWorld, needle: String) {
    assert!(w.stdout.contains(&needle), "stdout:\n{}", w.stdout);
}

#[then(expr = "dpkg reports {word} installed")]
fn dpkg_installed(_w: &mut RealWorld, pkg: String) {
    let (code, out) = run("dpkg-query", &["-W", "-f=${db:Status-Status}", &pkg]);
    assert_eq!(code, 0, "dpkg-query failed: {out}");
    assert_eq!(out.trim(), "installed", "dpkg status: {out}");
}

#[then(expr = "dpkg reports {word} at {word}")]
fn dpkg_version(_w: &mut RealWorld, pkg: String, version: String) {
    let (code, out) = run("dpkg-query", &["-W", "-f=${Version}", &pkg]);
    assert_eq!(code, 0, "dpkg-query failed: {out}");
    assert_eq!(out.trim(), version, "dpkg version: {out}");
}

#[then(expr = "dpkg reports {word} not installed")]
fn dpkg_absent(_w: &mut RealWorld, pkg: String) {
    let (_code, out) = run("dpkg-query", &["-W", "-f=${db:Status-Status}", &pkg]);
    assert_ne!(
        out.trim(),
        "installed",
        "dpkg still reports installed: {out}"
    );
}

#[then(expr = "apt-mark reports {word} held")]
fn held(_w: &mut RealWorld, pkg: String) {
    let (_code, out) = run("apt-mark", &["showhold"]);
    assert!(out.lines().any(|l| l.trim() == pkg), "showhold:\n{out}");
}

#[then(expr = "apt-mark reports {word} not held")]
fn not_held(_w: &mut RealWorld, pkg: String) {
    let (_code, out) = run("apt-mark", &["showhold"]);
    assert!(!out.lines().any(|l| l.trim() == pkg), "showhold:\n{out}");
}

#[then(expr = "rpm reports {word} installed")]
fn rpm_installed(_w: &mut RealWorld, pkg: String) {
    let (code, out) = run("rpm", &["-q", &pkg]);
    assert_eq!(code, 0, "rpm -q {pkg}: {out}");
}

/// The repository file a scenario adds, and where it keeps the repository
/// files it edits, to put them back.
const DNF_TEST_REPO: &str = "/etc/yum.repos.d/bosun-test.repo";
const DNF_REPO_BACKUP: &str = "/var/tmp/bosun-yum.repos.d";

/// Keep a copy of `/etc/yum.repos.d` (once per scenario), so the scenario's
/// changes can be undone.
fn back_up_dnf_repos(w: &mut RealWorld) {
    if !w.dnf_repos {
        w.dnf_repos = true;
        let _ = fs::remove_dir_all(DNF_REPO_BACKUP);
        let (code, out) = run("cp", &["-a", "/etc/yum.repos.d", DNF_REPO_BACKUP]);
        assert_eq!(code, 0, "backing up the repositories: {out}");
    }
}

/// Put `/etc/yum.repos.d` back as it was before the scenario.
fn restore_dnf_repos() {
    if Path::new(DNF_REPO_BACKUP).exists() {
        let _ = fs::remove_dir_all("/etc/yum.repos.d");
        let _ = run("cp", &["-a", DNF_REPO_BACKUP, "/etc/yum.repos.d"]);
        let _ = fs::remove_dir_all(DNF_REPO_BACKUP);
    }
}

/// Add a repository, without signature checks, at `priority`.
#[given(expr = "a dnf repository {word} at {string} with priority {int}")]
fn dnf_repo(w: &mut RealWorld, id: String, url: String, priority: i32) {
    back_up_dnf_repos(w);
    fs::write(
        DNF_TEST_REPO,
        format!("[{id}]\nname={id}\nbaseurl={url}\ngpgcheck=0\nenabled=1\npriority={priority}\n"),
    )
    .unwrap();
    let (code, out) = run("dnf", &["makecache"]);
    assert_eq!(code, 0, "dnf makecache: {out}");
}

/// Give an existing repository `priority` (its section in its repo file).
#[given(expr = "the dnf repository {word} has priority {int}")]
fn dnf_repo_priority(w: &mut RealWorld, id: String, priority: i32) {
    back_up_dnf_repos(w);
    let section = format!("[{id}]");
    let file = fs::read_dir("/etc/yum.repos.d")
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .find(|p| fs::read_to_string(p).is_ok_and(|t| t.lines().any(|l| l.trim() == section)))
        .unwrap_or_else(|| panic!("no repo file has {section}"));
    let text = fs::read_to_string(&file).unwrap();
    let edited: String = text
        .lines()
        .flat_map(|l| {
            let extra = (l.trim() == section).then(|| format!("priority={priority}"));
            std::iter::once(l.to_string()).chain(extra)
        })
        .map(|l| l + "\n")
        .collect();
    fs::write(&file, edited).unwrap();
}

#[then(expr = "rpm reports {word} at {word}")]
fn rpm_version(_w: &mut RealWorld, pkg: String, version: String) {
    let (code, out) = run(
        "rpm",
        &[
            "-q",
            "--queryformat",
            "%|EPOCH?{%{EPOCH}:}:{}|%{VERSION}-%{RELEASE}",
            &pkg,
        ],
    );
    assert_eq!(code, 0, "rpm -q {pkg}: {out}");
    assert_eq!(out.trim(), version, "rpm -q {pkg}");
}

#[then(expr = "rpm reports {word} not installed")]
fn rpm_absent(_w: &mut RealWorld, pkg: String) {
    let (code, out) = run("rpm", &["-q", &pkg]);
    assert_ne!(code, 0, "rpm still reports {pkg} installed: {out}");
}

/// Whether `dnf versionlock list` names `pkg`: dnf5 prints `Package name:
/// <pkg>`, dnf4 `<pkg>-<epoch>:<version>-<release>.*`.
fn dnf_locked(pkg: &str) -> (bool, String) {
    let (code, out) = run("dnf", &["versionlock", "list"]);
    assert_eq!(code, 0, "dnf versionlock list: {out}");
    let dnf5 = format!("Package name: {pkg}");
    let dnf4 = format!("{pkg}-");
    let locked = out.lines().map(str::trim).any(|l| {
        l == dnf5
            || l.strip_prefix(&dnf4)
                .is_some_and(|rest| rest.starts_with(|c: char| c.is_ascii_digit()))
    });
    (locked, out)
}

#[then(expr = "dnf reports {word} locked")]
fn dnf_reports_locked(_w: &mut RealWorld, pkg: String) {
    let (locked, out) = dnf_locked(&pkg);
    assert!(locked, "versionlock list:\n{out}");
}

#[then(expr = "dnf reports {word} not locked")]
fn dnf_reports_unlocked(_w: &mut RealWorld, pkg: String) {
    let (locked, out) = dnf_locked(&pkg);
    assert!(!locked, "versionlock list:\n{out}");
}

#[then(expr = "it warns {string}")]
fn warns(w: &mut RealWorld, needle: String) {
    assert!(
        w.stdout.contains(&needle) || w.stderr.contains(&needle),
        "stdout:\n{}\nstderr:\n{}",
        w.stdout,
        w.stderr
    );
}

#[then(expr = "it does not warn {string}")]
fn does_not_warn(w: &mut RealWorld, needle: String) {
    assert!(
        !w.stdout.contains(&needle) && !w.stderr.contains(&needle),
        "stdout:\n{}\nstderr:\n{}",
        w.stdout,
        w.stderr
    );
}

/// Root installs `pkg@version` into npm's own global prefix (a system
/// Node's, root's to write).
#[given(regex = r"^root has npm install (\S+)@(\S+) globally$")]
fn npm_root_global(_w: &mut RealWorld, pkg: String, version: String) {
    let (code, out) = run("npm", &["install", "-g", &format!("{pkg}@{version}")]);
    assert_eq!(code, 0, "npm install -g: {out}");
}

#[then(regex = r"^root removes (\S+) from npm's global prefix$")]
fn npm_root_global_remove(_w: &mut RealWorld, pkg: String) {
    let (code, out) = run("npm", &["uninstall", "-g", &pkg]);
    assert_eq!(code, 0, "npm uninstall -g: {out}");
}

#[then(expr = "the binary {word} exists and is executable")]
fn binary_executable(w: &mut RealWorld, name: String) {
    let path = w.bin_dir().join(&name);
    let meta = fs::metadata(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    assert!(meta.is_file(), "{} is not a file", path.display());
    // Windows has no mode bits to check.
    #[cfg(unix)]
    assert!(
        meta.permissions().mode() & 0o111 != 0,
        "{} not executable",
        path.display()
    );
}

#[then(expr = "the binary {word} does not exist")]
fn binary_gone(w: &mut RealWorld, name: String) {
    let path = w.bin_dir().join(&name);
    assert!(!path.exists(), "{} still exists", path.display());
}

/// The non-root user's `~/.npmrc` sets npm's global prefix to `~/<dir>`.
#[given(regex = r"^the non-root user's npm prefix is ~/(\S+)$")]
fn npm_prefix(w: &mut RealWorld, dir: String) {
    let home = w.home();
    let npmrc = format!("prefix={}\n", home.join(dir).display());
    fs::write(home.join(".npmrc"), npmrc).unwrap();
}

#[then(regex = r"^~/(\S+) exists and is executable$")]
fn home_file_executable(w: &mut RealWorld, rel: String) {
    let path = w.home().join(rel);
    let meta = fs::metadata(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    assert!(!meta.is_dir(), "{} is a directory", path.display());
    #[cfg(unix)]
    assert!(
        meta.permissions().mode() & 0o111 != 0,
        "{} not executable",
        path.display()
    );
}

#[then(regex = r"^~/(\S+) does not exist$")]
fn home_file_gone(w: &mut RealWorld, rel: String) {
    let path = w.home().join(rel);
    assert!(
        fs::symlink_metadata(&path).is_err(),
        "{} still exists",
        path.display()
    );
}

/// `npm ls -g` under the prefix `~/<dir>` names `pkg` at `version`.
#[then(regex = r"^npm lists (\S+) (\S+) under ~/(\S+)$")]
fn npm_lists(w: &mut RealWorld, pkg: String, version: String, dir: String) {
    let prefix = w.home().join(dir).display().to_string();
    let (code, out) = run(
        "npm",
        &["ls", "-g", "--depth=0", "--json", "--prefix", &prefix],
    );
    assert_eq!(code, 0, "npm ls: {out}");
    let ls: serde_json::Value = serde_json::from_str(&out).expect("npm ls --json");
    assert_eq!(
        ls["dependencies"][&pkg]["version"],
        version.as_str(),
        "npm ls:\n{out}"
    );
}

/// `uv tool list`, for the non-root user's home, names `pkg` at `version`.
#[then(expr = "uv lists {word} {word}")]
fn uv_lists(w: &mut RealWorld, pkg: String, version: String) {
    let home = format!("HOME={}", w.home().display());
    let (code, out) = run("env", &[&home, "uv", "tool", "list"]);
    assert_eq!(code, 0, "uv tool list: {out}");
    let want = format!("{pkg} v{version}");
    assert!(out.lines().any(|l| l == want), "uv tool list:\n{out}");
}

#[then("the bin dir has no temp files")]
fn no_temp_files(w: &mut RealWorld) {
    let dir = w.bin_dir();
    let mut stack = vec![dir.clone()];
    while let Some(d) = stack.pop() {
        for entry in fs::read_dir(&d).into_iter().flatten().flatten() {
            let p = entry.path();
            if p.is_dir() {
                stack.push(p);
            } else {
                let name = p.file_name().unwrap().to_string_lossy().to_string();
                assert!(
                    !name.ends_with(".tmp") && !name.ends_with(".partial"),
                    "leftover temp file: {}",
                    p.display()
                );
            }
        }
    }
}

fn enabled() -> bool {
    std::env::var("BOSUN_ACCEPTANCE_REAL").is_ok_and(|v| v == "1")
        && (Path::new("/.dockerenv").exists()
            || Path::new("/run/.containerenv").exists()
            || std::env::var_os("container").is_some())
}

/// Whether `program` is on `PATH`.
fn on_path(program: &str) -> bool {
    std::env::var_os("PATH")
        .is_some_and(|path| std::env::split_paths(&path).any(|dir| dir.join(program).is_file()))
}

fn main() {
    if !enabled() {
        eprintln!(
            "acceptance_real: skipped - set BOSUN_ACCEPTANCE_REAL=1 inside a container \
             (this mutates the container's apt/filesystem)."
        );
        return;
    }
    // Each tag names a program the scenario needs on PATH (`@apt-get`,
    // `@dnf-3` for dnf4, `@apk`), so one suite runs in Debian, Fedora,
    // RHEL-family and Alpine containers. One scenario at a time: they share
    // the container's package database and system state.
    futures::executor::block_on(
        RealWorld::cucumber()
            .max_concurrent_scenarios(1)
            .after(|_, _, _, _, world| {
                if let Some(w) = world {
                    if w.local_repo {
                        remove_local_repo();
                    }
                    if w.dnf_repos {
                        restore_dnf_repos();
                    }
                }
                Box::pin(async {})
            })
            .filter_run_and_exit("tests/acceptance_real/features", |feature, _, scenario| {
                feature
                    .tags
                    .iter()
                    .chain(&scenario.tags)
                    .all(|t| on_path(t))
            }),
    );
}
