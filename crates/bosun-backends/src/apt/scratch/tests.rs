use std::time::SystemTime;

use bosun_ports::testing::FakeCommandRunner;
use bosun_ports::Output;

use super::*;
use crate::apt::sources::{parse_deb822, parse_one_line};

/// `plan`'s file at `rel`, as text.
fn file(plan: &Plan, rel: &str) -> String {
    let (_, bytes) = plan
        .files
        .iter()
        .find(|(p, _)| p == rel)
        .unwrap_or_else(|| panic!("no {rel} in {:?}", names(plan)));
    String::from_utf8(bytes.clone()).unwrap()
}

fn names(plan: &Plan) -> Vec<&str> {
    plan.files.iter().map(|(p, _)| p.as_str()).collect()
}

fn sources(plan: &Plan) -> String {
    file(plan, "etc/apt/sources.list.d/target.sources")
}

/// A root holding `files` (relative path, contents).
fn root(files: &[(&str, &str)]) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    for (rel, text) in files {
        let path = dir.path().join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }
    dir
}

const DEBIAN: &str = "\
Types: deb
URIs: http://deb.debian.org/debian
Suites: bookworm
Components: main
Signed-By: /usr/share/keyrings/debian-archive-keyring.gpg
";

#[test]
fn an_entry_whose_keyring_the_root_holds_is_checked_against_it() {
    let r = root(&[("usr/share/keyrings/debian-archive-keyring.gpg", "KEY")]);
    let entries = parse_deb822(&format!(
        "{DEBIAN}\n{}",
        DEBIAN.replace("bookworm", "bookworm-updates")
    ));
    let plan = plan(Some(r.path()), &entries, Arch::Arm64);
    let digest = Plan::digest(&[b"KEY"]);
    let key = format!("keys/{digest}-debian-archive-keyring.gpg");
    assert_eq!(file(&plan, &key), "KEY");
    // One copy, however many entries name it.
    assert_eq!(
        names(&plan)
            .iter()
            .filter(|n| n.starts_with("keys/"))
            .count(),
        1
    );
    assert_eq!(
        sources(&plan),
        format!(
            "Types: deb\nURIs: http://deb.debian.org/debian\nSuites: bookworm\nComponents: main\n\
             Signed-By: {HERE}/{key}\n\n\
             Types: deb\nURIs: http://deb.debian.org/debian\nSuites: bookworm-updates\n\
             Components: main\nSigned-By: {HERE}/{key}\n"
        )
    );
}

#[test]
fn an_entry_with_no_key_to_check_it_is_trusted_over_https_in_the_scratch_root_only() {
    let entries = parse_deb822(DEBIAN);
    for r in [None, Some(root(&[]))] {
        let plan = plan(r.as_ref().map(|r| r.path()), &entries, Arch::Amd64);
        assert_eq!(
            sources(&plan),
            "Types: deb\nURIs: https://deb.debian.org/debian\nSuites: bookworm\n\
             Components: main\nTrusted: yes\n"
        );
    }
    // Without Signed-By and with no trusted keys either.
    let bare = parse_one_line("deb http://archive.ubuntu.com/ubuntu jammy main\n");
    assert!(sources(&plan(None, &bare, Arch::Amd64)).contains("Trusted: yes"));
}

#[test]
fn an_entry_without_a_keyring_is_checked_against_the_roots_trusted_keys() {
    let r = root(&[
        (
            "etc/apt/trusted.gpg.d/ubuntu-keyring-2018-archive.gpg",
            "K1",
        ),
        ("etc/apt/trusted.gpg", "K0"),
    ]);
    let entries = parse_one_line(
        "deb http://archive.ubuntu.com/ubuntu jammy main\n\
         deb [signed-by=0123ABCD] http://archive.ubuntu.com/ubuntu jammy-updates main\n",
    );
    let plan = plan(Some(r.path()), &entries, Arch::Amd64);
    assert_eq!(
        file(
            &plan,
            "etc/apt/trusted.gpg.d/ubuntu-keyring-2018-archive.gpg"
        ),
        "K1"
    );
    assert_eq!(file(&plan, "etc/apt/trusted.gpg"), "K0");
    assert_eq!(
        sources(&plan),
        "Types: deb\nURIs: http://archive.ubuntu.com/ubuntu\nSuites: jammy\nComponents: main\n\n\
         Types: deb\nURIs: http://archive.ubuntu.com/ubuntu\nSuites: jammy-updates\n\
         Components: main\nSigned-By: 0123ABCD\n"
    );
    // A fingerprint with no trusted keys to find it among is no key.
    let alone = plan_of(None, "deb [signed-by=0123ABCD] http://a.example s main\n");
    assert!(
        alone.contains("Trusted: yes") && !alone.contains("Signed-By"),
        "{alone}"
    );
}

