//! The github-release backend (§12): bosun *is* the installer — it fetches a
//! release archive, verifies its checksum, and extracts the binary. Mutations
//! are `Fetch` + `Extract` steps a host executor (or the container renderer)
//! runs; the query side lists release tags over HTTP.
//!
//! An asset is picked by **exact name**: the `asset` qualifier (the manifest's
//! `backends.github-release.asset`) rendered as a minijinja template for this host, else
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
    Arch, BackendId, Capabilities, Checksum, Installed, LibcFamily, Os, Ref, Scheme, Scope, Source,
    Target, Version,
};
use bosun_ports::{
    ArchiveFormat, Backend, BackendFactory, HostInfo, HttpFetcher, HttpRequest, HttpResponse,
    Index, Installer, PortError, Probe, Step, Steps, UserInstalls,
};
use sha2::{Digest, Sha256};
use tracing::warn;

use crate::{errmsg, logmsg};

mod checksum;
mod tag;
mod template;

const BACKEND: &str = "github-release";

/// Builds the github-release backend. Always present (bosun does the fetching).
pub struct GithubReleaseFactory {
    fetcher: Arc<dyn HttpFetcher>,
    bin_dir: PathBuf,
    os: Os,
    arch: Arch,
    libc: Option<LibcFamily>,
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
            libc: None,
        }
    }

    /// The host's C library family, for the `libc` and `rust_target` asset
    /// template variables (§5.4); without it they are an error.
    pub fn with_libc(mut self, libc: Option<LibcFamily>) -> GithubReleaseFactory {
        self.libc = libc;
        self
    }
}

impl BackendFactory for GithubReleaseFactory {
    fn meta(&self) -> bosun_ports::BackendMeta {
        bosun_ports::BackendMeta {
            // github tags compare as semver though the purl type is `github` (§6).
            purl_type: "github",
            vers_type: "semver",
            platform_independent: false,
            option_keys: &["asset", "tag"],
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
        let path_var = std::env::var("PATH").unwrap_or_default();
        if needs_path_warning(&self.bin_dir, &path_var, self.os) {
            warn!(message = logmsg::BIN_DIR_NOT_ON_PATH, bin_dir = %self.bin_dir.display());
        }
        let backend = self.backend();
        Backend {
            index: Box::new(backend.clone()),
            installer: Box::new(backend),
        }
    }

    fn remote_index(&self) -> Option<Box<dyn Index>> {
        // The release list and checksums come from GitHub's API wherever it runs.
        Some(Box::new(self.backend()))
    }
}

impl GithubReleaseFactory {
    fn backend(&self) -> GithubReleaseBackend {
        GithubReleaseBackend {
            fetcher: self.fetcher.clone(),
            bin_dir: self.bin_dir.clone(),
            os: self.os,
            arch: self.arch,
            libc: self.libc,
            token: std::env::var("GITHUB_TOKEN").ok(),
        }
    }
}

/// Whether `bin_dir` is one of `path_var`'s entries, as `os` reads `PATH`:
/// `;`-separated and case-insensitive on Windows (a trailing `\` ignored),
/// `:`-separated and exact elsewhere.
pub(crate) fn on_path(bin_dir: &Path, path_var: &str, os: Os) -> bool {
    if os == Os::Windows {
        let norm = |p: &str| p.trim_end_matches(['\\', '/']).to_lowercase();
        let want = norm(&bin_dir.to_string_lossy());
        return path_var
            .split(';')
            .any(|p| !p.is_empty() && norm(p) == want);
    }
    path_var
        .split(':')
        .any(|p| !p.is_empty() && Path::new(p) == bin_dir)
}

/// Whether to warn that `bin_dir` is not on `PATH`. Split out from `build` so
/// the condition is tested rather than only logged.
fn needs_path_warning(bin_dir: &Path, path_var: &str, os: Os) -> bool {
    !on_path(bin_dir, path_var, os)
}

#[derive(Clone)]
struct GithubReleaseBackend {
    fetcher: Arc<dyn HttpFetcher>,
    bin_dir: PathBuf,
    os: Os,
    arch: Arch,
    libc: Option<LibcFamily>,
    token: Option<String>,
}

#[derive(Debug, serde::Deserialize)]
struct Release {
    tag_name: String,
    #[serde(default)]
    assets: Vec<ReleaseAsset>,
}

/// One asset of a release, chosen for a package: where to download it, its
/// name, and its published sha256 when there is one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ReleaseFile {
    pub(crate) url: String,
    pub(crate) name: String,
    pub(crate) sha256: Option<String>,
}

