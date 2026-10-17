//! Over real captures: Debian 12's `bookworm`, `bookworm-updates` and
//! `bookworm-security` `InRelease` files and their `main/binary-amd64`
//! `Packages.xz`, fetched 2026-10-16 and trimmed to a few packages. Each
//! trimmed `Packages` has its own size and hashes written into the trimmed
//! `InRelease`, so the hash chain holds; the signatures are the originals,
//! which no longer match the trimmed text (they are not checked).

use std::cell::RefCell;
use std::sync::Once;
use std::time::{Duration, SystemTime};

use bosun_core::{Os, Purl};
use bosun_ports::testing::FakeHttpFetcher;
use bosun_ports::HttpResponse;

use super::*;

const BOOKWORM: &[u8] = include_bytes!("../../../tests/fixtures/apt/bookworm.InRelease");
const BOOKWORM_XZ: &[u8] = include_bytes!("../../../tests/fixtures/apt/bookworm.Packages.xz");
const UPDATES: &[u8] = include_bytes!("../../../tests/fixtures/apt/bookworm-updates.InRelease");
const UPDATES_XZ: &[u8] =
    include_bytes!("../../../tests/fixtures/apt/bookworm-updates.Packages.xz");
const SECURITY: &[u8] = include_bytes!("../../../tests/fixtures/apt/bookworm-security.InRelease");
const SECURITY_XZ: &[u8] =
    include_bytes!("../../../tests/fixtures/apt/bookworm-security.Packages.xz");

const AMD64: Target = Target::Platform {
    os: Os::Linux,
    arch: Arch::Amd64,
};

fn text(bytes: &[u8]) -> String {
    String::from_utf8(bytes.to_vec()).unwrap()
}

fn decompressed(xz: &[u8]) -> String {
    let mut out = Vec::new();
    lzma_rs::xz_decompress(&mut &xz[..], &mut out).unwrap();
    String::from_utf8(out).unwrap()
}

// ---- release files -----------------------------------------------------------

#[test]
fn signed_text_is_the_message_between_the_armor_headers_and_the_signature() {
    let body = signed_text(&text(BOOKWORM));
    assert!(body.starts_with("Origin: Debian\n"), "{body}");
    assert!(body.ends_with("main/binary-arm64/Packages.xz"), "{body}");
    assert!(!body.contains("PGP"), "{body}");
}

#[test]
fn signed_text_undoes_dash_escaping_and_passes_plain_text_through() {
    let signed = format!("{SIGNED}\nHash: SHA256\n\nSuite: x\n- -dash\n{SIGNATURE}\nsig\n");
    assert_eq!(signed_text(&signed), "Suite: x\n-dash");
    assert_eq!(signed_text("Suite: x\n"), "Suite: x\n");
    // A clearsigned file cut short has no message.
    assert_eq!(signed_text(SIGNED), "");
}

#[test]
fn a_real_release_gives_its_suite_codename_by_hash_and_sha256_section() {
    let r = parse_release(&signed_text(&text(BOOKWORM))).unwrap();
    assert_eq!(r.suite.as_deref(), Some("oldstable"));
    assert_eq!(r.codename.as_deref(), Some("bookworm"));
    assert!(r.by_hash);
    assert_eq!(
        r.sha256["main/binary-amd64/Packages.xz"],
        FileHash {
            sha256: hex_sha256(BOOKWORM_XZ),
            size: BOOKWORM_XZ.len() as u64,
        }
    );
    // The real arm64 line, untouched, and no MD5 sums read as SHA256 ones.
    assert_eq!(r.sha256["main/binary-arm64/Packages.xz"].size, 8689464);
    assert!(r.sha256.values().all(|h| h.sha256.len() == 64));
    assert_eq!(r.sha256.len(), 3);
}

#[test]
fn a_release_without_acquire_by_hash_is_fetched_by_path() {
    let r = parse_release("Codename: x\nAcquire-By-Hash: no\n").unwrap();
    assert!(!r.by_hash);
    assert!(!parse_release("Codename: x\n").unwrap().by_hash);
}

#[test]
fn a_malformed_sha256_line_is_an_error() {
    for bad in [
        "SHA256:\n abc 12\n",
        "SHA256:\n abc twelve main/x\n",
        "SHA256:\n abc 12 main/x extra\n",
    ] {
        let err = parse_release(bad).unwrap_err().to_string();
        assert!(err.contains(errmsg::APT_BAD_RELEASE), "{bad:?}: {err}");
    }
}

#[test]
fn only_the_sha256_section_is_read_for_hashes() {
    let r = parse_release(
        "MD5Sum:\n 0123 5 main/x\nSHA256:\n ABCD 7 main/x\nDescription: d\n more text here\n",
    )
    .unwrap();
    assert_eq!(
        r.sha256,
        BTreeMap::from([(
            "main/x".to_string(),
            FileHash {
                sha256: "abcd".to_string(),
                size: 7
            }
        )])
    );
}

