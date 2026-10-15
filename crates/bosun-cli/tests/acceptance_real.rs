//! Real-backend acceptance (§11, tier 2): runs the real `bosun` binary against
//! real apt / github-release, **mutating the container**. Guarded twice — it
//! refuses to run unless `BOSUN_ACCEPTANCE_REAL=1` *and* a container marker is
//! present — so `cargo test --workspace` (which builds and runs this binary)
//! stays hermetic and a developer cannot run it on a workstation by mistake.
//! Run it with `just acceptance-real` inside a disposable container.
//!
//! State is redirected via `HOME` (user scope + the github bin dir land in a
//! temp dir); system-scope apt state lives at its real `/var/lib/bosun` path,
//! cleaned at the start of the apt scenario.

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

#[given("the manifest:")]
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

fn main() {
    if !enabled() {
        eprintln!(
            "acceptance_real: skipped - set BOSUN_ACCEPTANCE_REAL=1 inside a container \
             (this mutates the container's apt/filesystem)."
        );
        return;
    }
    futures::executor::block_on(
        RealWorld::cucumber().run_and_exit("tests/acceptance_real/features"),
    );
}
