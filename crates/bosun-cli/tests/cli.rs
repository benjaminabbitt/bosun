//! Integration tests: run the real `bosun` binary against a fixture (the
//! `BOSUN_FIXTURE` seam), never the real host package managers (§10). State is
//! redirected to a temp dir via `BOSUN_STATE_DIR`.

use std::fs;
use std::path::{Path, PathBuf};

use assert_cmd::Command;
use serde_json::Value;
use tempfile::TempDir;

const FIXTURE: &str = r#"{
  "os": "linux", "arch": "amd64",
  "distro": {"id": "debian", "codename": "bookworm"},
  "backends": [
    {"id":"apt","version":"2.6.1","purl_type":"deb","vers_type":"deb","platform_independent":false,
     "available": {"jq": ["1.7.0-1","1.7.1-3"], "fd": ["1.0.0"]}},
    {"id":"cargo","version":"1.76.0","purl_type":"cargo","vers_type":"cargo","platform_independent":true,
     "available": {"ripgrep": ["14.0.0","14.1.1"], "fd": ["1.0.0"]}}
  ]
}"#;

const MANIFEST: &str = r#"version: 2
packages:
  - name: jq
    spec: ">=1.7|<2"
    install:
      linux:debian: {backend: apt}
  - name: ripgrep
    spec: ">=14.0.0|<15.0.0"
    install:
      linux: {backend: cargo}
"#;

/// A project dir with a fixture and a manifest, plus a state dir.
fn project() -> TempDir {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("fixture.json"), FIXTURE).unwrap();
    fs::write(dir.path().join("bosun.yaml"), MANIFEST).unwrap();
    dir
}

fn bosun(dir: &Path) -> Command {
    // env!("CARGO_BIN_EXE_bosun") is the binary cargo built for this test run —
    // under cargo-llvm-cov that is the instrumented one, so subprocess coverage
    // is captured (cargo_bin's own resolution would miss it).
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_bosun"));
    cmd.env("BOSUN_FIXTURE", dir.join("fixture.json"))
        .env("BOSUN_STATE_DIR", dir.join("state"));
    cmd
}

fn stdout_json(output: &[u8]) -> Value {
    serde_json::from_slice(output).expect("stdout is json")
}

#[test]
fn version_is_reported() {
    let out = Command::new(env!("CARGO_BIN_EXE_bosun"))
        .arg("--version")
        .assert()
        .success();
    let text = String::from_utf8_lossy(&out.get_output().stdout).to_string();
    assert!(text.contains("0.0.0"), "version output: {text}");
}

#[test]
fn backends_lists_the_fixture_backends() {
    let dir = project();
    let assert = bosun(dir.path())
        .args(["--format", "json", "backends"])
        .assert()
        .success();
    let json = stdout_json(&assert.get_output().stdout);
    let ids: Vec<&str> = json["registered"]
        .as_array()
        .unwrap()
        .iter()
        .map(|b| b["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, vec!["apt", "cargo"]);
}

#[test]
fn output_defaults_to_json_when_piped() {
    let dir = project();
    // No --format, output captured (not a TTY) → must be valid json.
    let assert = bosun(dir.path()).arg("backends").assert().success();
    let _ = stdout_json(&assert.get_output().stdout);
}

#[test]
fn lock_resolves_and_writes_the_lockfile() {
    let dir = project();
    let assert = bosun(dir.path())
        .args(["--format", "json", "lock"])
        .arg(dir.path())
        .assert()
        .success();
    let json = stdout_json(&assert.get_output().stdout);
    let resolved = json["resolved"].as_array().unwrap();
    assert_eq!(resolved.len(), 2);
    // The lockfile is written next to the manifest with the resolved versions.
    let lock = fs::read_to_string(dir.path().join("bosun.lock")).unwrap();
    assert!(lock.contains("\"resolved\": \"1.7.1-3\""), "lock: {lock}");
    assert!(
        lock.contains("pkg:deb/debian/jq?arch=amd64&distro=bookworm"),
        "lock: {lock}"
    );
}

#[test]
fn status_reports_wanted_versions_for_an_empty_machine() {
    let dir = project();
    let assert = bosun(dir.path())
        .args(["--format", "json", "status"])
        .arg(dir.path())
        .assert()
        .success();
    let json = stdout_json(&assert.get_output().stdout);
    let jq = json["packages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["name"] == "jq")
        .unwrap();
    assert_eq!(jq["installed"], Value::Null);
    assert_eq!(jq["wanted"], "1.7.1-3");
    assert_eq!(jq["up_to_date"], false);
}

#[test]
fn a_missing_manifest_is_a_structured_error() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("fixture.json"), FIXTURE).unwrap();
    // No bosun.yaml in this dir.
    let assert = bosun(dir.path())
        .args(["--format", "json", "lock"])
        .arg(dir.path())
        .assert()
        .code(1);
    let json = stdout_json(&assert.get_output().stdout);
    assert_eq!(json["error"], "bosun.cli.manifest_not_found");
}

fn project_with(manifest: &str) -> TempDir {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("fixture.json"), FIXTURE).unwrap();
    fs::write(dir.path().join("bosun.yaml"), manifest).unwrap();
    dir
}

fn write_system_state(dir: &Path, json: &str) {
    let sys = dir.join("state").join("system");
    fs::create_dir_all(&sys).unwrap();
    fs::write(sys.join("state.json"), json).unwrap();
}

