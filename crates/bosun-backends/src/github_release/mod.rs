//! The github-release backend (§12): bosun *is* the installer — it fetches a
//! release archive, verifies its checksum, and extracts the binary. Mutations
//! are `Fetch` + `Extract` steps a host executor (or the container renderer)
//! runs; the query side lists release tags over HTTP.
//!
//! An asset is picked by **exact name**: the `asset` qualifier (the manifest's
//! `ext.github-release.asset`) rendered as a minijinja template for this host, else
//! goreleaser's default archive name `<binary>_<version>_<os>_<arch>.tar.gz`
//! (see [`template`]). The binary is located inside the archive by name, at
//! any depth, when the `Extract` step runs.
//!
//! The binary name (which can differ from the repo, as `rg` does for ripgrep)
//! is carried in the purl's `binary` qualifier, defaulting to the repo's last
//! path segment.
//!
//! Versions are the tag minus a leading `v`, compared as SemVer; GitHub
//! pre-releases are listed, and the selection guard (§5.6) decides. A checksum
//! the release publishes is enforced (§12); with none published, bosun
//! installs anyway and warns (see [`checksum`]). The downloaded binary is
//! never run during verification.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use bosun_core::{
    Arch, BackendId, Capabilities, Checksum, Installed, Os, Ref, Scheme, Scope, Source, Target,
    Version,
};
use bosun_ports::{
    ArchiveFormat, Backend, BackendFactory, HostInfo, HttpFetcher, HttpRequest, HttpResponse,
    Index, Installer, PortError, Probe, Step, Steps,
};
use tracing::warn;

use crate::{errmsg, logmsg};