fn plan_of(root: Option<&Path>, list: &str) -> String {
    sources(&plan(root, &parse_one_line(list), Arch::Amd64))
}

#[test]
fn a_keyring_and_a_fingerprint_together_keep_both() {
    let r = root(&[("etc/apt/keyrings/v.asc", "A")]);
    let got = plan_of(
        Some(r.path()),
        "deb [signed-by=/etc/apt/keyrings/v.asc,FFFF] https://v.example s main\n",
    );
    let digest = Plan::digest(&[b"A"]);
    assert!(
        got.contains(&format!("Signed-By: {HERE}/keys/{digest}-v.asc FFFF\n")),
        "{got}"
    );
    // One keyring missing is no key.
    let partial = plan_of(
        Some(r.path()),
        "deb [signed-by=/etc/apt/keyrings/v.asc,/etc/apt/keyrings/gone.gpg] http://v.example s main\n",
    );
    assert!(partial.contains("Trusted: yes"), "{partial}");
    assert!(partial.contains("URIs: https://v.example"), "{partial}");
}

#[test]
fn an_inline_key_and_the_targets_own_trust_travel_as_they_are() {
    let inline = parse_deb822(
        "Types: deb\nURIs: http://v.example\nSuites: s\nComponents: main\n\
         Signed-By:\n -----BEGIN PGP PUBLIC KEY BLOCK-----\n .\n abc\n -----END PGP PUBLIC KEY BLOCK-----\n",
    );
    assert_eq!(
        sources(&plan(None, &inline, Arch::Amd64)),
        "Types: deb\nURIs: http://v.example\nSuites: s\nComponents: main\n\
         Signed-By:\n -----BEGIN PGP PUBLIC KEY BLOCK-----\n .\n abc\n -----END PGP PUBLIC KEY BLOCK-----\n"
    );
    let trusted = plan_of(
        None,
        "deb [trusted=yes signed-by=/k.gpg] http://local.example ./\n",
    );
    assert_eq!(
        trusted,
        "Types: deb\nURIs: http://local.example\nSuites: ./\nTrusted: yes\n"
    );
}

#[test]
fn architectures_and_other_options_are_kept() {
    let got = plan_of(
        None,
        "deb [arch=arm64 check-valid-until=no] https://archive.debian.org/debian buster main\n",
    );
    assert_eq!(
        got,
        "Types: deb\nURIs: https://archive.debian.org/debian\nSuites: buster\nComponents: main\n\
         Architectures: arm64\nTrusted: yes\nCheck-Valid-Until: no\n"
    );
}

#[test]
fn the_scratch_configuration_is_its_own() {
    let r = root(&[
        (
            "etc/apt/preferences",
            "Package: *\nPin: release a=x\nPin-Priority: 1\n",
        ),
        ("etc/apt/preferences.d/vendor", "P"),
        (
            "etc/apt/apt.conf.d/docker-clean",
            "APT::Update::Post-Invoke { \"rm\"; };",
        ),
    ]);
    let plan = plan(Some(r.path()), &[], Arch::Arm64);
    assert_eq!(
        file(&plan, "etc/apt/preferences"),
        "Package: *\nPin: release a=x\nPin-Priority: 1\n"
    );
    assert_eq!(file(&plan, "etc/apt/preferences.d/vendor"), "P");
    assert!(
        !names(&plan).iter().any(|n| n.contains("apt.conf.d")),
        "{:?}",
        names(&plan)
    );
    assert_eq!(file(&plan, "status"), "");
    assert_eq!(
        file(&plan, "apt.conf"),
        format!(
            "Dir \"{HERE}/\";\nDir::State::status \"{HERE}/status\";\n\
             APT::Architecture \"arm64\";\nAPT::Architectures {{ \"arm64\"; }};\n\
             Acquire::Languages \"none\";\nAcquire::GzipIndexes \"true\";\n"
        )
    );
}