#[test]
fn text_format_renders_human_lines() {
    let dir = project();
    let backends = bosun(dir.path())
        .args(["--format", "text", "backends"])
        .assert()
        .success();
    let text = String::from_utf8_lossy(&backends.get_output().stdout).to_string();
    assert!(
        text.contains("apt") && text.contains("cargo"),
        "backends text: {text}"
    );

    let lock = bosun(dir.path())
        .args(["--format", "text", "lock"])
        .arg(dir.path())
        .assert()
        .success();
    let text = String::from_utf8_lossy(&lock.get_output().stdout).to_string();
    assert!(text.contains("Locked"), "lock text: {text}");
}

#[test]
fn installer_preference_picks_among_the_acceptable_backends() {
    let manifest = "version: 2\npackages:\n  - name: fd\n    spec: \">=1.0.0|<2.0.0\"\n    install:\n      linux: {backends: {apt: {}, cargo: {}}}\n";

    // Default: no preference, and both are user scope in this fixture, so the
    // first registered backend (apt) wins.
    let dir = project_with(manifest);
    let default = bosun(dir.path())
        .args(["--format", "json", "lock"])
        .arg(dir.path())
        .assert()
        .success();
    assert_eq!(
        stdout_json(&default.get_output().stdout)["resolved"][0]["backend"],
        "apt"
    );

    // --installer cargo reorders the preference, so cargo wins.
    let dir = project_with(manifest);
    let pref = bosun(dir.path())
        .args(["--installer", "cargo", "--format", "json", "lock"])
        .arg(dir.path())
        .assert()
        .success();
    assert_eq!(
        stdout_json(&pref.get_output().stdout)["resolved"][0]["backend"],
        "cargo"
    );
}

#[test]
fn status_reflects_installed_state() {
    let dir = project();
    // jq is already at the wanted version; ripgrep is behind (outdated).
    write_system_state(
        dir.path(),
        r#"{"stateVersion":1,"packages":[
          {"name":"jq","targets":{"linux/amd64":{"backend":"apt","purl":"pkg:deb/debian/jq?arch=amd64&distro=bookworm","installed":"1.7.1-3","origin":"installed","claimedBy":[]}}},
          {"name":"ripgrep","targets":{"any":{"backend":"cargo","purl":"pkg:cargo/ripgrep","installed":"14.0.0","origin":"installed","claimedBy":[]}}}
        ]}"#,
    );
    // text exercises the human renderer and both the up-to-date and outdated paths.
    let text_run = bosun(dir.path())
        .args(["--format", "text", "status"])
        .arg(dir.path())
        .assert()
        .success();
    let text = String::from_utf8_lossy(&text_run.get_output().stdout).to_string();
    assert!(text.contains("up to date"), "status text: {text}");
    assert!(text.contains("outdated"), "status text: {text}");

    let json_run = bosun(dir.path())
        .args(["--format", "json", "status"])
        .arg(dir.path())
        .assert()
        .success();
    let json = stdout_json(&json_run.get_output().stdout);
    let jq = json["packages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["name"] == "jq")
        .unwrap();
    assert_eq!(jq["installed"], "1.7.1-3");
    assert_eq!(jq["up_to_date"], true);
}

#[test]
fn a_non_applicable_entry_is_reported_with_its_reason() {
    let manifest = "version: 2\npackages:\n  - name: mac-only\n    spec: \">=1.0.0|<2.0.0\"\n    install:\n      darwin: {backend: cargo}\n";
    let dir = project_with(manifest);
    let run = bosun(dir.path())
        .args(["--format", "json", "lock"])
        .arg(dir.path())
        .assert()
        .success();
    let json = stdout_json(&run.get_output().stdout);
    let diagnostics = json["diagnostics"].as_array().unwrap();
    assert!(
        diagnostics
            .iter()
            .any(|d| d.as_str().unwrap().contains("no matching selector")),
        "diagnostics: {diagnostics:?}"
    );
}

#[test]
fn the_fixture_seam_requires_both_env_vars() {
    let dir = project();
    // BOSUN_FIXTURE alone (no state dir) → structured environment error, exit 1.
    let only_fixture = Command::new(env!("CARGO_BIN_EXE_bosun"))
        .env("BOSUN_FIXTURE", dir.path().join("fixture.json"))
        .env_remove("BOSUN_STATE_DIR")
        .args(["--format", "json", "backends"])
        .assert()
        .code(1);
    assert_eq!(
        stdout_json(&only_fixture.get_output().stdout)["error"],
        "bosun.cli.environment"
    );

    // BOSUN_STATE_DIR alone (no fixture) → the same error, never a real probe.
    let only_state = Command::new(env!("CARGO_BIN_EXE_bosun"))
        .env("BOSUN_STATE_DIR", dir.path().join("state"))
        .env_remove("BOSUN_FIXTURE")
        .args(["--format", "json", "backends"])
        .assert()
        .code(1);
    assert_eq!(
        stdout_json(&only_state.get_output().stdout)["error"],
        "bosun.cli.environment"
    );
}

#[test]
fn the_fixture_seam_warns_on_stderr_while_active() {
    let dir = project();
    let assert = bosun(dir.path()).arg("backends").assert().success();
    let stderr = String::from_utf8_lossy(&assert.get_output().stderr).to_string();
    assert!(stderr.contains("test fixture active"), "stderr: {stderr}");
}

// --- 10-2: install / uninstall / prune ------------------------------------