#[test]
fn a_release_is_for_the_suite_it_names_by_suite_or_codename() {
    let bookworm = parse_release(&signed_text(&text(BOOKWORM))).unwrap();
    assert!(bookworm.is_for("bookworm"));
    assert!(bookworm.is_for("oldstable"));
    assert!(!bookworm.is_for("trixie"));
    let security = parse_release(&signed_text(&text(SECURITY))).unwrap();
    assert!(security.is_for("bookworm-security"));
}

// ---- package indexes ---------------------------------------------------------

fn listed<'a>(available: &'a Available, name: &str) -> Vec<&'a str> {
    available[name].iter().map(|(v, _)| v.as_str()).collect()
}

#[test]
fn a_real_package_index_gives_each_package_its_version() {
    let mut available = Available::new();
    add_package_versions(&decompressed(BOOKWORM_XZ), 500, &mut available);
    assert_eq!(available["jq"], vec![("1.6-2.1+deb12u2".to_string(), 500)]);
    assert_eq!(listed(&available, "fd-find"), vec!["8.6.0-3"]);
    // Architecture: all packages are in each arch's index.
    assert_eq!(listed(&available, "tzdata"), vec!["2026b-0+deb12u1"]);
    assert_eq!(available.len(), 5);
}

#[test]
fn versions_gather_across_indexes_once_each() {
    let mut available = Available::new();
    for xz in [BOOKWORM_XZ, UPDATES_XZ, SECURITY_XZ, SECURITY_XZ] {
        add_package_versions(&decompressed(xz), 500, &mut available);
    }
    assert_eq!(
        listed(&available, "openssl"),
        vec!["3.0.20-1~deb12u2", "3.0.17-1~deb12u2", "3.0.22-1~deb12u1"]
    );
}

#[test]
fn a_version_listed_twice_keeps_its_highest_priority() {
    let mut available = Available::new();
    add_package_versions("Package: a\nVersion: 1\n", 100, &mut available);
    add_package_versions("Package: a\nVersion: 1\n", 500, &mut available);
    add_package_versions("Package: a\nVersion: 1\n", 1, &mut available);
    assert_eq!(available["a"], vec![("1".to_string(), 500)]);
}

#[test]
fn a_stanza_needs_both_package_and_version() {
    let mut available = Available::new();
    add_package_versions(
        "Package: a\nVersion: 1\n\nPackage: b\n\nVersion: 3\n\nPackage: c\nVersion: 2",
        500,
        &mut available,
    );
    assert_eq!(
        available,
        BTreeMap::from([
            ("a".to_string(), vec![("1".to_string(), 500)]),
            ("c".to_string(), vec![("2".to_string(), 500)]),
        ])
    );
}

#[test]
fn apt_considers_only_its_highest_priority_versions() {
    let v = |list: &[(&str, i32)]| -> Vec<(String, i32)> {
        list.iter().map(|(v, p)| (v.to_string(), *p)).collect()
    };
    assert_eq!(
        candidates(&v(&[("2", 100), ("1", 500), ("1.1", 500)])),
        vec!["1", "1.1"]
    );
    assert_eq!(candidates(&v(&[("2", 100)])), vec!["2"]);
    assert_eq!(candidates(&v(&[("2", 1), ("1", 0)])), vec!["2"]);
    assert_eq!(candidates(&v(&[("1", 0)])), vec!["1"]);
    assert!(candidates(&v(&[("1", -1)])).is_empty());
    assert!(candidates(&[]).is_empty());
}

#[test]
fn a_release_gives_apts_default_priority() {
    let r = |text: &str| parse_release(text).unwrap().priority();
    assert_eq!(r("Suite: stable\n"), 500);
    assert_eq!(r("NotAutomatic: yes\nButAutomaticUpgrades: yes\n"), 100);
    assert_eq!(r("NotAutomatic: yes\n"), 1);
    assert_eq!(r("NotAutomatic: no\nButAutomaticUpgrades: yes\n"), 500);
    assert_eq!(r("NotAutomatic: yes\nButAutomaticUpgrades: no\n"), 1);
}

// ---- sources -----------------------------------------------------------------

#[test]
fn debian_reads_main_from_its_release_updates_and_security_suites() {
    let s = default_sources("debian", "bookworm", Arch::Arm64).unwrap();
    assert_eq!(
        s.suites,
        vec![
            suite(DEBIAN, "bookworm".to_string(), &["main"], true),
            suite(DEBIAN, "bookworm-updates".to_string(), &["main"], false),
            suite(
                DEBIAN_SECURITY,
                "bookworm-security".to_string(),
                &["main"],
                false
            ),
        ]
    );
}