#[test]
fn a_scratch_root_is_named_for_what_it_holds() {
    let cache = tempfile::tempdir().unwrap();
    let a = plan(
        None,
        &parse_one_line("deb https://a.example s main\n"),
        Arch::Amd64,
    );
    let b = plan(
        None,
        &parse_one_line("deb https://b.example s main\n"),
        Arch::Amd64,
    );
    let s = Scratch::write(cache.path(), &a).unwrap();
    assert_eq!(s, Scratch::write(cache.path(), &a).unwrap());
    assert_ne!(s, Scratch::write(cache.path(), &b).unwrap());
    assert_ne!(
        s,
        Scratch::write(cache.path(), &plan(None, &[], Arch::Amd64)).unwrap()
    );
    let here = s.dir.to_string_lossy().into_owned();
    let conf = fs::read_to_string(s.dir.join("apt.conf")).unwrap();
    assert!(conf.starts_with(&format!("Dir \"{here}/\";")), "{conf}");
    assert!(s.dir.join("var/lib/apt/lists/partial").is_dir());
    assert!(s.dir.join("var/cache/apt/archives/partial").is_dir());
    assert!(s.dir.starts_with(cache.path().join("scratch")));
}

#[test]
fn keys_are_written_as_they_are_and_sources_name_the_scratch_root() {
    let cache = tempfile::tempdir().unwrap();
    let r = root(&[("k.gpg", "@SCRATCH@ bytes")]);
    let p = plan(
        Some(r.path()),
        &parse_one_line("deb [signed-by=/k.gpg] https://a.example s main\n"),
        Arch::Amd64,
    );
    let s = Scratch::write(cache.path(), &p).unwrap();
    let digest = Plan::digest(&[b"@SCRATCH@ bytes"]);
    let key = s.dir.join(format!("keys/{digest}-k.gpg"));
    assert_eq!(fs::read_to_string(&key).unwrap(), "@SCRATCH@ bytes");
    let written = fs::read_to_string(s.dir.join("etc/apt/sources.list.d/target.sources")).unwrap();
    assert!(
        written.contains(&format!("Signed-By: {}\n", key.display())),
        "{written}"
    );
}

const POLICY: &str = "\
libc6:
  Installed: 2.39-0ubuntu8.9
  Candidate: 2.39-0ubuntu8.9
  Version table:
 *** 2.39-0ubuntu8.9 500
        500 http://archive.ubuntu.com/ubuntu noble-updates/main amd64 Packages
        500 http://security.ubuntu.com/ubuntu noble-security/main amd64 Packages
        100 /var/lib/dpkg/status
     2.39-0ubuntu8 500
        500 http://archive.ubuntu.com/ubuntu noble/main amd64 Packages
     2.40-1 100
        100 http://archive.ubuntu.com/ubuntu noble-backports/main amd64 Packages
     2.30 -10
        -10 http://x.example s/main amd64 Packages
";

#[test]
fn apt_cache_policy_gives_each_version_its_priority() {
    let v = |s: &str, p: i32| (s.to_string(), p);
    assert_eq!(
        parse_policy(POLICY),
        vec![
            v("2.39-0ubuntu8.9", 500),
            v("2.39-0ubuntu8", 500),
            v("2.40-1", 100),
            v("2.30", -10),
        ]
    );
    assert!(parse_policy("").is_empty());
    assert!(
        parse_policy("x:\n  Installed: (none)\n  Candidate: (none)\n  Version table:\n").is_empty()
    );
    // Before the table, nothing is a version.
    assert!(parse_policy("x:\n  Candidate: 1 500\n").is_empty());
}

fn ok(stdout: &str) -> Output {
    Output {
        code: Some(0),
        stdout: stdout.as_bytes().to_vec(),
        stderr: Vec::new(),
    }
}

fn failed(stderr: &str) -> Output {
    Output {
        code: Some(100),
        stdout: Vec::new(),
        stderr: stderr.as_bytes().to_vec(),
    }
}