/// A fixture with install sides: ripgrep via cargo (user) and jq via apt
/// (system), each reporting its version from verify after apply.
const APPLY_FIXTURE: &str = r#"{
  "os": "linux", "arch": "amd64",
  "distro": {"id": "debian", "codename": "bookworm"},
  "privilege": "unelevated", "user": "babbitt",
  "backends": [
    {"id":"cargo","version":"1.76.0","purl_type":"cargo","vers_type":"cargo","platform_independent":true,
     "scope":"user","supports_hold":false,
     "available":{"ripgrep":["14.1.1"]}},
    {"id":"apt","version":"2.6.1","purl_type":"deb","vers_type":"deb","platform_independent":false,
     "scope":"system","supports_hold":true,
     "available":{"jq":["1.7.1-3"]}}
  ]
}"#;

const USER_MANIFEST: &str = r#"version: 2
packages:
  - name: ripgrep
    spec: ">=14.0.0|<15.0.0"
    install:
      linux: {backend: cargo}
"#;

const SYSTEM_MANIFEST: &str = r#"version: 2
packages:
  - name: jq
    spec: ">=1.7|<2"
    install:
      linux:debian: {backend: apt}
"#;

fn apply_project(manifest: &str) -> TempDir {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("fixture.json"), APPLY_FIXTURE).unwrap();
    fs::write(dir.path().join("bosun.yaml"), manifest).unwrap();
    dir
}

fn state_file(dir: &Path) -> PathBuf {
    dir.join("state").join("user").join("state.json")
}

#[test]
fn install_plan_shows_the_plan_and_writes_no_state() {
    let dir = apply_project(USER_MANIFEST);
    let assert = bosun(dir.path())
        .args(["--format", "json", "install"])
        .arg(dir.path())
        .arg("--plan")
        .assert()
        .code(2); // changes pending
    let json = stdout_json(&assert.get_output().stdout);
    assert_eq!(json["actions"][0]["action"], "install");
    assert!(!state_file(dir.path()).exists(), "--plan writes nothing");
}

#[test]
fn install_applies_and_records_state() {
    let dir = apply_project(USER_MANIFEST);
    let assert = bosun(dir.path())
        .args(["--format", "json", "install"])
        .arg(dir.path())
        .arg("--yes")
        .assert()
        .code(0);
    let json = stdout_json(&assert.get_output().stdout);
    assert_eq!(json["outcomes"][0]["result"], "applied");
    let state = fs::read_to_string(state_file(dir.path())).unwrap();
    assert!(state.contains("\"ripgrep\""), "state: {state}");
    assert!(
        state.contains("\"installed\": \"14.1.1\""),
        "state: {state}"
    );
}

#[test]
fn install_without_a_terminal_and_without_yes_refuses() {
    let dir = apply_project(USER_MANIFEST);
    // assert_cmd gives no TTY; no --yes → refuse (exit 1), nothing installed.
    let assert = bosun(dir.path())
        .args(["--format", "json", "install"])
        .arg(dir.path())
        .assert()
        .code(1);
    assert_eq!(
        stdout_json(&assert.get_output().stdout)["error"],
        "bosun.cli.environment"
    );
    assert!(!state_file(dir.path()).exists());
}

#[test]
fn a_system_install_unelevated_needs_root_with_a_remedy() {
    let dir = apply_project(SYSTEM_MANIFEST);
    let assert = bosun(dir.path())
        .args(["--format", "json", "install"])
        .arg(dir.path())
        .arg("--yes")
        .assert()
        .code(1);
    let json = stdout_json(&assert.get_output().stdout);
    assert_eq!(json["error"], "elevation_required");
    assert!(
        json["remedy"].as_str().unwrap().contains("sudo"),
        "remedy: {json}"
    );
    // jq named exactly once despite install + hold.
    assert_eq!(json["packages"].as_array().unwrap().len(), 1);
}

#[test]
fn frozen_install_applies_the_locked_version() {
    let dir = apply_project(USER_MANIFEST);
    bosun(dir.path())
        .args(["lock"])
        .arg(dir.path())
        .assert()
        .success();
    let assert = bosun(dir.path())
        .args(["--format", "json", "install"])
        .arg(dir.path())
        .args(["--yes", "--frozen"])
        .assert()
        .code(0);
    assert_eq!(
        stdout_json(&assert.get_output().stdout)["outcomes"][0]["result"],
        "applied"
    );
    let state = fs::read_to_string(state_file(dir.path())).unwrap();
    assert!(
        state.contains("\"installed\": \"14.1.1\""),
        "state: {state}"
    );
}

#[test]
fn uninstall_removes_the_package_from_state() {
    let dir = apply_project(USER_MANIFEST);
    bosun(dir.path())
        .args(["install"])
        .arg(dir.path())
        .arg("--yes")
        .assert()
        .success();
    assert!(fs::read_to_string(state_file(dir.path()))
        .unwrap()
        .contains("ripgrep"));

    bosun(dir.path())
        .args(["uninstall"])
        .arg(dir.path())
        .arg("--yes")
        .assert()
        .success();
    let state = fs::read_to_string(state_file(dir.path())).unwrap();
    assert!(!state.contains("ripgrep"), "state after uninstall: {state}");
}