#[test]
fn ubuntu_reads_four_components_from_the_archive_or_ports() {
    let amd = default_sources("ubuntu", "noble", Arch::Amd64).unwrap();
    assert!(amd
        .suites
        .iter()
        .all(|s| s.components == ["main", "restricted", "universe", "multiverse"]));
    assert_eq!(amd.suites[0].mirror, UBUNTU);
    assert_eq!(amd.suites[1].suite, "noble-updates");
    assert_eq!(amd.suites[2].mirror, UBUNTU_SECURITY);
    let arm = default_sources("ubuntu", "noble", Arch::Arm64).unwrap();
    assert!(arm.suites.iter().all(|s| s.mirror == UBUNTU_PORTS));
    assert_eq!(arm.suites[2].suite, "noble-security");
}

#[test]
fn a_distro_without_a_known_mirror_has_no_sources() {
    assert_eq!(default_sources("linuxmint", "wilma", Arch::Amd64), None);
}

#[test]
fn a_suite_is_under_dists_and_a_flat_repository_at_its_path() {
    let s = |suite: &str| Suite {
        mirror: "https://m.example/r".to_string(),
        suite: suite.to_string(),
        components: vec!["main".to_string(), "contrib".to_string()],
        required: true,
    };
    assert_eq!(s("stable").dist_url(), "https://m.example/r/dists/stable");
    assert_eq!(s("./").dist_url(), "https://m.example/r");
    assert_eq!(s("/").dist_url(), "https://m.example/r");
    assert_eq!(s("sub/").dist_url(), "https://m.example/r/sub");
    assert_eq!(s("./sub/").dist_url(), "https://m.example/r/sub");
    assert_eq!(
        s("stable").index_dirs(Arch::Arm64),
        vec!["main/binary-arm64", "contrib/binary-arm64"]
    );
    assert_eq!(s("./").index_dirs(Arch::Arm64), vec![""]);
}

#[test]
fn http_mirrors_are_read_over_https() {
    assert_eq!(
        https("http://deb.debian.org/debian"),
        "https://deb.debian.org/debian"
    );
    assert_eq!(https("https://a.example"), "https://a.example");
    assert_eq!(https("file:/srv/repo"), "file:/srv/repo");
}

#[test]
fn a_targets_entries_give_its_suites_for_the_arch() {
    let entries = sources::parse_one_line(
        "\
deb http://archive.ubuntu.com/ubuntu/ jammy main restricted
deb http://archive.ubuntu.com/ubuntu/ jammy universe main
deb [arch=arm64] http://ports.ubuntu.com/ubuntu-ports jammy main
deb [arch=amd64] https://vendor.example/apt stable main
deb https://flat.example/repo ./
",
    );
    let amd = sources_from(&entries, Arch::Amd64);
    assert_eq!(
        amd.suites,
        vec![
            suite(
                "https://archive.ubuntu.com/ubuntu",
                "jammy".to_string(),
                &["main", "restricted", "universe"],
                true
            ),
            suite(
                "https://vendor.example/apt",
                "stable".to_string(),
                &["main"],
                true
            ),
            suite("https://flat.example/repo", "./".to_string(), &[], true),
        ]
    );
    let arm = sources_from(&entries, Arch::Arm64);
    assert_eq!(
        arm.suites[1].mirror,
        "https://ports.ubuntu.com/ubuntu-ports"
    );
    assert_eq!(arm.suites.len(), 3);
}

#[test]
fn a_mirror_is_cached_under_its_host_and_path() {
    assert_eq!(cache_name(DEBIAN), "deb.debian.org_debian");
    assert_eq!(cache_name("http://m:8080/a/"), "m_8080_a");
    assert_eq!(cache_name("plain"), "plain");
}

// ---- the index ---------------------------------------------------------------

fn by_hash(mirror: &str, suite: &str, xz: &[u8]) -> String {
    format!(
        "{mirror}/dists/{suite}/main/binary-amd64/by-hash/SHA256/{}",
        hex_sha256(xz)
    )
}

fn bookworm_mirror() -> FakeHttpFetcher {
    FakeHttpFetcher::new()
        .with_body(format!("{DEBIAN}/dists/bookworm/InRelease"), BOOKWORM)
        .with_body(by_hash(DEBIAN, "bookworm", BOOKWORM_XZ), BOOKWORM_XZ)
        .with_body(
            format!("{DEBIAN}/dists/bookworm-updates/InRelease"),
            UPDATES,
        )
        .with_body(by_hash(DEBIAN, "bookworm-updates", UPDATES_XZ), UPDATES_XZ)
        .with_body(
            format!("{DEBIAN_SECURITY}/dists/bookworm-security/InRelease"),
            SECURITY,
        )
        .with_body(
            by_hash(DEBIAN_SECURITY, "bookworm-security", SECURITY_XZ),
            SECURITY_XZ,
        )
}

