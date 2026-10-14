//! The github-release backend (§12): bosun *is* the installer — it fetches a
//! release archive, verifies its checksum, and extracts the binary. Mutations
//! are `Fetch` + `Extract` steps a host executor (or the container renderer)
//! runs; the query side lists release tags over HTTP.
//!
//! One release carries several per-binary archives (ctxloom, ltk, taskloom,
//! harp), so an asset is picked by **(binary, os, arch)** against goreleaser's
//! default archive name `<binary>_<version>_<os>_<arch>.tar.gz`. (bosun could
//! not fetch a real ctxloom release listing — the repo is not public — so the
//! fixture uses that documented default; slice-12 dogfood confirms it.)
//!
//! The binary name (which differs from the repo for the shared ctxloom repo)
//! is carried in the package id's `kind`, defaulting to the repo's last path
//! segment.
//!
//! Versions are the tag minus a leading `v`, compared as SemVer; GitHub
//! pre-releases are listed, and the selection guard (§5.6) decides. The
//! checksum is enforced, never skipped (§12). The downloaded binary is never
//! run during verification.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use bosun_core::{Arch, BackendId, Capabilities, Os, Ref, Scheme, Scope, Source, Target, Version};
use bosun_ports::{
    Backend, BackendFactory, HostInfo, HttpFetcher, HttpRequest, HttpResponse, Index, Installer,
    PortError, Probe, Step, Steps,
};
use tracing::warn;

use crate::logmsg;

const BACKEND: &str = "github-release";

/// Builds the github-release backend. Always present (bosun does the fetching).
pub struct GithubReleaseFactory {
    fetcher: Arc<dyn HttpFetcher>,
    bin_dir: PathBuf,
    os: Os,
    arch: Arch,
}

impl GithubReleaseFactory {
    /// Construct with the HTTP fetcher, the install bin dir (default
    /// `~/.local/bin`, resolved by the caller), and the host os/arch.
    pub fn new(
        fetcher: Arc<dyn HttpFetcher>,
        bin_dir: PathBuf,
        os: Os,
        arch: Arch,
    ) -> GithubReleaseFactory {
        GithubReleaseFactory {
            fetcher,
            bin_dir,
            os,
            arch,
        }
    }
}

impl BackendFactory for GithubReleaseFactory {
    fn id(&self) -> BackendId {
        BackendId::new(BACKEND)
    }

    fn probe(&self, _host: &HostInfo) -> Result<Option<Probe>, PortError> {
        // bosun is the installer, so this backend is always available.
        Ok(Some(Probe {
            backend: BackendId::new(BACKEND),
            version: Version::parse("1.0.0", Scheme::Semver).unwrap_or_else(|_| unreachable!()),
        }))
    }

    fn build(&self, _probe: Probe) -> Backend {
        if needs_path_warning(&self.bin_dir, &std::env::var("PATH").unwrap_or_default()) {
            warn!(message = logmsg::BIN_DIR_NOT_ON_PATH, bin_dir = %self.bin_dir.display());
        }
        let backend = GithubReleaseBackend {
            fetcher: self.fetcher.clone(),
            bin_dir: self.bin_dir.clone(),
            os: self.os,
            arch: self.arch,
            token: std::env::var("GITHUB_TOKEN").ok(),
        };
        Backend {
            index: Box::new(backend.clone()),
            installer: Box::new(backend),
        }
    }
}

/// Whether `bin_dir` is one of the `:`-separated entries of `path_var`.
fn on_path(bin_dir: &Path, path_var: &str) -> bool {
    path_var
        .split(':')
        .any(|p| !p.is_empty() && Path::new(p) == bin_dir)
}

/// Whether to warn that `bin_dir` is not on `$PATH`. Split out from `build` so
/// the condition is tested rather than only logged.
fn needs_path_warning(bin_dir: &Path, path_var: &str) -> bool {
    !on_path(bin_dir, path_var)
}

#[derive(Clone)]
struct GithubReleaseBackend {
    fetcher: Arc<dyn HttpFetcher>,
    bin_dir: PathBuf,
    os: Os,
    arch: Arch,
    token: Option<String>,
}