#[test]
fn prune_removes_an_orphan_whose_manifest_is_gone() {
    let dir = apply_project(USER_MANIFEST);
    bosun(dir.path())
        .args(["install"])
        .arg(dir.path())
        .arg("--yes")
        .assert()
        .success();
    // Delete the manifest; its claim is now orphaned.
    fs::remove_file(dir.path().join("bosun.yaml")).unwrap();

    let assert = bosun(dir.path())
        .args(["--format", "json", "prune"])
        .arg("--yes")
        .assert()
        .code(0);
    assert_eq!(
        stdout_json(&assert.get_output().stdout)["outcomes"][0]["action"],
        "remove"
    );
    let state = fs::read_to_string(state_file(dir.path())).unwrap();
    assert!(!state.contains("ripgrep"), "state after prune: {state}");
}

#[test]
fn a_failed_step_mid_apply_keeps_the_earlier_package() {
    // Two cargo packages; the step for "two" fails. "one" installs and is
    // checkpointed before "two" runs, so it survives the failure (Appendix B).
    let fixture = r#"{
      "os":"linux","arch":"amd64","privilege":"unelevated","user":"t",
      "fail_step":"two",
      "backends":[
        {"id":"cargo","version":"1.76.0","purl_type":"cargo","vers_type":"cargo","platform_independent":true,
         "scope":"user","supports_hold":false,
         "available":{"one":["1.0.0"],"two":["2.0.0"]}}
      ]
    }"#;
    let manifest = "version: 2\npackages:\n  - name: one\n    spec: \">=1.0.0|<2.0.0\"\n    install:\n      linux: {backend: cargo}\n  - name: two\n    spec: \">=2.0.0|<3.0.0\"\n    install:\n      linux: {backend: cargo}\n";
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("fixture.json"), fixture).unwrap();
    fs::write(dir.path().join("bosun.yaml"), manifest).unwrap();

    let assert = bosun(dir.path())
        .args(["--format", "json", "install"])
        .arg(dir.path())
        .arg("--yes")
        .assert()
        .code(1); // a failed action → non-zero
    let json = stdout_json(&assert.get_output().stdout);
    let results: Vec<(&str, &str)> = json["outcomes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|o| (o["name"].as_str().unwrap(), o["result"].as_str().unwrap()))
        .collect();
    assert!(results.contains(&("one", "applied")));
    assert!(results.iter().any(|(n, r)| *n == "two" && *r == "failed"));
    // "one" was checkpointed to disk before "two" failed.
    let state = fs::read_to_string(state_file(dir.path())).unwrap();
    assert!(state.contains("\"one\""), "state: {state}");
    assert!(!state.contains("\"two\""), "state: {state}");
}

fn write_user_state(dir: &Path, json: &str) {
    let d = dir.join("state").join("user");
    fs::create_dir_all(&d).unwrap();
    fs::write(d.join("state.json"), json).unwrap();
}

#[test]
fn install_over_an_existing_package_upgrades_and_merges_claims() {
    let dir = apply_project(USER_MANIFEST);
    // ripgrep already at an older version, claimed by another manifest.
    write_user_state(
        dir.path(),
        r#"{"stateVersion":1,"packages":[{"name":"ripgrep","targets":{"linux/amd64":{"backend":"cargo","purl":"pkg:cargo/ripgrep","installed":"14.0.0","origin":"installed","claimedBy":[{"manifest":"/other/bosun.yaml","spec":"vers:cargo/>=14.0.0|<15.0.0"}]}}}]}"#,
    );
    // The plan is an upgrade (installed 14.0.0 < wanted 14.1.1).
    let plan = bosun(dir.path())
        .args(["--format", "json", "install"])
        .arg(dir.path())
        .arg("--plan")
        .assert()
        .code(2);
    assert_eq!(
        stdout_json(&plan.get_output().stdout)["actions"][0]["action"],
        "upgrade"
    );

    // Applying merges the other manifest's claim with this project's.
    bosun(dir.path())
        .args(["install"])
        .arg(dir.path())
        .arg("--yes")
        .assert()
        .success();
    let state: Value =
        serde_json::from_str(&fs::read_to_string(state_file(dir.path())).unwrap()).unwrap();
    let rg = state["packages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["name"] == "ripgrep")
        .unwrap();
    let claims = rg["targets"]["linux/amd64"]["claimedBy"]
        .as_array()
        .unwrap();
    assert_eq!(claims.len(), 2, "both claims kept: {claims:?}");
}

#[test]
fn install_plan_and_apply_render_as_text() {
    let dir = apply_project(USER_MANIFEST);
    let plan = bosun(dir.path())
        .args(["--format", "text", "install"])
        .arg(dir.path())
        .arg("--plan")
        .assert()
        .code(2);
    let text = String::from_utf8_lossy(&plan.get_output().stdout).to_string();
    assert!(
        text.contains("install") && text.contains("Plan:"),
        "plan text: {text}"
    );

    let applied = bosun(dir.path())
        .args(["--format", "text", "install"])
        .arg(dir.path())
        .arg("--yes")
        .assert()
        .code(0);
    let text = String::from_utf8_lossy(&applied.get_output().stdout).to_string();
    assert!(text.contains("applied"), "apply text: {text}");
}

#[test]
fn uninstall_accepts_a_manifest_file_path() {
    let dir = apply_project(USER_MANIFEST);
    bosun(dir.path())
        .args(["install"])
        .arg(dir.path())
        .arg("--yes")
        .assert()
        .success();
    // Pass the manifest file path directly (not the directory).
    bosun(dir.path())
        .args(["uninstall"])
        .arg(dir.path().join("bosun.yaml"))
        .arg("--yes")
        .assert()
        .success();
    let state = fs::read_to_string(state_file(dir.path())).unwrap();
    assert!(!state.contains("ripgrep"), "state after uninstall: {state}");
}

// --- 10-3 regressions (A, C, D, E) -----------------------------------------

fn write_project(fixture: &str, manifest: &str) -> TempDir {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("fixture.json"), fixture).unwrap();
    fs::write(dir.path().join("bosun.yaml"), manifest).unwrap();
    dir
}