fn status(code: u16) -> HttpResponse {
    HttpResponse {
        status: code,
        body: Vec::new(),
    }
}

fn index(fetcher: &Arc<FakeHttpFetcher>, cache: &Path) -> RemoteAptIndex {
    RemoteAptIndex::new(fetcher.clone(), cache.to_path_buf())
}

fn deb(purl: &str) -> Ref {
    Ref::new(purl.parse::<Purl>().unwrap())
}

const JQ: &str = "pkg:deb/debian/jq?arch=amd64&distro=bookworm";

fn versions(index: &RemoteAptIndex, purl: &str) -> Vec<String> {
    let mut v: Vec<String> = index
        .query_available(&deb(purl), &AMD64)
        .unwrap()
        .iter()
        .map(|v| v.as_str().to_string())
        .collect();
    v.sort();
    v
}

fn query_err(index: &RemoteAptIndex, purl: &str) -> String {
    index
        .query_available(&deb(purl), &AMD64)
        .unwrap_err()
        .to_string()
}

#[test]
fn a_release_offers_every_suites_versions() {
    let dir = tempfile::tempdir().unwrap();
    let fetcher = Arc::new(bookworm_mirror());
    let idx = index(&fetcher, dir.path());
    assert_eq!(
        versions(&idx, JQ),
        vec!["1.6-2.1+deb12u2", "1.6-2.1+deb12u3"]
    );
    assert_eq!(
        versions(&idx, "pkg:deb/debian/openssl?arch=amd64&distro=bookworm"),
        vec!["3.0.17-1~deb12u2", "3.0.20-1~deb12u2", "3.0.22-1~deb12u1"]
    );
    assert!(versions(&idx, "pkg:deb/debian/just?arch=amd64&distro=bookworm").is_empty());
    // One fetch per file, however many queries.
    assert_eq!(fetcher.calls().len(), 6);
}

#[test]
fn a_release_file_is_fresh_for_an_hour() {
    assert_eq!(RELEASE_TTL, Duration::from_secs(3600));
}

#[test]
fn a_cached_release_is_used_until_it_is_an_hour_old() {
    let dir = tempfile::tempdir().unwrap();
    let fetcher = Arc::new(bookworm_mirror());
    versions(&index(&fetcher, dir.path()), JQ);
    assert_eq!(fetcher.calls().len(), 6);

    // A later run reads it all from the cache.
    assert_eq!(versions(&index(&fetcher, dir.path()), JQ).len(), 2);
    assert_eq!(fetcher.calls().len(), 6);

    // An hour on, the release files are fetched again; the indexes they list
    // are the cached ones still, so only those three are fetched.
    let an_hour_ago = SystemTime::now() - RELEASE_TTL - Duration::from_secs(1);
    for suite in ["bookworm", "bookworm-updates"] {
        let f = dir
            .path()
            .join("deb.debian.org_debian")
            .join(suite)
            .join("InRelease");
        fs::File::options()
            .write(true)
            .open(&f)
            .unwrap()
            .set_modified(an_hour_ago)
            .unwrap();
    }
    versions(&index(&fetcher, dir.path()), JQ);
    let urls: Vec<String> = fetcher.calls()[6..].iter().map(|c| c.url.clone()).collect();
    assert_eq!(
        urls,
        vec![
            format!("{DEBIAN}/dists/bookworm/InRelease"),
            format!("{DEBIAN}/dists/bookworm-updates/InRelease"),
        ]
    );
}

#[test]
fn refresh_fetches_every_release_file_again() {
    let dir = tempfile::tempdir().unwrap();
    let fetcher = Arc::new(bookworm_mirror());
    let idx = index(&fetcher, dir.path());
    versions(&idx, JQ);
    idx.refresh().unwrap();
    versions(&idx, JQ);
    assert_eq!(fetcher.calls().len(), 9);
}

#[test]
fn a_cached_index_that_no_longer_matches_is_fetched_again() {
    let dir = tempfile::tempdir().unwrap();
    let fetcher = Arc::new(bookworm_mirror());
    versions(&index(&fetcher, dir.path()), JQ);
    let cached = dir
        .path()
        .join("deb.debian.org_debian/bookworm/main_binary-amd64_Packages.xz");
    fs::write(&cached, b"stale").unwrap();
    assert_eq!(versions(&index(&fetcher, dir.path()), JQ).len(), 2);
    assert_eq!(fetcher.calls().len(), 7);
    assert_eq!(fs::read(&cached).unwrap(), BOOKWORM_XZ);
}