mod checksum;
mod template;

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
    fn meta(&self) -> bosun_ports::BackendMeta {
        bosun_ports::BackendMeta {
            // github tags compare as semver though the purl type is `github` (§6).
            purl_type: "github",
            vers_type: "semver",
            platform_independent: false,
            ext_keys: &["asset"],
        }
    }

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
    /// The binary name: the purl's `binary` qualifier, else the purl's name
    /// (the repo segment). The shared ctxloom repo would set `binary` to keep
    /// its tools distinct; a single-binary repo needs nothing.
    fn binary_of(pkg: &Ref) -> String {
        pkg.purl
            .qualifiers()
            .get("binary")
            .cloned()
            .unwrap_or_else(|| pkg.purl.name().to_string())
    }

    /// The expected archive name: the `asset` qualifier rendered as a template
    /// (see [`template`]), else goreleaser's default
    /// `{{ binary }}_{{ version }}_{{ os }}_{{ arch }}.tar.gz`. `tag` is the
    /// release's actual tag. A bad template is an error, never a fallback.
    fn expected_asset(
        &self,
        pkg: &Ref,
        binary: &str,
        version: &Version,
        tag: &str,
    ) -> Result<String, PortError> {
        let source = pkg
            .purl
            .qualifiers()
            .get("asset")
            .map_or(template::DEFAULT_TEMPLATE, String::as_str);
        template::render(
            source,
            &template::AssetVars {
                binary,
                version: version.as_str(),
                tag,
                os: self.os,
                arch: self.arch,
            },
        )
    }

    /// Pick the asset whose name is **exactly** `expected` (not a prefix, so a
    /// `_full` variant never matches the plain archive); error listing the
    /// candidates when none (or, defensively, several) match.
    fn pick_archive<'a>(
        &self,
        expected: &str,
        assets: &'a [ReleaseAsset],
    ) -> Result<&'a ReleaseAsset, PortError> {
        let matches: Vec<&ReleaseAsset> = assets.iter().filter(|a| a.name == expected).collect();
        match matches.as_slice() {
            [one] => Ok(one),
            found => {
                let candidates: Vec<&str> = assets.iter().map(|a| a.name.as_str()).collect();
                Err(PortError::Backend {
                    detail: format!(
                        "{} asset(s) named {expected:?} (expected 1); candidates: {candidates:?}",
                        found.len(),
                    ),
                })
            }
        }
    }

    /// The release's published sha256 for `asset`, or `None` when it
    /// publishes none for it (see [`checksum`]). A sums file that can't be
    /// fetched is an error; one that doesn't list the asset is not.
    fn published_sha256(
        &self,
        assets: &[ReleaseAsset],
        asset: &str,
    ) -> Result<Option<String>, PortError> {
        let names: Vec<&str> = assets.iter().map(|a| a.name.as_str()).collect();
        for file in checksum::sums_files(asset, &names) {
            let Some(url) = assets
                .iter()
                .find(|a| a.name == file.name)
                .map(|a| a.browser_download_url.as_str())
            else {
                continue;
            };
            let resp = self
                .fetcher
                .fetch(&api_request(url, self.token.as_deref()))?;
            check_status(url, &resp)?;
            let body = String::from_utf8_lossy(&resp.body);
            if let Some(sha) = checksum::sha256_in(&body, asset, file.per_asset) {
                return Ok(Some(sha));
            }
        }
        Ok(None)
    }

    /// Resolve the archive for this host at `version` and its published
    /// sha256, when the release publishes one. Returns `(download_url,
    /// archive_name, format, sha256)`. Shared by `install_exact` (which
    /// fetches and extracts it) and the query-side `checksum` (which records
    /// it), so an asset that is not a binary fails at resolve.
    fn archive_and_checksum(
        &self,
        pkg: &Ref,
        version: &Version,
    ) -> Result<(String, String, ArchiveFormat, Option<String>), PortError> {
        let repo = pkg.purl.native_id();
        let binary = Self::binary_of(pkg);
        let release = self.fetch_release(&repo, version)?;
        let expected = self.expected_asset(pkg, &binary, version, &release.tag_name)?;
        let format = ArchiveFormat::of(&expected).ok_or_else(|| PortError::Backend {
            detail: format!("{repo}: {expected:?} is {}", errmsg::NOT_A_BINARY),
        })?;
        let archive = self.pick_archive(&expected, &release.assets)?;
        let sha256 = self.published_sha256(&release.assets, &archive.name)?;
        Ok((
            archive.browser_download_url.clone(),
            archive.name.clone(),
            format,
            sha256,
        ))
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
        let repo = pkg.purl.native_id();
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

    fn checksum(
        &self,
        pkg: &Ref,
        version: &Version,
        _target: &Target,
    ) -> Result<Option<Checksum>, PortError> {
        let (_, _, _, sha256) = self.archive_and_checksum(pkg, version)?;
        Ok(sha256.map(Checksum))
    }
}