#[test]
fn install_accepts_a_package_present_outside_bosun() {
    // A — host has jq installed (not by bosun), candidate is u2; the manifest
    // is satisfied by what's present → accept, never install or hold.
    let fixture = r#"{"os":"linux","arch":"amd64","distro":{"id":"debian","codename":"trixie"},
      "privilege":"unelevated","user":"ben",
      "backends":[{"id":"apt","version":"2.6.1","purl_type":"deb","vers_type":"deb","platform_independent":false,
        "scope":"system","supports_hold":true,
        "available":{"jq":["1.7.1-6+deb13u2"]},"installed":{"jq":"1.7.1-6+deb13u1"}}]}"#;
    let manifest = "version: 2\npackages:\n  - name: jq\n    spec: \">=1.6\"\n    install:\n      linux:debian: {backend: apt}\n";
    let dir = write_project(fixture, manifest);
    let assert = bosun(dir.path())
        .args(["--format", "json", "install"])
        .arg(dir.path())
        .arg("--plan")
        .assert()
        .code(0);
    let json = stdout_json(&assert.get_output().stdout);
    let actions = json["actions"].as_array().unwrap();
    assert_eq!(actions.len(), 1);
    assert_eq!(actions[0]["action"], "accept");
    assert!(actions
        .iter()
        .all(|a| a["action"] != "install" && a["action"] != "hold"));
}

#[test]
fn lock_refuses_to_write_an_incomplete_target() {
    // C — an unsatisfiable entry must exit 1 and write no lockfile.
    let fixture = r#"{"os":"linux","arch":"amd64","privilege":"unelevated","user":"t",
      "backends":[{"id":"github-release","version":"1.0.0","purl_type":"github","vers_type":"semver","platform_independent":false,
        "scope":"user","available":{"ctxloom/ctxloom":["0.6.4"]}}]}"#;
    let manifest = "version: 2\npackages:\n  - name: ctxloom\n    spec: \">=99.0.0\"\n    install:\n      linux: {backend: github-release, package: ctxloom/ctxloom}\n";
    let dir = write_project(fixture, manifest);
    let assert = bosun(dir.path())
        .args(["--format", "json", "lock"])
        .arg(dir.path())
        .assert()
        .code(1);
    assert_eq!(stdout_json(&assert.get_output().stdout)["error"], true);
    assert!(!dir.path().join("bosun.lock").exists(), "no partial lock");
}

#[test]
fn install_respects_another_projects_claim_range() {
    // D — project B installed ripgrep 14.0.0 and claims it <14.1.0; installing
    // project A must not upgrade past B's range.
    let fixture = r#"{"os":"linux","arch":"amd64","privilege":"unelevated","user":"t",
      "backends":[{"id":"cargo","version":"1.76.0","purl_type":"cargo","vers_type":"cargo","platform_independent":true,
        "scope":"user","available":{"ripgrep":["14.0.0","14.1.1"]},"installed":{"ripgrep":"14.0.0"}}]}"#;
    let manifest = "version: 2\npackages:\n  - name: ripgrep\n    spec: \">=14.0.0|<15.0.0\"\n    install:\n      linux: {backend: cargo}\n";
    let dir = write_project(fixture, manifest);
    write_user_state(
        dir.path(),
        r#"{"stateVersion":1,"packages":[{"name":"ripgrep","targets":{"linux/amd64":{"backend":"cargo","purl":"pkg:cargo/ripgrep","installed":"14.0.0","origin":"installed","claimedBy":[{"manifest":"/other/bosun.yaml","spec":"vers:cargo/>=14.0.0|<14.1.0"}]}}}]}"#,
    );
    let assert = bosun(dir.path())
        .args(["--format", "json", "install"])
        .arg(dir.path())
        .arg("--plan")
        .assert()
        .code(0);
    let json = stdout_json(&assert.get_output().stdout);
    assert!(
        json["actions"].as_array().unwrap().is_empty(),
        "cross-claim clamps to 14.0.0, no upgrade: {json}"
    );
}

#[test]
fn a_system_uninstall_remedy_names_uninstall() {
    // E — the remedy names the actual command, not always install.
    let fixture = r#"{"os":"linux","arch":"amd64","distro":{"id":"debian","codename":"trixie"},
      "privilege":"unelevated","user":"ben",
      "backends":[{"id":"apt","version":"2.6.1","purl_type":"deb","vers_type":"deb","platform_independent":false,
        "scope":"system","supports_hold":true,"available":{},"installed":{}}]}"#;
    let manifest = "version: 2\npackages:\n  - name: jq\n    spec: \">=1.6\"\n    install:\n      linux:debian: {backend: apt}\n";
    let dir = write_project(fixture, manifest);
    let claim = dir.path().join("bosun.yaml").display().to_string();
    let state = r#"{"stateVersion":1,"packages":[{"name":"jq","targets":{"linux/amd64":{"backend":"apt","purl":"pkg:deb/debian/jq?arch=amd64&distro=trixie","installed":"1.7.1-6+deb13u2","origin":"installed","claimedBy":[{"manifest":"MANIFEST","spec":"vers:deb/>=1.6"}]}}}]}"#.replace("MANIFEST", &claim);
    write_system_state(dir.path(), &state);

    let assert = bosun(dir.path())
        .args(["--format", "json", "uninstall"])
        .arg(dir.path())
        .arg("--yes")
        .assert()
        .code(1);
    let json = stdout_json(&assert.get_output().stdout);
    assert_eq!(json["error"], "elevation_required");
    assert!(
        json["remedy"].as_str().unwrap().contains("uninstall"),
        "remedy names the command: {json}"
    );
}