#[test]
fn an_index_that_does_not_match_its_release_file_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let tampered =
        bookworm_mirror().with_body(by_hash(DEBIAN, "bookworm", BOOKWORM_XZ), UPDATES_XZ);
    let err = query_err(&index(&Arc::new(tampered), dir.path()), JQ);
    assert!(err.contains(errmsg::APT_HASH_MISMATCH), "{err}");
    assert!(err.contains(&hex_sha256(BOOKWORM_XZ)), "{err}");
    assert!(!dir
        .path()
        .join("deb.debian.org_debian/bookworm/main_binary-amd64_Packages.xz")
        .exists());
}

#[test]
fn an_index_of_the_right_hash_but_the_wrong_size_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let release = text(BOOKWORM).replace(
        &format!(
            "{}     2056 main/binary-amd64/Packages.xz",
            hex_sha256(BOOKWORM_XZ)
        ),
        &format!(
            "{}     2057 main/binary-amd64/Packages.xz",
            hex_sha256(BOOKWORM_XZ)
        ),
    );
    let fetcher =
        bookworm_mirror().with_body(format!("{DEBIAN}/dists/bookworm/InRelease"), release);
    let err = query_err(&index(&Arc::new(fetcher), dir.path()), JQ);
    assert!(err.contains(errmsg::APT_HASH_MISMATCH), "{err}");
}

#[test]
fn an_index_is_fetched_by_path_when_the_release_offers_no_hashes() {
    let dir = tempfile::tempdir().unwrap();
    let release = text(BOOKWORM).replace("Acquire-By-Hash: yes\n", "");
    let fetcher = bookworm_mirror()
        .with_body(format!("{DEBIAN}/dists/bookworm/InRelease"), release)
        .with_body(
            format!("{DEBIAN}/dists/bookworm/main/binary-amd64/Packages.xz"),
            BOOKWORM_XZ,
        );
    assert_eq!(
        versions(&index(&Arc::new(fetcher), dir.path()), JQ).len(),
        2
    );
}

#[test]
fn a_failed_index_fetch_names_the_url_and_status() {
    let dir = tempfile::tempdir().unwrap();
    let fetcher =
        bookworm_mirror().with_response(by_hash(DEBIAN, "bookworm", BOOKWORM_XZ), status(503));
    let err = query_err(&index(&Arc::new(fetcher), dir.path()), JQ);
    assert!(err.contains("by-hash") && err.contains("503"), "{err}");
}

#[test]
fn missing_updates_and_security_suites_are_skipped() {
    let dir = tempfile::tempdir().unwrap();
    let fetcher = bookworm_mirror()
        .with_response(
            format!("{DEBIAN}/dists/bookworm-updates/InRelease"),
            status(404),
        )
        .with_response(
            format!("{DEBIAN_SECURITY}/dists/bookworm-security/InRelease"),
            status(404),
        );
    assert_eq!(
        versions(&index(&Arc::new(fetcher), dir.path()), JQ),
        vec!["1.6-2.1+deb12u2"]
    );
}

#[test]
fn a_release_not_on_its_mirror_is_an_error_naming_it() {
    let dir = tempfile::tempdir().unwrap();
    let fetcher = FakeHttpFetcher::new()
        .with_response(format!("{DEBIAN}/dists/bookwrom/InRelease"), status(404));
    let err = query_err(
        &index(&Arc::new(fetcher), dir.path()),
        "pkg:deb/debian/jq?arch=amd64&distro=bookwrom",
    );
    assert!(err.contains(errmsg::APT_NO_RELEASE), "{err}");
    assert!(
        err.contains("bookwrom at https://deb.debian.org/debian"),
        "{err}"
    );
}

#[test]
fn a_release_file_for_another_suite_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let fetcher = bookworm_mirror().with_body(
        format!("{DEBIAN}/dists/bookworm-updates/InRelease"),
        BOOKWORM,
    );
    let err = query_err(&index(&Arc::new(fetcher), dir.path()), JQ);
    assert!(err.contains(errmsg::APT_RELEASE_MISMATCH), "{err}");
    assert!(err.ends_with("is bookworm"), "{err}");
}

#[test]
fn an_error_status_for_a_release_file_is_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let fetcher = bookworm_mirror().with_response(
        format!("{DEBIAN}/dists/bookworm-updates/InRelease"),
        status(500),
    );
    let err = query_err(&index(&Arc::new(fetcher), dir.path()), JQ);
    assert!(
        err.contains("bookworm-updates/InRelease") && err.contains("500"),
        "{err}"
    );
}

#[test]
fn an_unreachable_mirror_falls_back_to_the_release_file_fetched_before() {
    let dir = tempfile::tempdir().unwrap();
    versions(&index(&Arc::new(bookworm_mirror()), dir.path()), JQ);
    let file = dir.path().join("deb.debian.org_debian/bookworm/InRelease");
    fs::File::options()
        .write(true)
        .open(&file)
        .unwrap()
        .set_modified(SystemTime::now() - RELEASE_TTL * 2)
        .unwrap();
    // Nothing is mapped: every fetch fails.
    let offline = Arc::new(FakeHttpFetcher::new());
    let log = logged(|| {
        assert_eq!(versions(&index(&offline, dir.path()), JQ).len(), 2);
    });
    assert!(log.contains(logmsg::APT_STALE_RELEASE), "{log}");

    // With nothing fetched before, it is an error.
    let empty = tempfile::tempdir().unwrap();
    let err = query_err(&index(&offline, empty.path()), JQ);
    assert!(err.contains("no fake response"), "{err}");
}