#[derive(Debug, serde::Deserialize)]
struct Release {
    tag_name: String,
    #[serde(default)]
    assets: Vec<ReleaseAsset>,
}

#[derive(Debug, serde::Deserialize)]
struct ReleaseAsset {
    name: String,
    browser_download_url: String,
}

/// A GitHub API request carrying the standard headers and, when a non-empty
/// `token` is given, `Authorization` (which raises the 60 req/h anon limit).
fn api_request(url: &str, token: Option<&str>) -> HttpRequest {
    let mut headers = vec![
        (
            "Accept".to_string(),
            "application/vnd.github+json".to_string(),
        ),
        ("User-Agent".to_string(), "bosun".to_string()),
    ];
    if let Some(token) = token {
        if !token.is_empty() {
            headers.push(("Authorization".to_string(), format!("Bearer {token}")));
        }
    }
    HttpRequest {
        url: url.to_string(),
        headers,
    }
}

/// Interpret a GitHub API response, turning 403 (rate limit) into an error
/// that says how to raise the limit.
fn check_status(url: &str, resp: &HttpResponse) -> Result<(), PortError> {
    match resp.status {
        s if (200..300).contains(&s) => Ok(()),
        403 => Err(PortError::Backend {
            detail: format!(
                "{url}: GitHub rate limit (403). Set GITHUB_TOKEN to raise the 60 req/h anonymous limit."
            ),
        }),
        s => Err(PortError::HttpStatus { url: url.to_string(), status: s }),
    }
}

fn version_from_tag(tag: &str) -> Option<Version> {
    Version::parse(tag.strip_prefix('v').unwrap_or(tag), Scheme::Semver).ok()
}

impl GithubReleaseBackend {
    /// The binary name: the package id's `kind`, else the repo's last segment.
    fn binary_of(pkg: &Ref) -> String {
        pkg.package.kind.clone().unwrap_or_else(|| {
            pkg.package
                .id
                .rsplit('/')
                .next()
                .unwrap_or(&pkg.package.id)
                .to_string()
        })
    }

    /// Pick the one asset matching (binary, os, arch) by goreleaser's default
    /// name; error (listing candidates) when zero or several match.
    fn pick_archive<'a>(
        &self,
        binary: &str,
        assets: &'a [ReleaseAsset],
    ) -> Result<&'a ReleaseAsset, PortError> {
        let prefix = format!("{}_", binary.to_ascii_lowercase());
        let suffix = format!("_{}_{}.tar.gz", self.os, self.arch);
        let matches: Vec<&ReleaseAsset> = assets
            .iter()
            .filter(|a| {
                let n = a.name.to_ascii_lowercase();
                n.starts_with(&prefix) && n.ends_with(&suffix)
            })
            .collect();
        match matches.as_slice() {
            [one] => Ok(one),
            found => {
                let candidates: Vec<&str> = assets.iter().map(|a| a.name.as_str()).collect();
                Err(PortError::Backend {
                    detail: format!(
                        "{} asset(s) match {binary} for {}/{} (expected 1); candidates: {:?}",
                        found.len(),
                        self.os,
                        self.arch,
                        candidates
                    ),
                })
            }
        }
    }

    /// The sha256 recorded for `asset_name` in a `checksums.txt` body.
    fn checksum_for(body: &str, asset_name: &str) -> Option<String> {
        body.lines().find_map(|line| {
            let mut it = line.split_whitespace();
            let sha = it.next()?;
            let name = it.next()?;
            // goreleaser writes `<sha>  <name>`; names may have a `*` prefix.
            (name.trim_start_matches('*') == asset_name).then(|| sha.to_string())
        })
    }

    fn fetch_release(&self, repo: &str, version: &Version) -> Result<Release, PortError> {
        // Try a `v`-prefixed tag first, then the bare version.
        for tag in [
            format!("v{}", version.as_str()),
            version.as_str().to_string(),
        ] {
            let url = format!("https://api.github.com/repos/{repo}/releases/tags/{tag}");
            let resp = self
                .fetcher
                .fetch(&api_request(&url, self.token.as_deref()))?;
            if resp.status == 404 {
                continue;
            }
            check_status(&url, &resp)?;
            return serde_json::from_slice(&resp.body).map_err(|e| PortError::Backend {
                detail: format!("{url}: {e}"),
            });
        }
        Err(PortError::Backend {
            detail: format!("no release tagged v{0} or {0} in {repo}", version.as_str()),
        })
    }
}