#[test]
fn an_accepted_package_with_a_conflicting_claim_warns_not_refuses() {
    // jq is present (accepted) at 1.7.1 and satisfies this manifest (>=1.6),
    // but project B claims it <1.7. bosun does not own it, so it is never
    // re-resolved against B's range — it reports the mismatch and changes
    // nothing, rather than refusing the whole run.
    let fixture = r#"{"os":"linux","arch":"amd64","distro":{"id":"debian","codename":"trixie"},
      "privilege":"unelevated","user":"ben",
      "backends":[{"id":"apt","version":"2.6.1","purl_type":"deb","vers_type":"deb","platform_independent":false,
        "scope":"system","supports_hold":true,"available":{"jq":["1.7.1"]}}]}"#;
    let manifest = "version: 2\npackages:\n  - name: jq\n    spec: \">=1.6\"\n    install:\n      linux:debian: {backend: apt}\n";
    let dir = write_project(fixture, manifest);
    write_system_state(
        dir.path(),
        r#"{"stateVersion":1,"packages":[{"name":"jq","targets":{"linux/amd64":{"backend":"apt","purl":"pkg:deb/debian/jq?arch=amd64&distro=trixie","installed":"1.7.1","origin":"accepted","claimedBy":[{"manifest":"/b/bosun.yaml","spec":"vers:deb/<1.7"}]}}}]}"#,
    );
    let assert = bosun(dir.path())
        .args(["--format", "json", "install"])
        .arg(dir.path())
        .arg("--plan")
        .assert()
        .code(0); // a warning, not a refusal
    let json = stdout_json(&assert.get_output().stdout);
    assert!(
        json["actions"].as_array().unwrap().is_empty(),
        "no change: {json}"
    );
    assert!(
        !json["diagnostics"].as_array().unwrap().is_empty(),
        "reports the mismatch: {json}"
    );
}

#[test]
fn a_user_scope_install_never_opens_system_state() {
    // Regression for the 11-2 fix: a user-scope (cargo) install must read only
    // user state, never the system file. Make the system state path unreadable
    // (a directory where a file is expected) — an eager read would error; the
    // lazy per-scope read leaves it untouched. No system lock is ever created.
    let dir = apply_project(USER_MANIFEST); // ripgrep via cargo (user scope)
    let system = dir.path().join("state").join("system");
    fs::create_dir_all(&system).unwrap();
    fs::create_dir(system.join("state.json")).unwrap(); // reading this as a file would fail

    bosun(dir.path())
        .args(["install"])
        .arg(dir.path())
        .arg("--yes")
        .assert()
        .code(0);

    // The user install landed, and system state was never opened (no lock).
    assert!(state_file(dir.path()).exists(), "user state written");
    assert!(
        !system.join("state.lock").exists(),
        "system state must not be opened by a user-scope install"
    );
}

#[test]
fn yaml_format_renders_yaml() {
    let dir = project();
    let out = bosun(dir.path())
        .args(["--format", "yaml", "backends"])
        .assert()
        .success();
    let text = String::from_utf8_lossy(&out.get_output().stdout).to_string();
    assert!(text.contains("registered:"), "yaml: {text}");
    assert!(text.contains("id: apt"), "yaml: {text}");
}

#[test]
fn status_reports_a_not_applicable_entry() {
    let dir = project_with(
        "version: 2\npackages:\n  - name: mac-only\n    spec: \">=1.0.0|<2.0.0\"\n    install:\n      darwin: {backend: cargo}\n",
    );
    let out = bosun(dir.path())
        .args(["--format", "json", "status"])
        .arg(dir.path())
        .assert()
        .success();
    let json = stdout_json(&out.get_output().stdout);
    let diags = json["diagnostics"].as_array().unwrap();
    assert!(
        diags
            .iter()
            .any(|d| d.as_str().unwrap().contains("not applicable")),
        "diagnostics: {diags:?}"
    );
}

#[test]
fn a_text_format_error_is_human_readable() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("fixture.json"), FIXTURE).unwrap();
    // No manifest → a structured error, rendered as text.
    let out = bosun(dir.path())
        .args(["--format", "text", "lock"])
        .arg(dir.path())
        .assert()
        .code(1);
    let text = String::from_utf8_lossy(&out.get_output().stdout).to_string();
    assert!(text.contains("error:"), "text error: {text}");
    assert!(
        text.contains("bosun.cli.manifest_not_found"),
        "text error: {text}"
    );
}

#[test]
fn a_text_format_privilege_error_shows_the_remedy() {
    let fixture = r#"{"os":"linux","arch":"amd64","distro":{"id":"debian","codename":"trixie"},
      "privilege":"unelevated","user":"ben",
      "backends":[{"id":"apt","version":"2.6.1","purl_type":"deb","vers_type":"deb","platform_independent":false,
        "scope":"system","supports_hold":true,"available":{"jq":["1.7.1-6+deb13u2"]}}]}"#;
    let manifest = "version: 2\npackages:\n  - name: jq\n    spec: \">=1.6\"\n    install:\n      linux:debian: {backend: apt}\n";
    let dir = write_project(fixture, manifest);
    let out = bosun(dir.path())
        .args(["--format", "text", "install"])
        .arg(dir.path())
        .arg("--yes")
        .assert()
        .code(1);
    let text = String::from_utf8_lossy(&out.get_output().stdout).to_string();
    assert!(text.contains("error:"), "{text}");
    assert!(text.contains("sudo bosun install --scope system"), "{text}");
}