#[test]
fn reading_a_release_warns_of_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let idx = index(&Arc::new(bookworm_mirror()), dir.path());
    let log = logged(|| {
        versions(&idx, JQ);
    });
    assert!(!log.contains("WARN"), "{log}");
}

#[test]
fn a_release_with_no_index_for_the_arch_is_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let err = query_err(
        &index(&Arc::new(bookworm_mirror()), dir.path()),
        "pkg:deb/debian/jq?arch=arm64&distro=bookworm",
    );
    // The arm64 index is listed, but only for the release, not served here.
    assert!(err.contains("arm64"), "{err}");

    let none = text(BOOKWORM).replace(
        "main/binary-arm64/Packages.xz",
        "main/binary-arm64/Packages.bz2",
    );
    let dir = tempfile::tempdir().unwrap();
    let fetcher = Arc::new(
        FakeHttpFetcher::new()
            .with_body(format!("{DEBIAN}/dists/bookworm/InRelease"), none)
            .with_response(
                format!("{DEBIAN}/dists/bookworm-updates/InRelease"),
                status(404),
            )
            .with_response(
                format!("{DEBIAN_SECURITY}/dists/bookworm-security/InRelease"),
                status(404),
            ),
    );
    let err = query_err(
        &index(&fetcher, dir.path()),
        "pkg:deb/debian/jq?arch=arm64&distro=bookworm",
    );
    assert!(err.contains(errmsg::APT_NO_INDEX), "{err}");
    assert!(
        err.ends_with("bookworm at https://deb.debian.org/debian for arm64"),
        "{err}"
    );
}

fn gzip(bytes: &[u8]) -> Vec<u8> {
    use std::io::Write;
    let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    e.write_all(bytes).unwrap();
    e.finish().unwrap()
}

#[test]
fn a_gzip_index_is_read_before_an_xz_one() {
    let dir = tempfile::tempdir().unwrap();
    let gz = gzip(decompressed(BOOKWORM_XZ).as_bytes());
    let line = format!(
        " {} {:8} main/binary-amd64/Packages.gz\n",
        hex_sha256(&gz),
        gz.len()
    );
    let release = text(BOOKWORM).replacen("SHA256:\n", &format!("SHA256:\n{line}"), 1);
    let fetcher = Arc::new(
        bookworm_mirror()
            .with_body(format!("{DEBIAN}/dists/bookworm/InRelease"), release)
            .with_body(by_hash(DEBIAN, "bookworm", &gz), gz.clone()),
    );
    assert_eq!(versions(&index(&fetcher, dir.path()), JQ).len(), 2);
    let urls: Vec<String> = fetcher.calls().iter().map(|c| c.url.clone()).collect();
    assert!(urls.contains(&by_hash(DEBIAN, "bookworm", &gz)), "{urls:?}");
    assert!(
        !urls.contains(&by_hash(DEBIAN, "bookworm", BOOKWORM_XZ)),
        "{urls:?}"
    );
}

#[test]
fn an_index_file_is_the_gzip_one_else_the_xz_one() {
    let hash = FileHash {
        sha256: "ab".to_string(),
        size: 1,
    };
    let mut r = Release::default();
    assert_eq!(index_file(&r, "main/binary-amd64"), None);
    r.sha256
        .insert("main/binary-amd64/Packages.xz".to_string(), hash.clone());
    r.sha256
        .insert("main/binary-amd64/Packages".to_string(), hash.clone());
    assert_eq!(
        index_file(&r, "main/binary-amd64"),
        Some((
            "main/binary-amd64/Packages.xz".to_string(),
            &hash,
            Compression::Xz
        ))
    );
    r.sha256
        .insert("main/binary-amd64/Packages.gz".to_string(), hash.clone());
    assert_eq!(
        index_file(&r, "main/binary-amd64").map(|(p, _, c)| (p, c)),
        Some((
            "main/binary-amd64/Packages.gz".to_string(),
            Compression::Gzip
        ))
    );
}

#[test]
fn gzip_members_are_all_read() {
    let mut two = gzip(b"Package: a\nVersion: 1\n\n");
    two.extend(gzip(b"Package: b\nVersion: 2\n"));
    assert_eq!(
        Compression::Gzip.decompress(&two).unwrap(),
        b"Package: a\nVersion: 1\n\nPackage: b\nVersion: 2\n"
    );
    assert!(Compression::Gzip.decompress(b"not gzip").is_err());
    assert!(Compression::Xz.decompress(b"not xz").is_err());
}