impl Installer for GithubReleaseBackend {
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            supports_exact_version: true,
            // bosun owns the fetched binary and nothing else upgrades it, so a
            // hold is unnecessary (§5.2).
            hold: bosun_core::HoldSupport::NotNeeded,
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
        let binary = Self::binary_of(pkg);
        let (url, archive_name, format, sha256) = self.archive_and_checksum(pkg, version)?;
        if sha256.is_none() {
            warn!(
                message = logmsg::NO_CHECKSUM,
                repo = %pkg.purl.native_id(),
                asset = %archive_name,
            );
        }
        let download = self.bin_dir.join(".bosun-cache").join(&archive_name);
        Ok(vec![
            Step::Fetch {
                url,
                sha256,
                dest: download.clone(),
            },
            Step::Extract {
                format,
                archive: download,
                member: binary.clone(),
                dest: self.bin_dir.join(&binary),
                mode: 0o755,
            },
        ])
    }

    fn verify_installed(&self, pkg: &Ref) -> Result<Installed, PortError> {
        // Presence only: the binary exists at the recorded path. bosun never
        // runs a binary it didn't build, and `Present { version: None }` carries
        // no version — bosun state is the version authority (§4.6), so this can
        // never be mistaken for a version in a comparison.
        let path = self.bin_dir.join(Self::binary_of(pkg));
        Ok(if path.exists() {
            Installed::Present { version: None }
        } else {
            Installed::Absent
        })
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

#[cfg(test)]
mod tests {
    use super::*;
    use bosun_ports::testing::FakeHttpFetcher;

    // Real ctxloom v0.6.4 data (ctxloom/ctxloom; only `ctxloom` ships, no
    // pre-releases). See tests/fixtures/github/README once vendored.
    const RELEASES: &str = include_str!("../../tests/fixtures/github/ctxloom-releases-page1.json");
    const CHECKSUMS: &str =
        include_str!("../../tests/fixtures/github/ctxloom-v0.6.4-checksums.txt");

    fn sem(v: &str) -> Version {
        Version::parse(v, Scheme::Semver).unwrap()
    }

    fn gh(purl: &str) -> Ref {
        Ref::new(purl.parse().unwrap())
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

    fn asset(name: &str) -> ReleaseAsset {
        ReleaseAsset {
            name: name.to_string(),
            browser_download_url: format!("https://dl/{name}"),
        }
    }

    /// The v0.6.4 release from the real fixture.
    fn ctxloom_v064() -> Release {
        serde_json::from_str::<Vec<Release>>(RELEASES)
            .unwrap()
            .into_iter()
            .find(|r| r.tag_name == "v0.6.4")
            .unwrap()
    }

    #[test]
    fn on_path_matches_exact_entry() {
        let dir = Path::new("/home/x/.local/bin");
        assert!(on_path(dir, "/usr/bin:/home/x/.local/bin:/bin"));
        assert!(!on_path(dir, "/usr/bin:/bin"));
        assert!(!on_path(dir, ""));
    }

    #[test]
    fn binary_defaults_to_the_purl_name_or_the_binary_qualifier() {
        // Single-binary repo: binary defaults to the entry/repo name.
        assert_eq!(
            GithubReleaseBackend::binary_of(&gh("pkg:github/ctxloom/ctxloom")),
            "ctxloom"
        );
        // A `binary` qualifier gives two tools from one repo distinct identities.
        assert_eq!(
            GithubReleaseBackend::binary_of(&gh("pkg:github/acme/tools?binary=foo")),
            "foo"
        );
        assert_eq!(
            GithubReleaseBackend::binary_of(&gh("pkg:github/acme/tools?binary=bar")),
            "bar"
        );
    }

    /// A purl with `asset` set to `template` (percent-encoded as purl writes it).
    fn with_asset(base: &str, template: &str) -> Ref {
        let p: bosun_core::Purl = base.parse().unwrap();
        let mut q = p.qualifiers().clone();
        q.insert("asset".into(), template.into());
        Ref::new(
            bosun_core::Purl::from_components(p.ty(), p.namespace(), p.name(), None, q, None)
                .unwrap(),
        )
    }

    #[test]
    fn expected_asset_default_and_override() {
        let b = backend(Arc::new(FakeHttpFetcher::new()), PathBuf::from("/b"));
        let plain = gh("pkg:github/ctxloom/ctxloom");
        assert_eq!(
            b.expected_asset(&plain, "ctxloom", &sem("0.6.4"), "v0.6.4")
                .unwrap(),
            "ctxloom_0.6.4_linux_amd64.tar.gz"
        );
        let full = with_asset(
            "pkg:github/ctxloom/ctxloom",
            "{{ binary }}_{{ version }}_{{ os }}_{{ arch }}_full.tar.gz",
        );
        assert_eq!(
            b.expected_asset(&full, "ctxloom", &sem("0.6.4"), "v0.6.4")
                .unwrap(),
            "ctxloom_0.6.4_linux_amd64_full.tar.gz"
        );
    }

    #[test]
    fn expected_asset_renders_the_tag_and_goreleaser_init_arch() {
        let b = backend(Arc::new(FakeHttpFetcher::new()), PathBuf::from("/b"));
        let gum = with_asset(
            "pkg:github/charmbracelet/gum",
            "{{ binary }}_{{ tag }}_{{ os | title }}_{{ arch_goreleaser }}.tar.gz",
        );
        assert_eq!(
            b.expected_asset(&gum, "gum", &sem("2.0.2"), "v2.0.2")
                .unwrap(),
            "gum_v2.0.2_Linux_x86_64.tar.gz"
        );
    }

    #[test]
    fn expected_asset_with_an_unknown_variable_is_an_error() {
        let b = backend(Arc::new(FakeHttpFetcher::new()), PathBuf::from("/b"));
        let bad = with_asset("pkg:github/o/r", "{{ platform }}.tar.gz");
        let err = b
            .expected_asset(&bad, "r", &sem("1.0.0"), "v1.0.0")
            .unwrap_err();
        assert!(matches!(err, PortError::Backend { detail }
            if detail.contains(crate::errmsg::ASSET_UNKNOWN_VARIABLE) && detail.contains("platform")));
    }

    #[test]
    fn checksum_fails_on_a_bad_template_once_the_release_is_found() {
        // Resolve reaches the template through `checksum`, so a bad template is
        // a resolve-time error, not a silent fallback to the default name.
        let tag_url = "https://api.github.com/repos/o/r/releases/tags/v1.0.0";
        let release = r#"{"tag_name":"v1.0.0","assets":[]}"#;
        let fetcher = Arc::new(FakeHttpFetcher::new().with_body(tag_url, release));
        let err = backend(fetcher, PathBuf::from("/b"))
            .checksum(
                &with_asset("pkg:github/o/r", "{{ libc }}.tar.gz"),
                &sem("1.0.0"),
                &"linux/amd64".parse().unwrap(),
            )
            .unwrap_err();
        assert!(matches!(err, PortError::Backend { detail }
            if detail.contains(crate::errmsg::ASSET_NOT_YET)));
    }

    #[test]
    fn checksum_refuses_an_asset_that_is_not_a_binary() {
        // A template naming a .deb fails at resolve, before anything downloads.
        let tag_url = "https://api.github.com/repos/o/r/releases/tags/v1.0.0";
        let release = r#"{"tag_name":"v1.0.0","assets":[
            {"name":"r_1.0.0_amd64.deb","browser_download_url":"https://x/r.deb"}]}"#;
        let fetcher = Arc::new(FakeHttpFetcher::new().with_body(tag_url, release));
        let err = backend(fetcher, PathBuf::from("/b"))
            .checksum(
                &with_asset("pkg:github/o/r", "r_{{ version }}_{{ arch }}.deb"),
                &sem("1.0.0"),
                &"linux/amd64".parse().unwrap(),
            )
            .unwrap_err();
        assert!(matches!(err, PortError::Backend { detail }
            if detail.contains(crate::errmsg::NOT_A_BINARY) && detail.contains("r_1.0.0_amd64.deb")));
    }

    #[test]
    fn install_exact_extracts_by_the_assets_format() {
        let tag_url = "https://api.github.com/repos/o/r/releases/tags/v1.0.0";
        let release = r#"{"tag_name":"v1.0.0","assets":[
            {"name":"r-1.0.0-linux.zip","browser_download_url":"https://x/r.zip"},
            {"name":"r-linux-amd64","browser_download_url":"https://x/r"}]}"#;
        let fetcher = Arc::new(FakeHttpFetcher::new().with_body(tag_url, release));
        let b = backend(fetcher, PathBuf::from("/b"));
        for (template, format) in [
            (
                "{{ binary }}-{{ version }}-{{ os }}.zip",
                ArchiveFormat::Zip,
            ),
            ("{{ binary }}-{{ os }}-{{ arch }}", ArchiveFormat::Raw),
        ] {
            let steps = b
                .install_exact(
                    &with_asset("pkg:github/o/r?binary=r", template),
                    &sem("1.0.0"),
                )
                .unwrap();
            assert!(
                matches!(&steps[1], Step::Extract { format: f, .. } if *f == format),
                "{template}: {steps:?}"
            );
        }
    }

    #[test]
    fn picker_chooses_the_plain_archive_not_full() {
        let b = backend(Arc::new(FakeHttpFetcher::new()), PathBuf::from("/b"));
        let assets = ctxloom_v064().assets;
        // Exact match: the plain archive, never the `_full` variant.
        let got = b
            .pick_archive("ctxloom_0.6.4_linux_amd64.tar.gz", &assets)
            .unwrap();
        assert_eq!(got.name, "ctxloom_0.6.4_linux_amd64.tar.gz");
        // The override template reaches the `_full` asset.
        let full = b
            .pick_archive("ctxloom_0.6.4_linux_amd64_full.tar.gz", &assets)
            .unwrap();
        assert_eq!(full.name, "ctxloom_0.6.4_linux_amd64_full.tar.gz");
    }

    #[test]
    fn picker_errors_on_zero_and_lists_candidates() {
        let b = backend(Arc::new(FakeHttpFetcher::new()), PathBuf::from("/b"));
        let assets = vec![asset("ctxloom_0.6.4_windows_amd64.zip")];
        let err = b
            .pick_archive("ctxloom_0.6.4_linux_amd64.tar.gz", &assets)
            .unwrap_err();
        assert!(matches!(err, PortError::Backend { detail }
            if detail.contains("0 asset") && detail.contains("windows_amd64")));
    }

    #[test]
    fn checksum_lookup_from_real_checksums() {
        assert_eq!(
            checksum::sha256_in(CHECKSUMS, "ctxloom_0.6.4_linux_amd64.tar.gz", false),
            Some("89a356c697c9458929ecf851bc85da3a9548d19aa978837b54c0b0b27100810f".to_string())
        );
        assert_eq!(checksum::sha256_in(CHECKSUMS, "absent.tar.gz", false), None);
    }

    #[test]
    fn install_exact_uses_real_release_data() {
        let release = ctxloom_v064();
        let sums_url = release
            .assets
            .iter()
            .find(|a| a.name == "checksums.txt")
            .unwrap()
            .browser_download_url
            .clone();
        let archive_url = release
            .assets
            .iter()
            .find(|a| a.name == "ctxloom_0.6.4_linux_amd64.tar.gz")
            .unwrap()
            .browser_download_url
            .clone();
        let fetcher = Arc::new(
            FakeHttpFetcher::new()
                .with_body(
                    "https://api.github.com/repos/ctxloom/ctxloom/releases/tags/v0.6.4",
                    serde_json::to_string(&json_of(&release)).unwrap(),
                )
                .with_body(&sums_url, CHECKSUMS),
        );
        let b = backend(fetcher, PathBuf::from("/home/x/.local/bin"));
        let steps = b
            .install_exact(&gh("pkg:github/ctxloom/ctxloom"), &sem("0.6.4"))
            .unwrap();
        assert_eq!(
            steps,
            vec![
                Step::Fetch {
                    url: archive_url,
                    sha256: Some(
                        "89a356c697c9458929ecf851bc85da3a9548d19aa978837b54c0b0b27100810f".into()
                    ),
                    dest: PathBuf::from(
                        "/home/x/.local/bin/.bosun-cache/ctxloom_0.6.4_linux_amd64.tar.gz"
                    ),
                },
                Step::Extract {
                    format: ArchiveFormat::TarGz,
                    archive: PathBuf::from(
                        "/home/x/.local/bin/.bosun-cache/ctxloom_0.6.4_linux_amd64.tar.gz"
                    ),
                    member: "ctxloom".into(),
                    dest: PathBuf::from("/home/x/.local/bin/ctxloom"),
                    mode: 0o755,
                },
            ]
        );
    }

    #[test]
    fn checksum_resolves_the_enforced_sha_from_the_release() {
        // §12/slice 12-1: the query side surfaces the archive's checksum so
        // `lock` records it; it is the same sha the Fetch step enforces.
        let release = ctxloom_v064();
        let sums_url = release
            .assets
            .iter()
            .find(|a| a.name == "checksums.txt")
            .unwrap()
            .browser_download_url
            .clone();
        let fetcher = Arc::new(
            FakeHttpFetcher::new()
                .with_body(
                    "https://api.github.com/repos/ctxloom/ctxloom/releases/tags/v0.6.4",
                    serde_json::to_string(&json_of(&release)).unwrap(),
                )
                .with_body(&sums_url, CHECKSUMS),
        );
        let b = backend(fetcher, PathBuf::from("/b"));
        let got = b
            .checksum(
                &gh("pkg:github/ctxloom/ctxloom"),
                &sem("0.6.4"),
                &"linux/amd64".parse().unwrap(),
            )
            .unwrap();
        assert_eq!(
            got,
            Some(Checksum(
                "89a356c697c9458929ecf851bc85da3a9548d19aa978837b54c0b0b27100810f".to_string()
            ))
        );
    }

    /// Re-serialize a parsed Release as the tag-endpoint JSON body.
    fn json_of(release: &Release) -> serde_json::Value {
        serde_json::json!({
            "tag_name": release.tag_name,
            "assets": release.assets.iter().map(|a| serde_json::json!({
                "name": a.name,
                "browser_download_url": a.browser_download_url,
            })).collect::<Vec<_>>(),
        })
    }

    const SHA: &str = "89a356c697c9458929ecf851bc85da3a9548d19aa978837b54c0b0b27100810f";
    const TAG_URL: &str = "https://api.github.com/repos/o/r/releases/tags/v0.7.0";
    const ARCHIVE: &str = "r_0.7.0_linux_amd64.tar.gz";

    /// A v0.7.0 release of o/r carrying the archive plus `extra` assets, each
    /// served with the given body.
    fn release_with(extra: &[(&str, String)]) -> GithubReleaseBackend {
        let mut assets = vec![serde_json::json!({
            "name": ARCHIVE, "browser_download_url": format!("https://dl/{ARCHIVE}"),
        })];
        let mut fetcher = FakeHttpFetcher::new();
        for (name, body) in extra {
            assets.push(serde_json::json!({
                "name": name, "browser_download_url": format!("https://dl/{name}"),
            }));
            fetcher = fetcher.with_body(format!("https://dl/{name}"), body.clone());
        }
        let release = serde_json::json!({"tag_name": "v0.7.0", "assets": assets});
        backend(
            Arc::new(fetcher.with_body(TAG_URL, release.to_string())),
            PathBuf::from("/b"),
        )
    }

    fn fetch_sha(b: &GithubReleaseBackend) -> Option<String> {
        match &b
            .install_exact(&gh("pkg:github/o/r"), &sem("0.7.0"))
            .unwrap()[0]
        {
            Step::Fetch { sha256, .. } => sha256.clone(),
            other => panic!("expected Fetch, got {other:?}"),
        }
    }

    #[test]
    fn no_published_checksum_installs_unverified() {
        let b = release_with(&[]);
        assert_eq!(fetch_sha(&b), None);
        assert_eq!(
            b.checksum(
                &gh("pkg:github/o/r"),
                &sem("0.7.0"),
                &"linux/amd64".parse().unwrap()
            )
            .unwrap(),
            None,
            "nothing is recorded in the lock"
        );
    }

    #[test]
    fn a_sums_file_not_listing_the_asset_is_no_checksum() {
        let b = release_with(&[(
            "checksums.txt",
            format!("{SHA}  r_0.7.0_linux_arm64.tar.gz\n"),
        )]);
        assert_eq!(fetch_sha(&b), None);
    }

    #[test]
    fn a_combined_sums_file_is_used() {
        let b = release_with(&[("r_0.7.0_checksums.txt", format!("{SHA}  {ARCHIVE}\n"))]);
        assert_eq!(fetch_sha(&b), Some(SHA.to_string()));
        assert_eq!(
            b.checksum(
                &gh("pkg:github/o/r"),
                &sem("0.7.0"),
                &"linux/amd64".parse().unwrap()
            )
            .unwrap(),
            Some(Checksum(SHA.to_string()))
        );
    }

    #[test]
    fn a_per_asset_file_wins_over_a_combined_one() {
        let other = "1".repeat(64);
        let b = release_with(&[
            ("SHA256SUMS", format!("{other}  {ARCHIVE}\n")),
            (&format!("{ARCHIVE}.sha256"), format!("{SHA}\n")),
        ]);
        assert_eq!(fetch_sha(&b), Some(SHA.to_string()));
    }

    #[test]
    fn an_unreadable_per_asset_file_falls_through_to_the_combined_one() {
        let b = release_with(&[
            (&format!("{ARCHIVE}.sha256"), "not a hash\n".to_string()),
            ("sha256sum.txt", format!("{SHA}  {ARCHIVE}\n")),
        ]);
        assert_eq!(fetch_sha(&b), Some(SHA.to_string()));
    }

    #[test]
    fn query_available_from_real_releases_has_no_prereleases() {
        let fetcher = Arc::new(FakeHttpFetcher::new().with_body(
            "https://api.github.com/repos/ctxloom/ctxloom/releases?per_page=100&page=1",
            RELEASES,
        ));
        let got = backend(fetcher, PathBuf::from("/b"))
            .query_available(
                &gh("pkg:github/ctxloom/ctxloom"),
                &"linux/amd64".parse().unwrap(),
            )
            .unwrap();
        assert!(got.contains(&sem("0.6.4")));
        assert!(got.contains(&sem("0.6.2")));
        assert!(got.contains(&sem("0.5.4")));
        // ctxloom publishes no pre-releases.
        assert!(
            got.iter().all(|v| !v.is_prerelease()),
            "unexpected pre-release: {got:?}"
        );
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
            .query_available(&gh("pkg:github/o/r"), &"linux/amd64".parse().unwrap())
            .unwrap_err();
        assert!(matches!(err, PortError::Backend { detail } if detail.contains("GITHUB_TOKEN")));
    }

    #[test]
    fn verify_installed_is_presence_only() {
        let dir = tempfile::tempdir().unwrap();
        let b = backend(Arc::new(FakeHttpFetcher::new()), dir.path().to_path_buf());
        let pkg = gh("pkg:github/ctxloom/ctxloom");
        assert_eq!(b.verify_installed(&pkg).unwrap(), Installed::Absent);
        std::fs::write(dir.path().join("ctxloom"), b"bin").unwrap();
        // Present with no version — bosun state is the version authority.
        assert_eq!(
            b.verify_installed(&pkg).unwrap(),
            Installed::Present { version: None }
        );
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
            .query_available(&gh("pkg:github/o/r"), &"linux/amd64".parse().unwrap())
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
            b.uninstall(&gh("pkg:github/ctxloom/ctxloom")).unwrap(),
            vec![Step::Run {
                argv: vec![
                    "rm".into(),
                    "-f".into(),
                    "/home/x/.local/bin/ctxloom".into()
                ],
                env: vec![],
                scope: Scope::User,
            }]
        );
    }

    #[test]
    fn downgrade_is_install_exact() {
        let release = ctxloom_v064();
        let sums_url = release
            .assets
            .iter()
            .find(|a| a.name == "checksums.txt")
            .unwrap()
            .browser_download_url
            .clone();
        let fetcher = Arc::new(
            FakeHttpFetcher::new()
                .with_body(
                    "https://api.github.com/repos/ctxloom/ctxloom/releases/tags/v0.6.4",
                    serde_json::to_string(&json_of(&release)).unwrap(),
                )
                .with_body(&sums_url, CHECKSUMS),
        );
        let b = backend(fetcher, PathBuf::from("/b"));
        let pkg = gh("pkg:github/ctxloom/ctxloom");
        assert_eq!(
            b.downgrade(&pkg, &sem("0.6.4")).unwrap(),
            b.install_exact(&pkg, &sem("0.6.4")).unwrap()
        );
        assert_eq!(b.downgrade(&pkg, &sem("0.6.4")).unwrap().len(), 2);
    }
}