/// github-release's release lookup (tags, versions, asset templates,
/// checksums) for the backends that install an OS package attached to a
/// release (github-deb, github-rpm), so the rules are one implementation.
#[derive(Clone)]
pub(crate) struct ReleaseLookup(GithubReleaseBackend);

impl ReleaseLookup {
    /// Look releases up through `fetcher` for an `os`/`arch` host, with
    /// `GITHUB_TOKEN` when set.
    pub(crate) fn new(fetcher: Arc<dyn HttpFetcher>, os: Os, arch: Arch) -> ReleaseLookup {
        ReleaseLookup(GithubReleaseBackend {
            fetcher,
            bin_dir: PathBuf::new(),
            os,
            arch,
            libc: None,
            token: std::env::var("GITHUB_TOKEN").ok(),
        })
    }

    /// The host's C library family, for the `libc` and `rust_target` asset
    /// template variables (§5.4).
    pub(crate) fn with_libc(mut self, libc: Option<LibcFamily>) -> ReleaseLookup {
        self.0.libc = libc;
        self
    }

    /// The versions `pkg`'s releases carry, by its tag pattern.
    pub(crate) fn versions(&self, pkg: &Ref, target: &Target) -> Result<Vec<Version>, PortError> {
        self.0.query_available(pkg, target)
    }

    /// The asset the `asset` template (required: there is no default) names
    /// at `version`, refused unless `accept` takes its name.
    pub(crate) fn file(
        &self,
        pkg: &Ref,
        version: &Version,
        accept: impl Fn(&str) -> bool,
        refusal: &str,
    ) -> Result<ReleaseFile, PortError> {
        self.0.release_file(pkg, version, None, accept, refusal)
    }

    /// The sha256 the lock records for `file`: the published one, else the
    /// download's as it is now (trust on first use).
    pub(crate) fn recorded_sha256(
        &self,
        pkg: &Ref,
        file: &ReleaseFile,
    ) -> Result<String, PortError> {
        self.0.recorded_sha256(pkg, file)
    }

    /// For a backend that installs `file` as root or Administrator
    /// (`backend`): a release without a published checksum is refused unless
    /// the entry consents with `unverified: allow`. Only the first download
    /// would be unverified; the lock pins its sha256 from then on.
    pub(crate) fn require_verifiable(
        pkg: &Ref,
        file: &ReleaseFile,
        backend: &str,
    ) -> Result<(), PortError> {
        let consent = pkg.purl.qualifiers().get("unverified").map(String::as_str);
        match (&file.sha256, consent) {
            (_, Some(other)) if other != "allow" => Err(PortError::Backend {
                detail: format!(
                    "{}: {} {other:?}",
                    pkg.purl.native_id(),
                    errmsg::UNVERIFIED_VALUE
                ),
            }),
            (None, None) => Err(PortError::Backend {
                detail: format!(
                    "{}: {}: {}; to install it anyway, set backends.{backend}.unverified: allow",
                    pkg.purl.native_id(),
                    file.name,
                    errmsg::UNVERIFIED_ELEVATED
                ),
            }),
            _ => Ok(()),
        }
    }