fn scratch(cache: &Path) -> Scratch {
    Scratch::write(cache, &plan(None, &[], Arch::Amd64)).unwrap()
}

#[test]
fn update_runs_apt_get_under_the_scratch_configuration() {
    let cache = tempfile::tempdir().unwrap();
    let s = scratch(cache.path());
    let runner = FakeCommandRunner::new().with_output(ok(""));
    s.update(&runner, false).unwrap();
    let calls = runner.calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].program, "apt-get");
    assert_eq!(calls[0].args, vec!["-q", "update"]);
    assert_eq!(
        calls[0].env,
        vec![
            (
                "APT_CONFIG".to_string(),
                s.dir.join("apt.conf").to_string_lossy().into_owned()
            ),
            ("LC_ALL".to_string(), "C".to_string()),
        ]
    );
    // Fresh now: not run again, unless asked.
    s.update(&runner, false).unwrap();
    assert_eq!(runner.calls().len(), 1);
    let again = FakeCommandRunner::new().with_output(ok(""));
    s.update(&again, true).unwrap();
    assert_eq!(again.calls().len(), 1);
}

#[test]
fn an_update_an_hour_old_runs_again() {
    let cache = tempfile::tempdir().unwrap();
    let s = scratch(cache.path());
    s.update(&FakeCommandRunner::new().with_output(ok("")), false)
        .unwrap();
    fs::File::options()
        .write(true)
        .open(s.stamp())
        .unwrap()
        .set_modified(SystemTime::now() - RELEASE_TTL - std::time::Duration::from_secs(1))
        .unwrap();
    let runner = FakeCommandRunner::new().with_output(ok(""));
    s.update(&runner, false).unwrap();
    assert_eq!(runner.calls().len(), 1);
}

#[test]
fn a_failed_update_is_an_error_unless_an_earlier_one_succeeded() {
    let cache = tempfile::tempdir().unwrap();
    let s = scratch(cache.path());
    let err = s
        .update(
            &FakeCommandRunner::new().with_output(failed("E: not signed\n")),
            false,
        )
        .unwrap_err()
        .to_string();
    assert!(err.contains(errmsg::APT_SCRATCH_UPDATE), "{err}");
    assert!(err.ends_with("E: not signed"), "{err}");
    assert!(!s.stamp().exists());

    s.update(&FakeCommandRunner::new().with_output(ok("")), true)
        .unwrap();
    s.update(
        &FakeCommandRunner::new().with_output(failed("E: offline")),
        true,
    )
    .unwrap();

    let missing = s.update(&FakeCommandRunner::new(), true).unwrap_err();
    assert!(
        matches!(missing, PortError::CommandNotFound { .. }),
        "{missing}"
    );
}

#[test]
fn policy_asks_apt_cache_for_the_package() {
    let cache = tempfile::tempdir().unwrap();
    let s = scratch(cache.path());
    let runner = FakeCommandRunner::new().with_output(ok(POLICY));
    assert_eq!(s.policy(&runner, "libc6").unwrap().len(), 4);
    let call = &runner.calls()[0];
    assert_eq!(call.program, "apt-cache");
    assert_eq!(call.args, vec!["policy", "libc6"]);
    assert_eq!(call.env[0].0, "APT_CONFIG");

    let err = s
        .policy(&FakeCommandRunner::new().with_output(failed("E: bad")), "x")
        .unwrap_err()
        .to_string();
    assert!(err.contains("apt-cache") && err.contains("E: bad"), "{err}");
}

#[test]
fn a_deb822_field_continues_on_indented_lines() {
    let mut out = String::new();
    field(&mut out, "A", "one");
    field(&mut out, "B", "\nx\n\ny");
    field(&mut out, "C", "");
    assert_eq!(out, "A: one\nB:\n x\n .\n y\nC:\n");
}

#[test]
fn keyrings_are_paths_and_the_rest_fingerprints() {
    assert_eq!(
        keyrings("/a.gpg, ABCD /b.gpg"),
        Some((vec!["/a.gpg", "/b.gpg"], vec!["ABCD"]))
    );
    assert_eq!(keyrings("\n-----BEGIN PGP PUBLIC KEY BLOCK-----\nx"), None);
}