impl Index for GithubReleaseBackend {
    fn refresh(&self) -> Result<(), PortError> {
        Ok(()) // releases are fetched fresh per query
    }

    fn query_available(&self, pkg: &Ref, _target: &Target) -> Result<Vec<Version>, PortError> {
        let repo = &pkg.package.id;
        let mut versions = Vec::new();
        for page in 1.. {
            let url =
                format!("https://api.github.com/repos/{repo}/releases?per_page=100&page={page}");
            let resp = self
                .fetcher
                .fetch(&api_request(&url, self.token.as_deref()))?;
            check_status(&url, &resp)?;
            let releases: Vec<Release> =
                serde_json::from_slice(&resp.body).map_err(|e| PortError::Backend {
                    detail: format!("{url}: {e}"),
                })?;
            let count = releases.len();
            versions.extend(
                releases
                    .iter()
                    .filter_map(|r| version_from_tag(&r.tag_name)),
            );
            if count < 100 {
                break; // last page
            }
        }
        Ok(versions)
    }
}

impl Installer for GithubReleaseBackend {
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            supports_exact_version: true,
            supports_hold: false, // a fetched binary isn't auto-updated; no PM hold
            supports_downgrade: true,
        }
    }

    fn scope(&self) -> Scope {
        Scope::User
    }

    fn ensure_source(&self, _source: &Source) -> Result<Steps, PortError> {
        Ok(vec![]) // the repo in the purl is the source
    }

    fn install_exact(&self, pkg: &Ref, version: &Version) -> Result<Steps, PortError> {
        let repo = pkg.package.id.clone();
        let binary = Self::binary_of(pkg);
        let release = self.fetch_release(&repo, version)?;
        let archive = self.pick_archive(&binary, &release.assets)?;

        // Enforce the checksum (§12): checksums.txt must exist and list it.
        let sums = release
            .assets
            .iter()
            .find(|a| a.name == "checksums.txt")
            .ok_or_else(|| PortError::Backend {
                detail: format!("no checksums.txt in {repo} release for {binary}"),
            })?;
        let resp = self.fetcher.fetch(&api_request(
            &sums.browser_download_url,
            self.token.as_deref(),
        ))?;
        check_status(&sums.browser_download_url, &resp)?;
        let sha256 = Self::checksum_for(&String::from_utf8_lossy(&resp.body), &archive.name)
            .ok_or_else(|| PortError::Backend {
                detail: format!("{} not listed in checksums.txt", archive.name),
            })?;

        let download = self.bin_dir.join(".bosun-cache").join(&archive.name);
        Ok(vec![
            Step::Fetch {
                url: archive.browser_download_url.clone(),
                sha256,
                dest: download.clone(),
            },
            Step::Extract {
                archive: download,
                member: binary.clone(),
                dest: self.bin_dir.join(&binary),
                mode: 0o755,
            },
        ])
    }

    fn verify_installed(&self, pkg: &Ref) -> Result<Option<Version>, PortError> {
        // Presence only: the binary exists at the recorded path. bosun state is
        // the version authority (§4.6), and we never run a binary we didn't
        // build. `Some` means present; its version is the presence marker, not
        // a real reading — the apply layer records the resolved version.
        let path = self.bin_dir.join(Self::binary_of(pkg));
        Ok(path.exists().then(present_marker))
    }

    fn hold(&self, _pkg: &Ref, _version: &Version) -> Result<Steps, PortError> {
        Ok(vec![])
    }

    fn downgrade(&self, pkg: &Ref, version: &Version) -> Result<Steps, PortError> {
        self.install_exact(pkg, version)
    }

    fn uninstall(&self, pkg: &Ref) -> Result<Steps, PortError> {
        // Removing a fetched binary is deleting the file; modelled as a step.
        Ok(vec![Step::Run {
            argv: vec![
                "rm".to_string(),
                "-f".to_string(),
                self.bin_dir
                    .join(Self::binary_of(pkg))
                    .display()
                    .to_string(),
            ],
            env: vec![],
            scope: Scope::User,
        }])
    }
}