#[test]
fn a_corrupt_index_is_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let junk = b"not xz at all".to_vec();
    let release = text(BOOKWORM).replace(
        &format!(
            "{}     2056 main/binary-amd64/Packages.xz",
            hex_sha256(BOOKWORM_XZ)
        ),
        &format!(
            "{} {:8} main/binary-amd64/Packages.xz",
            hex_sha256(&junk),
            junk.len()
        ),
    );
    let fetcher = bookworm_mirror()
        .with_body(format!("{DEBIAN}/dists/bookworm/InRelease"), release)
        .with_body(by_hash(DEBIAN, "bookworm", &junk), junk);
    let err = query_err(&index(&Arc::new(fetcher), dir.path()), JQ);
    assert!(err.contains(errmsg::APT_DECOMPRESS), "{err}");
}

#[test]
fn the_release_comes_from_the_identity() {
    let dir = tempfile::tempdir().unwrap();
    let idx = index(&Arc::new(FakeHttpFetcher::new()), dir.path());
    for purl in [
        "pkg:deb/jq?arch=amd64&distro=bookworm",
        "pkg:deb/debian/jq?arch=amd64",
        "pkg:deb/debian/jq?arch=s390x&distro=bookworm",
    ] {
        let err = query_err(&idx, purl);
        assert!(err.contains(errmsg::APT_NO_DISTRO), "{purl}: {err}");
    }
    let err = query_err(&idx, "pkg:deb/linuxmint/jq?arch=amd64&distro=wilma");
    assert!(
        err.contains(errmsg::APT_NO_MIRROR) && err.ends_with("linuxmint"),
        "{err}"
    );
    // Without an arch qualifier the target's arch is read; `any` has none.
    let any = idx
        .query_available(&deb("pkg:deb/debian/jq?distro=bookworm"), &Target::Any)
        .unwrap_err()
        .to_string();
    assert!(any.contains(errmsg::APT_NO_DISTRO), "{any}");
}

#[test]
fn without_an_arch_qualifier_the_targets_arch_is_used() {
    let dir = tempfile::tempdir().unwrap();
    let idx = index(&Arc::new(bookworm_mirror()), dir.path());
    assert_eq!(versions(&idx, "pkg:deb/debian/jq?distro=bookworm").len(), 2);
}

#[test]
fn an_unwritable_cache_is_an_error_naming_it() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("a-file");
    fs::write(&file, b"").unwrap();
    let err = query_err(&index(&Arc::new(bookworm_mirror()), &file), JQ);
    assert!(err.contains(errmsg::APT_CACHE), "{err}");
}

thread_local! {
    static CAPTURED: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
}

/// Writes to this thread's [`CAPTURED`] buffer.
struct ThisThread;

impl std::io::Write for ThisThread {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        CAPTURED.with(|c| c.borrow_mut().extend_from_slice(b));
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// What `f` logs at warn and above. One global subscriber writes each test
/// thread's lines to that thread's buffer: a scoped one would race the other
/// tests for the interest tracing caches per callsite.
fn logged(f: impl FnOnce()) -> String {
    static GLOBAL: Once = Once::new();
    GLOBAL.call_once(|| {
        let subscriber = tracing_subscriber::fmt()
            .with_writer(|| ThisThread)
            .with_ansi(false)
            .with_max_level(tracing::Level::WARN)
            .finish();
        tracing::subscriber::set_global_default(subscriber).expect("the only global subscriber");
    });
    CAPTURED.with(|c| c.borrow_mut().clear());
    f();
    CAPTURED.with(|c| String::from_utf8(c.take()).unwrap())
}

// ---- the target's root -------------------------------------------------------

const VENDOR: &str = "https://vendor.example/apt";

/// A root whose `etc/apt` holds `sources.list.d/<name>` with `text`.
fn root_with(name: &str, text: &str) -> tempfile::TempDir {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("etc/apt/sources.list.d");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join(name), text).unwrap();
    root
}

fn rooted(fetcher: &Arc<FakeHttpFetcher>, cache: &Path, root: &Path) -> RemoteAptIndex {
    index(fetcher, cache).with_root(Some(TargetRoot {
        distro: "debian".to_string(),
        codename: "bookworm".to_string(),
        path: root.to_path_buf(),
    }))
}

/// A flat repository at `VENDOR` listing `packages` in its `Packages.gz`,
/// with `release` fields before its SHA256 section.
fn flat_vendor(fetcher: FakeHttpFetcher, release: &str, packages: &str) -> FakeHttpFetcher {
    let gz = gzip(packages.as_bytes());
    let inrelease = format!(
        "{release}SHA256:\n {} {} Packages.gz\n",
        hex_sha256(&gz),
        gz.len()
    );
    fetcher
        .with_body(format!("{VENDOR}/InRelease"), inrelease)
        .with_body(format!("{VENDOR}/Packages.gz"), gz)
}

