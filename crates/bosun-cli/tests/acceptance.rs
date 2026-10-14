//! Cucumber acceptance tests (§11): each scenario runs the real `bosun` binary
//! against the hermetic fixture seam (`BOSUN_FIXTURE` + `BOSUN_STATE_DIR` in a
//! temp dir), so `cargo test --workspace` exercises the user-visible behaviour
//! without touching the host. Real-backend scenarios are a separate tier
//! (slice 11-2, `just acceptance-real`).

use std::fs;
use std::path::PathBuf;
use std::process::Command;

use cucumber::gherkin::Step;
use cucumber::{given, then, when, World};
use tempfile::TempDir;

#[derive(Debug, Default, World)]
struct BosunWorld {
    dir: Option<TempDir>,
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
}

fn body(step: &Step) -> String {
    step.docstring.clone().expect("step needs a docstring")
}

#[given("a fixture:")]
fn a_fixture(world: &mut BosunWorld, step: &Step) {
    let p = world.path();
    fs::write(p.join("fixture.json"), body(step)).unwrap();
}

#[given("a manifest:")]
fn a_manifest(world: &mut BosunWorld, step: &Step) {
    let p = world.path();
    fs::write(p.join("bosun.yaml"), body(step)).unwrap();
}

#[given(expr = "the {word} state:")]
fn a_state(world: &mut BosunWorld, step: &Step, scope: String) {
    let p = world.path();
    // `{manifest}` in the state stands for this project's manifest path, so a
    // claim can reference the actual temp-dir file.
    let state = body(step).replace("{manifest}", &p.join("bosun.yaml").display().to_string());
    let dir = p.join("state").join(&scope);
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("state.json"), state).unwrap();
}

#[given(expr = "the manifest is deleted")]
#[when(expr = "the manifest is deleted")]
fn delete_manifest(world: &mut BosunWorld) {
    let p = world.path();
    fs::remove_file(p.join("bosun.yaml")).unwrap();
}

#[when(expr = "I run {string}")]
fn i_run(world: &mut BosunWorld, cmdline: String) {
    let p = world.path();
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

#[then(expr = "the exit code is {int}")]
fn exit_code(world: &mut BosunWorld, code: i32) {
    assert_eq!(
        world.code, code,
        "exit {}; stdout:\n{}\nstderr:\n{}",
        world.code, world.stdout, world.stderr
    );
}

#[then(expr = "stdout contains {string}")]
fn stdout_contains(world: &mut BosunWorld, needle: String) {
    assert!(world.stdout.contains(&needle), "stdout:\n{}", world.stdout);
}

#[then(expr = "stdout does not contain {string}")]
fn stdout_absent(world: &mut BosunWorld, needle: String) {
    assert!(!world.stdout.contains(&needle), "stdout:\n{}", world.stdout);
}

#[then(expr = "stderr contains {string}")]
fn stderr_contains(world: &mut BosunWorld, needle: String) {
    assert!(world.stderr.contains(&needle), "stderr:\n{}", world.stderr);
}

fn recorded_state(world: &mut BosunWorld) -> String {
    let p = world.path();
    let mut all = String::new();
    for scope in ["system", "user"] {
        let f = p.join("state").join(scope).join("state.json");
        if let Ok(text) = fs::read_to_string(&f) {
            all.push_str(&text);
        }
    }
    all
}

#[then(expr = "the recorded state contains {string}")]
fn state_contains(world: &mut BosunWorld, needle: String) {
    let state = recorded_state(world);
    assert!(state.contains(&needle), "state:\n{state}");
}

#[then(expr = "the recorded state does not contain {string}")]
fn state_absent(world: &mut BosunWorld, needle: String) {
    let state = recorded_state(world);
    assert!(!state.contains(&needle), "state:\n{state}");
}

#[then("a lockfile is written")]
fn lock_written(world: &mut BosunWorld) {
    let p = world.path();
    assert!(p.join("bosun.lock").exists(), "no bosun.lock");
}

#[then(expr = "the lockfile contains {string}")]
fn lock_contains(world: &mut BosunWorld, needle: String) {
    let p = world.path();
    let lock = fs::read_to_string(p.join("bosun.lock")).expect("read bosun.lock");
    assert!(lock.contains(&needle), "bosun.lock:\n{lock}");
}

#[then("the recorded claim names this project's manifest by its absolute path")]
fn claim_is_absolute(world: &mut BosunWorld) {
    let p = world.path();
    let abs = fs::canonicalize(p.join("bosun.yaml")).unwrap();
    let state = recorded_state(world);
    let quoted = format!("\"{}\"", abs.display());
    assert!(state.contains(&quoted), "want {quoted} in state:\n{state}");
}

#[then("stdout is a JSON document")]
fn stdout_json(world: &mut BosunWorld) {
    if let Err(e) = serde_json::from_str::<serde_json::Value>(&world.stdout) {
        panic!("{e}; stdout:\n{}", world.stdout);
    }
}

#[then("no lockfile is written")]
fn lock_absent(world: &mut BosunWorld) {
    let p = world.path();
    assert!(!p.join("bosun.lock").exists(), "bosun.lock exists");
}

fn main() {
    futures::executor::block_on(BosunWorld::cucumber().run_and_exit("tests/acceptance/features"));
}