/// The version `verify_installed` returns to signal "present" (see its docs).
fn present_marker() -> Version {
    Version::parse("0.0.0", Scheme::Semver).unwrap_or_else(|_| unreachable!())
}

#[cfg(test)]
mod tests {
    use super::*;
    use bosun_core::PackageId;
    use bosun_ports::testing::FakeHttpFetcher;

    fn sem(v: &str) -> Version {
        Version::parse(v, Scheme::Semver).unwrap()
    }

    fn backend(fetcher: Arc<dyn HttpFetcher>, bin_dir: PathBuf) -> GithubReleaseBackend {
        GithubReleaseBackend {
            fetcher,
            bin_dir,
            os: Os::Linux,
            arch: Arch::Amd64,
            token: None,
        }
    }

    /// A github package: repo via id, binary via kind.
    fn ghr(repo: &str, binary: &str) -> Ref {
        Ref::new(PackageId::with_kind(repo, binary))
    }

    #[test]
    fn on_path_matches_exact_entry() {
        let dir = Path::new("/home/x/.local/bin");
        assert!(on_path(dir, "/usr/bin:/home/x/.local/bin:/bin"));
        assert!(!on_path(dir, "/usr/bin:/bin"));
        assert!(!on_path(dir, ""));
    }

    #[test]
    fn binary_defaults_to_repo_segment() {
        assert_eq!(
            GithubReleaseBackend::binary_of(&Ref::new(PackageId::new("owner/repo"))),
            "repo"
        );
        assert_eq!(
            GithubReleaseBackend::binary_of(&ghr("ctxloom/ctxloom", "ltk")),
            "ltk"
        );
    }

    #[test]
    fn picks_the_matching_archive() {
        let b = backend(Arc::new(FakeHttpFetcher::new()), PathBuf::from("/b"));
        let assets = vec![
            asset("ltk_0.7.0_linux_amd64.tar.gz"),
            asset("ltk_0.7.0_linux_arm64.tar.gz"),
            asset("ltk_0.7.0_darwin_amd64.tar.gz"),
            asset("ctxloom_0.7.0_linux_amd64.tar.gz"),
            asset("checksums.txt"),
        ];
        let got = b.pick_archive("ltk", &assets).unwrap();
        assert_eq!(got.name, "ltk_0.7.0_linux_amd64.tar.gz");
    }

    #[test]
    fn picker_errors_on_zero_and_lists_candidates() {
        let b = backend(Arc::new(FakeHttpFetcher::new()), PathBuf::from("/b"));
        let assets = vec![asset("ltk_0.7.0_windows_amd64.tar.gz")];
        let err = b.pick_archive("ltk", &assets).unwrap_err();
        assert!(matches!(err, PortError::Backend { detail }
            if detail.contains("0 asset") && detail.contains("windows_amd64")));
    }

    #[test]
    fn checksum_lookup() {
        let body = "abc123  ltk_0.7.0_linux_amd64.tar.gz\ndef456  other.tar.gz\n";
        assert_eq!(
            GithubReleaseBackend::checksum_for(body, "ltk_0.7.0_linux_amd64.tar.gz"),
            Some("abc123".to_string())
        );
        assert_eq!(
            GithubReleaseBackend::checksum_for(body, "absent.tar.gz"),
            None
        );
    }

    fn asset(name: &str) -> ReleaseAsset {
        ReleaseAsset {
            name: name.to_string(),
            browser_download_url: format!("https://dl/{name}"),
        }
    }