    /// The binary name, also the default OS package name: the `binary`
    /// qualifier, else the repository's name.
    pub(crate) fn binary_of(pkg: &Ref) -> String {
        GithubReleaseBackend::binary_of(pkg)
    }
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

/// The package's tag pattern, from its `tag` qualifier; `None` without one.
fn tag_pattern(pkg: &Ref) -> Result<Option<tag::TagPattern>, PortError> {
    pkg.purl
        .qualifiers()
        .get("tag")
        .map(|t| tag::TagPattern::parse(t).map_err(|detail| PortError::Backend { detail }))
        .transpose()
}

impl GithubReleaseBackend {
    /// The binary's file name on this host: the binary name, with `.exe` on
    /// Windows. The `binary` template variable stays the bare name, as release
    /// assets spell it.
    fn installed_name(&self, pkg: &Ref) -> String {
        let binary = Self::binary_of(pkg);
        if self.os == Os::Windows && !binary.to_ascii_lowercase().ends_with(".exe") {
            format!("{binary}.exe")
        } else {
            binary
        }
    }

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
    /// (see [`template`]), else `default` (goreleaser's
    /// `{{ binary }}_{{ version }}_{{ os }}_{{ arch }}.tar.gz` for
    /// github-release; none for the OS-package backends, which need one).
    /// `tag` is the release's actual tag. A bad template is an error, never a
    /// fallback.
    fn expected_asset(
        &self,
        pkg: &Ref,
        binary: &str,
        version: &Version,
        tag: &str,
        default: Option<&str>,
    ) -> Result<String, PortError> {
        let source = match (pkg.purl.qualifiers().get("asset"), default) {
            (Some(template), _) => template.as_str(),
            (None, Some(default)) => default,
            (None, None) => {
                return Err(PortError::Backend {
                    detail: format!("{}: {}", pkg.purl.native_id(), errmsg::ASSET_REQUIRED),
                })
            }
        };
        template::render(
            source,
            &template::AssetVars {
                binary,
                version: version.as_str(),
                tag,
                os: self.os,
                arch: self.arch,
                libc: self.libc,
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

    /// The sha256 the lock records for `file`: the release's published one,
    /// else (trust on first use) the sha256 of the file as it downloads now,
    /// so a later install of this version fails if the asset was replaced.
    fn recorded_sha256(&self, pkg: &Ref, file: &ReleaseFile) -> Result<String, PortError> {
        if let Some(sha) = &file.sha256 {
            return Ok(sha.clone());
        }
        let resp = self.fetcher.fetch(&HttpRequest::get(&file.url))?;
        check_status(&file.url, &resp)?;
        let sha: String = Sha256::digest(&resp.body)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        warn!(
            message = logmsg::FIRST_USE_CHECKSUM,
            repo = %pkg.purl.native_id(),
            asset = %file.name,
            sha256 = %sha,
        );
        Ok(sha)
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
        let file = self.release_file(
            pkg,
            version,
            Some(template::DEFAULT_TEMPLATE),
            |name| ArchiveFormat::of(name).is_some(),
            errmsg::NOT_A_BINARY,
        )?;
        let format = ArchiveFormat::of(&file.name).expect("checked by release_file");
        Ok((file.url, file.name, format, file.sha256))
    }

    /// The release asset for `pkg` at `version`: found by its tag (the `tag`
    /// qualifier, else `v<version>`/`<version>`), named by the `asset`
    /// template (else `default`), and refused unless `accept` takes its name
    /// (`refusal` says why), with the release's published sha256 if any.
    fn release_file(
        &self,
        pkg: &Ref,
        version: &Version,
        default: Option<&str>,
        accept: impl Fn(&str) -> bool,
        refusal: &str,
    ) -> Result<ReleaseFile, PortError> {
        let repo = pkg.purl.native_id();
        let binary = Self::binary_of(pkg);
        let release = self.fetch_release(&repo, version, tag_pattern(pkg)?.as_ref())?;
        let expected = self.expected_asset(pkg, &binary, version, &release.tag_name, default)?;
        if !accept(&expected) {
            return Err(PortError::Backend {
                detail: format!("{repo}: {expected:?} is {refusal}"),
            });
        }
        let archive = self.pick_archive(&expected, &release.assets)?;
        let sha256 = self.published_sha256(&release.assets, &archive.name)?;
        Ok(ReleaseFile {
            url: archive.browser_download_url.clone(),
            name: archive.name.clone(),
            sha256,
        })
    }

    /// The release for `version`: tagged as `pattern` says, else `v<version>`
    /// or `<version>`.
    fn fetch_release(
        &self,
        repo: &str,
        version: &Version,
        pattern: Option<&tag::TagPattern>,
    ) -> Result<Release, PortError> {
        let tags = match pattern {
            Some(p) => vec![p.tag_for(version.as_str())],
            None => vec![
                format!("v{}", version.as_str()),
                version.as_str().to_string(),
            ],
        };
        for tag in &tags {
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
            detail: format!("no release tagged {} in {repo}", tags.join(" or ")),
        })
    }
}

impl Index for GithubReleaseBackend {
    fn refresh(&self) -> Result<(), PortError> {
        Ok(()) // releases are fetched fresh per query
    }

    fn query_available(&self, pkg: &Ref, _target: &Target) -> Result<Vec<Version>, PortError> {
        let repo = pkg.purl.native_id();
        let pattern = tag_pattern(pkg)?;
        let version_of = |tag: &str| match &pattern {
            Some(p) => p
                .version_in(tag)
                .and_then(|v| Version::parse(v, Scheme::Semver).ok()),
            None => version_from_tag(tag),
        };
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
            versions.extend(releases.iter().filter_map(|r| version_of(&r.tag_name)));
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
        let (url, name, _, sha256) = self.archive_and_checksum(pkg, version)?;
        let file = ReleaseFile { url, name, sha256 };
        Ok(Some(Checksum(self.recorded_sha256(pkg, &file)?)))
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
        let binary = self.installed_name(pkg);
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
        let path = self.bin_dir.join(self.installed_name(pkg));
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
        Ok(vec![Step::Remove {
            path: self.bin_dir.join(self.installed_name(pkg)),
        }])
    }

    fn user_installs(&self) -> Result<UserInstalls, PortError> {
        // bosun is the installer: no other record of these exists.
        Ok(UserInstalls::BosunState)
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
            libc: None,
            token: None,
        }
    }

    #[test]
    fn what_the_user_installed_is_what_bosun_recorded() {
        let b = backend(Arc::new(FakeHttpFetcher::new()), PathBuf::from("/bin"));
        assert_eq!(
            b.user_installs().unwrap(),
            bosun_ports::UserInstalls::BosunState
        );
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
        assert!(on_path(dir, "/usr/bin:/home/x/.local/bin:/bin", Os::Linux));
        assert!(!on_path(dir, "/usr/bin:/bin", Os::Linux));
        assert!(!on_path(dir, "", Os::Linux));
        // Linux paths are case-sensitive.
        assert!(!on_path(dir, "/HOME/x/.local/bin", Os::Linux));
    }

    #[test]
    fn on_windows_path_entries_split_at_semicolons_and_ignore_case() {
        let dir = Path::new(r"C:\Users\x\.local\bin");
        assert!(on_path(
            dir,
            r"C:\Windows;c:\users\X\.LOCAL\bin",
            Os::Windows
        ));
        assert!(on_path(dir, r"C:\Users\x\.local\bin\", Os::Windows));
        assert!(!on_path(dir, r"C:\Windows;C:\Users\x\bin", Os::Windows));
        assert!(!on_path(dir, "", Os::Windows));
    }

    fn windows(fetcher: Arc<dyn HttpFetcher>) -> GithubReleaseBackend {
        GithubReleaseBackend {
            os: Os::Windows,
            ..backend(fetcher, PathBuf::from("/w/bin"))
        }
    }

    #[test]
    fn on_windows_the_binary_is_found_and_installed_as_an_exe() {
        let tag_url = "https://api.github.com/repos/o/tool/releases/tags/v1.0.0";
        let release = r#"{"tag_name":"v1.0.0","assets":[
            {"name":"tool_1.0.0_windows_amd64.zip","browser_download_url":"https://x/tool.zip"},
            {"name":"tool-windows-amd64.exe","browser_download_url":"https://x/tool.exe"}]}"#;
        let fetcher: Arc<dyn HttpFetcher> =
            Arc::new(FakeHttpFetcher::new().with_body(tag_url, release));
        let b = windows(fetcher);
        let zip = b
            .install_exact(
                &with_asset(
                    "pkg:github/o/tool",
                    "{{ binary }}_{{ version }}_{{ os }}_{{ arch }}.zip",
                ),
                &sem("1.0.0"),
            )
            .unwrap();
        assert!(
            matches!(&zip[1], Step::Extract { member, dest, format: ArchiveFormat::Zip, .. }
                if member == "tool.exe" && dest == &PathBuf::from("/w/bin/tool.exe")),
            "{zip:?}"
        );
        let raw = b
            .install_exact(
                &with_asset("pkg:github/o/tool", "{{ binary }}-{{ os }}-{{ arch }}.exe"),
                &sem("1.0.0"),
            )
            .unwrap();
        assert!(
            matches!(&raw[1], Step::Extract { dest, format: ArchiveFormat::Raw, .. }
                if dest == &PathBuf::from("/w/bin/tool.exe")),
            "{raw:?}"
        );
        assert_eq!(
            b.uninstall(&gh("pkg:github/o/tool")).unwrap(),
            vec![Step::Remove {
                path: PathBuf::from("/w/bin/tool.exe")
            }]
        );
    }

    #[test]
    fn on_windows_presence_is_the_exe() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("tool.exe"), b"x").unwrap();
        let b = GithubReleaseBackend {
            bin_dir: dir.path().to_path_buf(),
            ..windows(Arc::new(FakeHttpFetcher::new()))
        };
        assert_eq!(
            b.verify_installed(&gh("pkg:github/o/tool")).unwrap(),
            Installed::Present { version: None }
        );
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
            b.expected_asset(
                &plain,
                "ctxloom",
                &sem("0.6.4"),
                "v0.6.4",
                Some(template::DEFAULT_TEMPLATE)
            )
            .unwrap(),
            "ctxloom_0.6.4_linux_amd64.tar.gz"
        );
        let full = with_asset(
            "pkg:github/ctxloom/ctxloom",
            "{{ binary }}_{{ version }}_{{ os }}_{{ arch }}_full.tar.gz",
        );
        assert_eq!(
            b.expected_asset(
                &full,
                "ctxloom",
                &sem("0.6.4"),
                "v0.6.4",
                Some(template::DEFAULT_TEMPLATE)
            )
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
            b.expected_asset(
                &gum,
                "gum",
                &sem("2.0.2"),
                "v2.0.2",
                Some(template::DEFAULT_TEMPLATE)
            )
            .unwrap(),
            "gum_v2.0.2_Linux_x86_64.tar.gz"
        );
    }

    #[test]
    fn expected_asset_with_an_unknown_variable_is_an_error() {
        let b = backend(Arc::new(FakeHttpFetcher::new()), PathBuf::from("/b"));
        let bad = with_asset("pkg:github/o/r", "{{ platform }}.tar.gz");
        let err = b
            .expected_asset(
                &bad,
                "r",
                &sem("1.0.0"),
                "v1.0.0",
                Some(template::DEFAULT_TEMPLATE),
            )
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
            if detail.contains(crate::errmsg::ASSET_LIBC_UNKNOWN)));
    }

    fn with_qualifiers(base: &str, pairs: &[(&str, &str)]) -> Ref {
        let p: bosun_core::Purl = base.parse().unwrap();
        let mut q = p.qualifiers().clone();
        for (k, v) in pairs {
            q.insert(k.to_string(), v.to_string());
        }
        Ref::new(
            bosun_core::Purl::from_components(p.ty(), p.namespace(), p.name(), None, q, None)
                .unwrap(),
        )
    }

    const KUSTOMIZE_RELEASES: &str = r#"[
        {"tag_name":"kyaml/v0.21.3","assets":[]},
        {"tag_name":"kustomize/v5.8.3","assets":[]},
        {"tag_name":"api/v0.21.3","assets":[]},
        {"tag_name":"kustomize/v5.8.0","assets":[]},
        {"tag_name":"kustomize/vnext","assets":[]}]"#;

    #[test]
    fn a_tag_template_lists_only_the_tags_that_fit_it() {
        let list =
            "https://api.github.com/repos/kubernetes-sigs/kustomize/releases?per_page=100&page=1";
        let fetcher = Arc::new(FakeHttpFetcher::new().with_body(list, KUSTOMIZE_RELEASES));
        let pkg = with_qualifiers(
            "pkg:github/kubernetes-sigs/kustomize",
            &[("tag", "kustomize/v{{ version }}")],
        );
        let mut versions: Vec<String> = backend(fetcher, PathBuf::from("/b"))
            .query_available(&pkg, &"linux/amd64".parse().unwrap())
            .unwrap()
            .iter()
            .map(|v| v.as_str().to_string())
            .collect();
        versions.sort();
        assert_eq!(versions, vec!["5.8.0", "5.8.3"]);
    }

    #[test]
    fn without_a_tag_template_a_prefixed_tag_is_not_a_version() {
        let list = "https://api.github.com/repos/jqlang/jq/releases?per_page=100&page=1";
        let fetcher = Arc::new(
            FakeHttpFetcher::new().with_body(list, r#"[{"tag_name":"jq-1.8.2","assets":[]}]"#),
        );
        let versions = backend(fetcher, PathBuf::from("/b"))
            .query_available(&gh("pkg:github/jqlang/jq"), &"linux/amd64".parse().unwrap())
            .unwrap();
        assert!(versions.is_empty());
    }

    #[test]
    fn install_finds_the_release_by_its_tag_template() {
        let tag_url = "https://api.github.com/repos/jqlang/jq/releases/tags/jq-1.8.2";
        let release = r#"{"tag_name":"jq-1.8.2","assets":[
            {"name":"jq-linux-amd64","browser_download_url":"https://x/jq-linux-amd64"}]}"#;
        let fetcher = Arc::new(FakeHttpFetcher::new().with_body(tag_url, release));
        let pkg = with_qualifiers(
            "pkg:github/jqlang/jq",
            &[
                ("tag", "jq-{{ version }}"),
                ("asset", "jq-{{ os }}-{{ arch }}"),
            ],
        );
        let steps = backend(fetcher, PathBuf::from("/b"))
            .install_exact(&pkg, &sem("1.8.2"))
            .unwrap();
        assert!(
            matches!(&steps[0], Step::Fetch { url, .. } if url == "https://x/jq-linux-amd64"),
            "{steps:?}"
        );
    }

    #[test]
    fn a_bad_tag_template_is_an_error_naming_it() {
        let pkg = with_qualifiers("pkg:github/jqlang/jq", &[("tag", "jq-1.8.2")]);
        let err = backend(Arc::new(FakeHttpFetcher::new()), PathBuf::from("/b"))
            .query_available(&pkg, &"linux/amd64".parse().unwrap())
            .unwrap_err();
        assert!(matches!(err, PortError::Backend { detail }
            if detail.contains(crate::errmsg::TAG_TEMPLATE) && detail.contains("jq-1.8.2")));
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

    /// o/tool v1.0.0 with the default-named archive and no sums file; the
    /// archive's bytes are `body`.
    fn unsummed_release(body: &[u8]) -> Arc<FakeHttpFetcher> {
        let name = "tool_1.0.0_linux_amd64.tar.gz";
        let release = serde_json::json!({
            "tag_name": "v1.0.0",
            "assets": [{"name": name, "browser_download_url": format!("https://dl/{name}")}],
        });
        Arc::new(
            FakeHttpFetcher::new()
                .with_body(
                    "https://api.github.com/repos/o/tool/releases/tags/v1.0.0",
                    release.to_string(),
                )
                .with_body(format!("https://dl/{name}"), body.to_vec()),
        )
    }

    #[test]
    fn checksum_of_a_release_without_sums_is_what_was_downloaded() {
        // Trust on first use: the lock records the sha256 of the archive as
        // it is now, so a later install of this version must match it.
        let b = backend(unsummed_release(b"the archive"), PathBuf::from("/b"));
        let got = b
            .checksum(
                &gh("pkg:github/o/tool?binary=tool"),
                &sem("1.0.0"),
                &"linux/amd64".parse().unwrap(),
            )
            .unwrap();
        let want: String = Sha256::digest(b"the archive")
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        assert_eq!(got, Some(Checksum(want)));
    }

    #[test]
    fn install_of_a_release_without_sums_leaves_the_fetch_to_the_lock() {
        // No published sha: the Fetch carries none, and the planner fills in the
        // one the lock recorded.
        let b = backend(unsummed_release(b"the archive"), PathBuf::from("/b"));
        let steps = b
            .install_exact(&gh("pkg:github/o/tool?binary=tool"), &sem("1.0.0"))
            .unwrap();
        assert!(
            matches!(&steps[0], Step::Fetch { sha256: None, .. }),
            "{steps:?}"
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
    fn no_published_checksum_fetches_with_none_of_its_own() {
        // What the lock recorded on first use is filled in by the planner
        // (see checksum_of_a_release_without_sums_is_what_was_downloaded).
        let b = release_with(&[]);
        assert_eq!(fetch_sha(&b), None);
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
        assert!(needs_path_warning(dir, "/usr/bin:/bin", Os::Linux));
        assert!(!needs_path_warning(
            dir,
            "/usr/bin:/home/x/.local/bin",
            Os::Linux
        ));
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
    fn the_factorys_libc_reaches_the_asset_template() {
        let tag_url = "https://api.github.com/repos/o/r/releases/tags/v1.0.0";
        let release = r#"{"tag_name":"v1.0.0","assets":[]}"#;
        let detail = |libc: Option<LibcFamily>| {
            let factory = GithubReleaseFactory::new(
                Arc::new(FakeHttpFetcher::new().with_body(tag_url, release)),
                PathBuf::from("/b"),
                Os::Linux,
                Arch::Amd64,
            )
            .with_libc(libc);
            let runner = bosun_ports::testing::FakeCommandRunner::new();
            let host = HostInfo {
                os: Os::Linux,
                arch: Arch::Amd64,
                distro: None,
                runner: &runner,
            };
            let backend = factory.build(factory.probe(&host).unwrap().unwrap());
            match backend.index.checksum(
                &with_asset("pkg:github/o/r", "{{ rust_target }}.tar.gz"),
                &sem("1.0.0"),
                &"linux/amd64".parse().unwrap(),
            ) {
                Err(PortError::Backend { detail }) => detail,
                other => panic!("want an asset error, got {other:?}"),
            }
        };
        // Rendered, then not found among the release's (no) assets.
        let musl = detail(Some(LibcFamily::Musl));
        assert!(musl.contains("x86_64-unknown-linux-musl.tar.gz"), "{musl}");
        let unknown = detail(None);
        assert!(
            unknown.contains(crate::errmsg::ASSET_LIBC_UNKNOWN),
            "{unknown}"
        );
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
            vec![Step::Remove {
                path: PathBuf::from("/home/x/.local/bin/ctxloom"),
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

    #[test]
    fn a_remote_index_reads_the_release_list() {
        let list = "https://api.github.com/repos/casey/just/releases?per_page=100&page=1";
        let fetcher = Arc::new(
            FakeHttpFetcher::new().with_body(list, r#"[{"tag_name":"1.40.0","assets":[]}]"#),
        );
        let index = GithubReleaseFactory::new(fetcher, PathBuf::from("/b"), Os::Linux, Arch::Arm64)
            .remote_index()
            .expect("a remote index");
        let just = Ref::new("pkg:github/casey/just".parse().unwrap());
        let versions = index
            .query_available(
                &just,
                &Target::Platform {
                    os: Os::Linux,
                    arch: Arch::Arm64,
                },
            )
            .unwrap();
        assert_eq!(
            versions,
            vec![Version::parse("1.40.0", Scheme::Semver).unwrap()]
        );
    }
}
