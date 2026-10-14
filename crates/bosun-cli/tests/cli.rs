//! Integration tests: run the real `bosun` binary against a fixture (the
//! `BOSUN_FIXTURE` seam), never the real host package managers (§10). State is
//! redirected to a temp dir via `BOSUN_STATE_DIR`.

use std::fs;
use std::path::Path;

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

const MANIFEST: &str = r#"version: 1
targets: [linux/amd64]
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
    let mut cmd = Command::cargo_bin("bosun").unwrap();
    cmd.env("BOSUN_FIXTURE", dir.join("fixture.json"))
        .env("BOSUN_STATE_DIR", dir.join("state"));
    cmd
}

fn stdout_json(output: &[u8]) -> Value {
    serde_json::from_slice(output).expect("stdout is json")
}

#[test]
fn version_is_reported() {
    let out = Command::cargo_bin("bosun")
        .unwrap()
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
fn lock_check_reports_missing_then_fresh() {
    let dir = project();
    // Before locking, the declared target is missing → exit 2.
    let before = bosun(dir.path())
        .args(["--format", "json", "lock"])
        .arg(dir.path())
        .arg("--check")
        .assert()
        .code(2);
    let json = stdout_json(&before.get_output().stdout);
    assert_eq!(json["targets"][0]["freshness"], "missing");

    // Lock, then check again → fresh, exit 0.
    bosun(dir.path())
        .args(["lock"])
        .arg(dir.path())
        .assert()
        .success();
    let after = bosun(dir.path())
        .args(["--format", "json", "lock"])
        .arg(dir.path())
        .arg("--check")
        .assert()
        .code(0);
    let json = stdout_json(&after.get_output().stdout);
    assert_eq!(json["targets"][0]["freshness"], "fresh");
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
fn installer_preference_overrides_manifest_order() {
    let manifest = "version: 1\ntargets: [linux/amd64]\npackages:\n  - name: fd\n    spec: \">=1.0.0|<2.0.0\"\n    install:\n      linux: {backends: [apt, cargo]}\n";

    // Default: the manifest lists apt first, so apt wins.
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
    let manifest = "version: 1\ntargets: [linux/amd64]\npackages:\n  - name: mac-only\n    spec: \">=1.0.0|<2.0.0\"\n    install:\n      darwin: {backend: cargo}\n";
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