#[test]
fn the_targets_own_sources_are_read_and_its_vendor_repositories_with_them() {
    let root = root_with(
        "debian.sources",
        "Types: deb\nURIs: http://deb.debian.org/debian\nSuites: bookworm\nComponents: main\n",
    );
    fs::write(
        root.path().join("etc/apt/sources.list.d/vendor.list"),
        format!("deb [signed-by=/etc/apt/keyrings/v.gpg] {VENDOR} ./\n"),
    )
    .unwrap();
    let fetcher = Arc::new(flat_vendor(
        bookworm_mirror(),
        "Origin: vendor\n",
        "Package: tool\nVersion: 2.0-1\n\nPackage: jq\nVersion: 1.7.1-9vendor\n",
    ));
    let dir = tempfile::tempdir().unwrap();
    let idx = rooted(&fetcher, dir.path(), root.path());
    assert_eq!(
        versions(&idx, "pkg:deb/debian/tool?arch=amd64&distro=bookworm"),
        vec!["2.0-1"]
    );
    // Only the suites the root lists: the release, not its updates or security.
    assert_eq!(versions(&idx, JQ), vec!["1.6-2.1+deb12u2", "1.7.1-9vendor"]);
    let urls: Vec<String> = fetcher.calls().iter().map(|c| c.url.clone()).collect();
    assert!(urls.iter().all(|u| u.starts_with("https://")), "{urls:?}");
    assert!(
        !urls.iter().any(|u| u.contains("bookworm-updates")),
        "{urls:?}"
    );
}

#[test]
fn a_suite_apt_picks_only_when_asked_offers_only_what_nothing_else_does() {
    let root = root_with(
        "a.list",
        &format!("deb https://deb.debian.org/debian bookworm main\ndeb {VENDOR} ./\n"),
    );
    let fetcher = Arc::new(flat_vendor(
        bookworm_mirror(),
        "NotAutomatic: yes\nButAutomaticUpgrades: yes\n",
        "Package: jq\nVersion: 9.9\n\nPackage: only-here\nVersion: 1.0\n",
    ));
    let dir = tempfile::tempdir().unwrap();
    let idx = rooted(&fetcher, dir.path(), root.path());
    assert_eq!(versions(&idx, JQ), vec!["1.6-2.1+deb12u2"]);
    assert_eq!(
        versions(&idx, "pkg:deb/debian/only-here?arch=amd64&distro=bookworm"),
        vec!["1.0"]
    );
}

#[test]
fn a_root_with_no_source_for_the_arch_is_an_error() {
    let root = root_with(
        "a.list",
        "deb [arch=arm64] https://deb.debian.org/debian bookworm main\n",
    );
    let dir = tempfile::tempdir().unwrap();
    let err = query_err(
        &rooted(&Arc::new(bookworm_mirror()), dir.path(), root.path()),
        JQ,
    );
    assert!(err.contains(errmsg::APT_NO_SOURCES), "{err}");
    assert!(err.ends_with("for amd64"), "{err}");
}

#[test]
fn another_release_than_the_roots_reads_a_stock_images_sources() {
    let root = root_with("a.list", &format!("deb {VENDOR} ./\n"));
    let dir = tempfile::tempdir().unwrap();
    let idx = index(&Arc::new(bookworm_mirror()), dir.path()).with_root(Some(TargetRoot {
        distro: "debian".to_string(),
        codename: "trixie".to_string(),
        path: root.path().to_path_buf(),
    }));
    assert_eq!(versions(&idx, JQ).len(), 2);
    let ubuntu = index(&Arc::new(bookworm_mirror()), dir.path()).with_root(Some(TargetRoot {
        distro: "ubuntu".to_string(),
        codename: "bookworm".to_string(),
        path: root.path().to_path_buf(),
    }));
    assert_eq!(versions(&ubuntu, JQ).len(), 2);
}

#[test]
fn a_flat_repository_is_fetched_by_hash_when_it_offers_it() {
    let root = root_with("a.list", &format!("deb {VENDOR} ./\n"));
    let gz = gzip(b"Package: jq\nVersion: 1\n");
    let release = format!(
        "Acquire-By-Hash: yes\nSHA256:\n {} {} Packages.gz\n",
        hex_sha256(&gz),
        gz.len()
    );
    let fetcher = Arc::new(
        FakeHttpFetcher::new()
            .with_body(format!("{VENDOR}/InRelease"), release)
            .with_body(format!("{VENDOR}/by-hash/SHA256/{}", hex_sha256(&gz)), gz),
    );
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(
        versions(&rooted(&fetcher, dir.path(), root.path()), JQ),
        vec!["1"]
    );
}