#[test]
fn install_honours_an_explicit_scope() {
    // --scope user restricts the run to user actions (exercises the ScopeArg
    // conversion and the scope-restricted apply path).
    let dir = apply_project(USER_MANIFEST);
    let out = bosun(dir.path())
        .args(["--format", "json", "install"])
        .arg(dir.path())
        .args(["--scope", "user", "--yes"])
        .assert()
        .code(0);
    assert_eq!(
        stdout_json(&out.get_output().stdout)["outcomes"][0]["result"],
        "applied"
    );
}

// --- 12-1 dogfood regressions ---------------------------------------------

#[test]
fn install_records_claims_with_an_absolute_manifest_path() {
    // Bug 2 (§4.6): a claim's manifest is the canonical absolute path. Run from
    // the project dir with no path argument, so the manifest resolves to the
    // relative "./bosun.yaml" the dogfood used — the recorded claim must still
    // be absolute, or two projects' "./bosun.yaml" collide.
    let dir = apply_project(USER_MANIFEST);
    bosun(dir.path())
        .current_dir(dir.path())
        .args(["install", "--yes"])
        .assert()
        .success();
    let state: Value =
        serde_json::from_str(&fs::read_to_string(state_file(dir.path())).unwrap()).unwrap();
    let rg = state["packages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["name"] == "ripgrep")
        .unwrap();
    let manifest = rg["targets"]["linux/amd64"]["claimedBy"][0]["manifest"]
        .as_str()
        .expect("a claim is recorded");
    assert!(
        Path::new(manifest).is_absolute(),
        "claim manifest is not absolute: {manifest}"
    );
    assert!(manifest.ends_with("bosun.yaml"), "{manifest}");
}

#[test]
fn lock_records_the_github_release_checksum() {
    // Bug 3 (§12): for github-release the checksum is enforced and recorded.
    let fixture = r#"{"os":"linux","arch":"amd64","privilege":"unelevated","user":"t",
      "backends":[{"id":"github-release","version":"1.0.0","purl_type":"github","vers_type":"semver","platform_independent":false,
        "scope":"user",
        "available":{"ctxloom/ctxloom":["0.6.4"]},
        "checksums":{"ctxloom/ctxloom":"89a356c697c9458929ecf851bc85da3a9548d19aa978837b54c0b0b27100810f"}}]}"#;
    let manifest = "version: 2\npackages:\n  - name: ctxloom\n    spec: \">=0.6.4|<0.7.0\"\n    install:\n      linux: {backend: github-release, package: ctxloom/ctxloom}\n";
    let dir = write_project(fixture, manifest);
    bosun(dir.path())
        .args(["lock"])
        .arg(dir.path())
        .assert()
        .success();
    let lock = fs::read_to_string(dir.path().join("bosun.lock")).unwrap();
    assert!(
        lock.contains("89a356c697c9458929ecf851bc85da3a9548d19aa978837b54c0b0b27100810f"),
        "lock records the checksum: {lock}"
    );
    assert!(
        lock.contains("\"checksum\""),
        "checksum field present: {lock}"
    );
}

#[test]
fn status_shows_a_package_present_but_not_managed() {
    // Bug 4: a package installed on the machine but absent from bosun state was
    // reported "not installed"; it must show as present-but-unmanaged.
    let fixture = r#"{"os":"linux","arch":"amd64","distro":{"id":"debian","codename":"trixie"},
      "privilege":"unelevated","user":"ben",
      "backends":[{"id":"apt","version":"2.6.1","purl_type":"deb","vers_type":"deb","platform_independent":false,
        "scope":"system","supports_hold":true,
        "available":{"jq":["1.7.1-6+deb13u1"]},"installed":{"jq":"1.7.1-6+deb13u1"}}]}"#;
    let manifest = "version: 2\npackages:\n  - name: jq\n    spec: \">=1.6\"\n    install:\n      linux:debian: {backend: apt}\n";
    let dir = write_project(fixture, manifest);
    let text = bosun(dir.path())
        .args(["--format", "text", "status"])
        .arg(dir.path())
        .assert()
        .success();
    let out = String::from_utf8_lossy(&text.get_output().stdout).to_string();
    assert!(out.contains("present, not managed"), "status text: {out}");
    assert!(
        out.contains("1.7.1-6+deb13u1"),
        "shows the live version: {out}"
    );
    let json_a = bosun(dir.path())
        .args(["--format", "json", "status"])
        .arg(dir.path())
        .assert()
        .success();
    let json = stdout_json(&json_a.get_output().stdout);
    let jq = json["packages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["name"] == "jq")
        .unwrap();
    assert_eq!(jq["present"], true);
    assert_eq!(jq["managed"], false);
    assert_eq!(jq["installed"], "1.7.1-6+deb13u1");
}