    #[test]
    fn install_exact_builds_fetch_then_extract_with_checksum() {
        let tag_url = "https://api.github.com/repos/ctxloom/ctxloom/releases/tags/v0.7.0";
        let release = r#"{"tag_name":"v0.7.0","assets":[
            {"name":"ltk_0.7.0_linux_amd64.tar.gz","browser_download_url":"https://dl/ltk.tgz"},
            {"name":"checksums.txt","browser_download_url":"https://dl/checksums.txt"}
        ]}"#;
        let sums = "deadbeef  ltk_0.7.0_linux_amd64.tar.gz\n";
        let fetcher = Arc::new(
            FakeHttpFetcher::new()
                .with_body(tag_url, release)
                .with_body("https://dl/checksums.txt", sums),
        );
        let b = backend(fetcher, PathBuf::from("/home/x/.local/bin"));
        let steps = b
            .install_exact(&ghr("ctxloom/ctxloom", "ltk"), &sem("0.7.0"))
            .unwrap();
        assert_eq!(
            steps,
            vec![
                Step::Fetch {
                    url: "https://dl/ltk.tgz".into(),
                    sha256: "deadbeef".into(),
                    dest: PathBuf::from(
                        "/home/x/.local/bin/.bosun-cache/ltk_0.7.0_linux_amd64.tar.gz"
                    ),
                },
                Step::Extract {
                    archive: PathBuf::from(
                        "/home/x/.local/bin/.bosun-cache/ltk_0.7.0_linux_amd64.tar.gz"
                    ),
                    member: "ltk".into(),
                    dest: PathBuf::from("/home/x/.local/bin/ltk"),
                    mode: 0o755,
                },
            ]
        );
    }

    #[test]
    fn install_exact_errors_when_checksum_missing() {
        let tag_url = "https://api.github.com/repos/o/r/releases/tags/v0.7.0";
        // Release has the archive but no checksums.txt asset.
        let release = r#"{"tag_name":"v0.7.0","assets":[
            {"name":"r_0.7.0_linux_amd64.tar.gz","browser_download_url":"https://dl/r.tgz"}
        ]}"#;
        let fetcher = Arc::new(FakeHttpFetcher::new().with_body(tag_url, release));
        let b = backend(fetcher, PathBuf::from("/b"));
        let err = b
            .install_exact(&Ref::new(PackageId::new("o/r")), &sem("0.7.0"))
            .unwrap_err();
        assert!(matches!(err, PortError::Backend { detail } if detail.contains("checksums.txt")));
    }

    #[test]
    fn query_available_lists_tags_including_prereleases() {
        let url = "https://api.github.com/repos/o/r/releases?per_page=100&page=1";
        let body = r#"[{"tag_name":"v0.7.0","assets":[]},
                       {"tag_name":"v0.7.0-pre1","assets":[]},
                       {"tag_name":"v0.6.0","assets":[]}]"#;
        let fetcher = Arc::new(FakeHttpFetcher::new().with_body(url, body));
        let got = backend(fetcher, PathBuf::from("/b"))
            .query_available(
                &Ref::new(PackageId::new("o/r")),
                &"linux/amd64".parse().unwrap(),
            )
            .unwrap();
        assert_eq!(got, vec![sem("0.7.0"), sem("0.7.0-pre1"), sem("0.6.0")]);
    }

    #[test]
    fn rate_limit_error_mentions_the_token() {
        let url = "https://api.github.com/repos/o/r/releases?per_page=100&page=1";
        let fetcher = Arc::new(FakeHttpFetcher::new().with_response(
            url,
            HttpResponse {
                status: 403,
                body: vec![],
            },
        ));
        let err = backend(fetcher, PathBuf::from("/b"))
            .query_available(
                &Ref::new(PackageId::new("o/r")),
                &"linux/amd64".parse().unwrap(),
            )
            .unwrap_err();
        assert!(matches!(err, PortError::Backend { detail } if detail.contains("GITHUB_TOKEN")));
    }

    #[test]
    fn verify_installed_is_presence_only() {
        let dir = tempfile::tempdir().unwrap();
        let b = backend(Arc::new(FakeHttpFetcher::new()), dir.path().to_path_buf());
        let pkg = ghr("ctxloom/ctxloom", "ltk");
        assert_eq!(b.verify_installed(&pkg).unwrap(), None);
        std::fs::write(dir.path().join("ltk"), b"bin").unwrap();
        assert_eq!(b.verify_installed(&pkg).unwrap(), Some(present_marker()));
    }

    #[test]
    fn needs_path_warning_when_not_on_path() {
        let dir = Path::new("/home/x/.local/bin");
        assert!(needs_path_warning(dir, "/usr/bin:/bin"));
        assert!(!needs_path_warning(dir, "/usr/bin:/home/x/.local/bin"));
    }

    #[test]
    fn api_request_adds_auth_only_for_a_non_empty_token() {
        let has_auth = |t: Option<&str>| {
            api_request("https://api", t)
                .headers
                .iter()
                .any(|(k, _)| k == "Authorization")
        };
        assert!(has_auth(Some("ghp_x")));
        assert!(!has_auth(None));
        assert!(!has_auth(Some("")));
        // The User-Agent is always present (GitHub requires one).
        assert!(api_request("https://api", None)
            .headers
            .iter()
            .any(|(k, _)| k == "User-Agent"));
    }

    #[test]
    fn probe_is_always_present() {
        let factory = GithubReleaseFactory::new(
            Arc::new(FakeHttpFetcher::new()),
            PathBuf::from("/b"),
            Os::Linux,
            Arch::Amd64,
        );
        let runner = bosun_ports::testing::FakeCommandRunner::new();
        let host = HostInfo {
            os: Os::Linux,
            arch: Arch::Amd64,
            distro: None,
            runner: &runner,
        };
        let probe = factory.probe(&host).unwrap().unwrap();
        assert_eq!(probe.backend.as_str(), "github-release");
    }

    #[test]
    fn query_available_paginates() {
        let page1: String = format!(
            "[{}]",
            (0..100)
                .map(|i| format!("{{\"tag_name\":\"v1.{i}.0\",\"assets\":[]}}"))
                .collect::<Vec<_>>()
                .join(",")
        );
        let fetcher = Arc::new(
            FakeHttpFetcher::new()
                .with_body(
                    "https://api.github.com/repos/o/r/releases?per_page=100&page=1",
                    page1,
                )
                .with_body(
                    "https://api.github.com/repos/o/r/releases?per_page=100&page=2",
                    r#"[{"tag_name":"v2.0.0","assets":[]}]"#,
                ),
        );
        let got = backend(fetcher, PathBuf::from("/b"))
            .query_available(
                &Ref::new(PackageId::new("o/r")),
                &"linux/amd64".parse().unwrap(),
            )
            .unwrap();
        // 100 from page 1 + 1 from page 2: a full page must trigger the next.
        assert_eq!(got.len(), 101);
        assert!(got.contains(&sem("2.0.0")));
    }

    #[test]
    fn uninstall_removes_the_binary() {
        let b = backend(
            Arc::new(FakeHttpFetcher::new()),
            PathBuf::from("/home/x/.local/bin"),
        );
        assert_eq!(
            b.uninstall(&ghr("ctxloom/ctxloom", "ltk")).unwrap(),
            vec![Step::Run {
                argv: vec!["rm".into(), "-f".into(), "/home/x/.local/bin/ltk".into()],
                env: vec![],
                scope: Scope::User,
            }]
        );
    }

    #[test]
    fn downgrade_is_install_exact() {
        let tag_url = "https://api.github.com/repos/ctxloom/ctxloom/releases/tags/v0.7.0";
        let release = r#"{"tag_name":"v0.7.0","assets":[
            {"name":"ltk_0.7.0_linux_amd64.tar.gz","browser_download_url":"https://dl/ltk.tgz"},
            {"name":"checksums.txt","browser_download_url":"https://dl/checksums.txt"}
        ]}"#;
        let fetcher = Arc::new(
            FakeHttpFetcher::new()
                .with_body(tag_url, release)
                .with_body(
                    "https://dl/checksums.txt",
                    "deadbeef  ltk_0.7.0_linux_amd64.tar.gz\n",
                ),
        );
        let b = backend(fetcher, PathBuf::from("/b"));
        let pkg = ghr("ctxloom/ctxloom", "ltk");
        // Same steps as install (downgrade just re-fetches a lower version).
        assert_eq!(
            b.downgrade(&pkg, &sem("0.7.0")).unwrap(),
            b.install_exact(&pkg, &sem("0.7.0")).unwrap()
        );
        assert_eq!(b.downgrade(&pkg, &sem("0.7.0")).unwrap().len(), 2);
    }
}