#[test]
fn frozen_refuses_an_accepted_package_at_a_different_version_than_the_lock() {
    // Bug 5 (§7): --frozen with jq accepted (not bosun-owned) at u1 while the
    // lock pins u2 is an error naming both versions; bosun will not change a
    // package it does not own, and makes no change.
    let fixture = r#"{"os":"linux","arch":"amd64","distro":{"id":"debian","codename":"trixie"},
      "privilege":"unelevated","user":"ben",
      "backends":[{"id":"apt","version":"2.6.1","purl_type":"deb","vers_type":"deb","platform_independent":false,
        "scope":"system","supports_hold":true,
        "available":{"jq":["1.7.1-6+deb13u1","1.7.1-6+deb13u2"]},"installed":{"jq":"1.7.1-6+deb13u1"}}]}"#;
    let manifest = "version: 2\npackages:\n  - name: jq\n    spec: \">=1.6\"\n    install:\n      linux:debian: {backend: apt}\n";
    let dir = write_project(fixture, manifest);
    let lock = r#"{"lockfileVersion":1,"targets":{"linux/amd64":{"manifestHash":"sha256:x"}},
      "packages":[{"name":"jq","spec":"vers:deb/>=1.6","targets":{"linux/amd64":{
        "backend":"apt","purl":"pkg:deb/debian/jq?arch=amd64&distro=trixie","resolved":"1.7.1-6+deb13u2"}}}]}"#;
    fs::write(dir.path().join("bosun.lock"), lock).unwrap();
    let assert = bosun(dir.path())
        .args(["--format", "json", "install"])
        .arg(dir.path())
        .args(["--plan", "--frozen"])
        .assert()
        .code(1);
    let json = stdout_json(&assert.get_output().stdout);
    let diags = json["diagnostics"].to_string();
    assert!(
        diags.contains("1.7.1-6+deb13u1") && diags.contains("1.7.1-6+deb13u2"),
        "diagnostic names both versions: {json}"
    );
    assert!(
        json["actions"]
            .as_array()
            .unwrap()
            .iter()
            .all(|a| a["action"] != "install" && a["action"] != "upgrade"),
        "no change to an unowned package: {json}"
    );
}

// --- 12-2: version scheme from the backend's vers type (idempotency) --------

/// Install, lock, then re-plan: a fully-applied manifest must plan to nothing,
/// both re-resolved and --frozen. Catches a version read back under the wrong
/// scheme (github state 0.6.4 as generic vs the semver spec would churn).
fn assert_second_run_is_idempotent(dir: &Path) {
    bosun(dir)
        .args(["install"])
        .arg(dir)
        .arg("--yes")
        .assert()
        .success();
    bosun(dir).args(["lock"]).arg(dir).assert().success();
    let replan = bosun(dir)
        .args(["--format", "json", "install"])
        .arg(dir)
        .arg("--plan")
        .assert()
        .code(0);
    assert!(
        stdout_json(&replan.get_output().stdout)["actions"]
            .as_array()
            .unwrap()
            .is_empty(),
        "re-resolved plan is a no-op"
    );
    let frozen = bosun(dir)
        .args(["--format", "json", "install"])
        .arg(dir)
        .args(["--plan", "--frozen"])
        .assert()
        .code(0);
    assert!(
        stdout_json(&frozen.get_output().stdout)["actions"]
            .as_array()
            .unwrap()
            .is_empty(),
        "frozen plan is a no-op"
    );
}

#[test]
fn apt_install_is_idempotent_on_the_second_run() {
    let fixture = r#"{"os":"linux","arch":"amd64","distro":{"id":"debian","codename":"trixie"},
      "privilege":"elevated","user":"root",
      "backends":[{"id":"apt","version":"2.6.1","purl_type":"deb","vers_type":"deb","platform_independent":false,
        "scope":"system","supports_hold":true,"available":{"jq":["1.7.1-6+deb13u1"]}}]}"#;
    let manifest = "version: 2\npackages:\n  - name: jq\n    spec: \">=1.6\"\n    install:\n      linux:debian: {backend: apt}\n";
    assert_second_run_is_idempotent(write_project(fixture, manifest).path());
}

#[test]
fn cargo_install_is_idempotent_on_the_second_run() {
    let fixture = r#"{"os":"linux","arch":"amd64","privilege":"unelevated","user":"t",
      "backends":[{"id":"cargo","version":"1.76.0","purl_type":"cargo","vers_type":"cargo","platform_independent":true,
        "scope":"user","available":{"ripgrep":["14.1.1"]}}]}"#;
    let manifest = "version: 2\npackages:\n  - name: ripgrep\n    spec: \">=14.0.0|<15.0.0\"\n    install:\n      linux: {backend: cargo}\n";
    assert_second_run_is_idempotent(write_project(fixture, manifest).path());
}

#[test]
fn github_release_install_is_idempotent_on_the_second_run() {
    // The regression: github's purl type is `github` (generic) but versions
    // compare as semver. State's 0.6.4 must read back as semver, or the second
    // run plans a spurious change.
    let fixture = r#"{"os":"linux","arch":"amd64","privilege":"unelevated","user":"t",
      "backends":[{"id":"github-release","version":"1.0.0","purl_type":"github","vers_type":"semver","platform_independent":false,
        "scope":"user","available":{"ctxloom/ctxloom":["0.6.4"]}}]}"#;
    let manifest = "version: 2\npackages:\n  - name: ctxloom\n    spec: \">=0.6.4|<0.7.0\"\n    install:\n      linux: {backend: github-release, package: ctxloom/ctxloom}\n";
    assert_second_run_is_idempotent(write_project(fixture, manifest).path());
}
