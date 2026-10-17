//! apt's remote index (§4.7): the versions a distro release's apt would offer,
//! read from its mirror over HTTPS rather than from this machine's apt, so
//! bosun can resolve for a machine it isn't running on (`render --target`,
//! §8.3).
//!
//! A release is the suites a stock image of it lists (`bookworm`,
//! `bookworm-updates` and `bookworm-security` for Debian 12), each fetched the
//! way apt fetches it: the suite's `InRelease`, then each component's
//! `Packages.gz` (else `.xz`), checked against the size and SHA256 the `InRelease` lists
//! (by hash when the release offers `Acquire-By-Hash`). The `InRelease` is
//! trusted as fetched over HTTPS; its OpenPGP signature is not checked here
//! (the image's apt checks it when it installs).
//!
//! Fetched files are cached: an `InRelease` is used for [`RELEASE_TTL`] before
//! it is fetched again (an older one is used, with a warning, when the mirror
//! can't be reached), and a package index for as long as its hash is the one
//! the current `InRelease` lists.

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use bosun_core::{Arch, Ref, Scheme, Target, Version};
use bosun_ports::{HttpFetcher, HttpRequest, Index, PortError};
use sha2::{Digest, Sha256};
use tracing::{debug, warn};

use crate::{errmsg, logmsg};

/// How long a fetched `InRelease` is used before it is fetched again. Debian's
/// mirrors update four times a day and its security suite more often; an hour
/// keeps a render close to what `apt-get update` would see now while sparing
/// the mirror a fetch per run.
pub const RELEASE_TTL: Duration = Duration::from_secs(60 * 60);

const DEBIAN: &str = "https://deb.debian.org/debian";
const DEBIAN_SECURITY: &str = "https://deb.debian.org/debian-security";
const UBUNTU: &str = "https://archive.ubuntu.com/ubuntu";
const UBUNTU_SECURITY: &str = "https://security.ubuntu.com/ubuntu";
const UBUNTU_PORTS: &str = "https://ports.ubuntu.com/ubuntu-ports";

/// One suite on one mirror.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Suite {
    /// The mirror's base URL (`https://deb.debian.org/debian`).
    pub mirror: String,
    /// The suite (`bookworm-updates`).
    pub suite: String,
    /// Whether a missing suite is an error (the release itself) or skipped
    /// (its updates and security suites, which a release may not have yet).
    pub required: bool,
}

/// The apt sources of a distro release: its suites and the components read
/// from each.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Sources {
    /// The suites, the release's own first.
    pub suites: Vec<Suite>,
    /// The components (`main`, …).
    pub components: Vec<&'static str>,
}

fn suite(mirror: &str, suite: String, required: bool) -> Suite {
    Suite {
        mirror: mirror.to_string(),
        suite,
        required,
    }
}

/// The sources a stock container image of `distro` `codename` lists, for
/// `arch`: the release, its `-updates` and its `-security` suites. Debian's
/// `main`; Ubuntu's four components, from the ports mirror on arm64.
/// Backports are left out: apt never picks them unless asked. `None` for a
/// distro bosun knows no mirror for.
pub fn default_sources(distro: &str, codename: &str, arch: Arch) -> Option<Sources> {
    let (main, security, components) = match (distro, arch) {
        ("debian", _) => (DEBIAN, DEBIAN_SECURITY, vec!["main"]),
        ("ubuntu", Arch::Amd64) => (UBUNTU, UBUNTU_SECURITY, ubuntu_components()),
        ("ubuntu", Arch::Arm64) => (UBUNTU_PORTS, UBUNTU_PORTS, ubuntu_components()),
        _ => return None,
    };
    Some(Sources {
        suites: vec![
            suite(main, codename.to_string(), true),
            suite(main, format!("{codename}-updates"), false),
            suite(security, format!("{codename}-security"), false),
        ],
        components,
    })
}

fn ubuntu_components() -> Vec<&'static str> {
    vec!["main", "restricted", "universe", "multiverse"]
}

/// One file a release lists: its SHA256 and size.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileHash {
    /// The lowercase hex SHA256.
    pub sha256: String,
    /// The size in bytes.
    pub size: u64,
}

/// The fields of a `Release` / `InRelease` bosun reads.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Release {
    /// `Suite` (`oldstable`, `noble-updates`).
    pub suite: Option<String>,
    /// `Codename` (`bookworm`, `bookworm-security`).
    pub codename: Option<String>,
    /// `Acquire-By-Hash: yes`: files can be fetched by their hash.
    pub by_hash: bool,
    /// The `SHA256` section, by path (`main/binary-amd64/Packages.xz`).
    pub sha256: BTreeMap<String, FileHash>,
}

impl Release {
    /// Whether this is the release file of `suite`: apt's suites are named by
    /// codename (`bookworm`, `bookworm-security`) or by suite (`noble-updates`).
    pub fn is_for(&self, suite: &str) -> bool {
        self.suite.as_deref() == Some(suite) || self.codename.as_deref() == Some(suite)
    }
}

const SIGNED: &str = "-----BEGIN PGP SIGNED MESSAGE-----";
const SIGNATURE: &str = "-----BEGIN PGP SIGNATURE-----";

/// The signed text of a clearsigned `InRelease`: after the armor headers'
/// blank line, up to the signature, dash-escaping (`- `) undone. Text that is
/// not clearsigned (a plain `Release`) is returned whole.
pub fn signed_text(raw: &str) -> String {
    let Some(rest) = raw.strip_prefix(SIGNED) else {
        return raw.to_string();
    };
    let body = rest.split_once("\n\n").map_or("", |(_, body)| body);
    let body = body.split_once(SIGNATURE).map_or(body, |(text, _)| text);
    body.lines()
        .map(|l| l.strip_prefix("- ").unwrap_or(l))
        .collect::<Vec<_>>()
        .join("\n")
}

fn bad_release(detail: String) -> PortError {
    PortError::Backend {
        detail: format!("{}: {detail}", errmsg::APT_BAD_RELEASE),
    }
}

/// Parse the signed text of a release file.
pub fn parse_release(text: &str) -> Result<Release, PortError> {
    let mut release = Release::default();
    let mut field = String::new();
    for line in text.lines() {
        if let Some(cont) = line.strip_prefix(' ') {
            if field == "SHA256" {
                let parts: Vec<&str> = cont.split_whitespace().collect();
                let [hash, size, path] = parts.as_slice() else {
                    return Err(bad_release(format!("SHA256 line {line:?}")));
                };
                let size = size
                    .parse()
                    .map_err(|_| bad_release(format!("SHA256 size in {line:?}")))?;
                release.sha256.insert(
                    path.to_string(),
                    FileHash {
                        sha256: hash.to_ascii_lowercase(),
                        size,
                    },
                );
            }
            continue;
        }
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        field = key.to_string();
        let value = value.trim();
        match key {
            "Suite" => release.suite = Some(value.to_string()),
            "Codename" => release.codename = Some(value.to_string()),
            "Acquire-By-Hash" => release.by_hash = value == "yes",
            _ => {}
        }
    }
    Ok(release)
}

/// Each package's versions in a `Packages` file, added to `into`.
pub fn add_package_versions(packages: &str, into: &mut BTreeMap<String, Vec<String>>) {
    let mut name: Option<&str> = None;
    let mut version: Option<&str> = None;
    for line in packages.lines().chain(std::iter::once("")) {
        if line.is_empty() {
            if let (Some(n), Some(v)) = (name.take(), version.take()) {
                let versions = into.entry(n.to_string()).or_default();
                if !versions.iter().any(|known| known == v) {
                    versions.push(v.to_string());
                }
            }
            name = None;
            version = None;
        } else if let Some(n) = line.strip_prefix("Package:") {
            name = Some(n.trim());
        } else if let Some(v) = line.strip_prefix("Version:") {
            version = Some(v.trim());
        }
    }
}

fn hex_sha256(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// A mirror URL as one directory name: `https://deb.debian.org/debian` →
/// `deb.debian.org_debian`.
fn cache_name(mirror: &str) -> String {
    let bare = mirror.split_once("://").map_or(mirror, |(_, rest)| rest);
    bare.trim_end_matches('/').replace(['/', ':'], "_")
}

fn cache_err(path: &Path, e: std::io::Error) -> PortError {
    PortError::Backend {
        detail: format!("{}: {}: {e}", errmsg::APT_CACHE, path.display()),
    }
}

/// Write `bytes` to `path` through a temp file and a rename, so a reader never
/// sees half a file.
fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), PortError> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).map_err(|e| cache_err(dir, e))?;
    }
    let tmp = path.with_extension(format!("tmp{}", std::process::id()));
    fs::write(&tmp, bytes).map_err(|e| cache_err(&tmp, e))?;
    fs::rename(&tmp, path).map_err(|e| cache_err(path, e))
}

/// Whether the file at `path` was written less than `ttl` ago.
fn fresh(path: &Path, ttl: Duration) -> bool {
    fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|written| SystemTime::now().duration_since(written).ok())
        .is_some_and(|age| age < ttl)
}

/// Package name → its versions across a release's suites and components.
type Available = BTreeMap<String, Vec<String>>;

/// apt's [`Index`] over a release's mirror (§4.7). The release comes from the
/// identity: the purl's namespace is the distro (`debian`), its `distro`
/// qualifier the codename, its `arch` qualifier the architecture.
pub struct RemoteAptIndex {
    fetcher: Arc<dyn HttpFetcher>,
    cache: PathBuf,
    /// Each release read this run, by distro, codename and arch.
    releases: RefCell<BTreeMap<(String, String, Arch), Rc<Available>>>,
    /// Set by `refresh`: fetch every `InRelease` again, however fresh.
    refetch: Cell<bool>,
}

impl RemoteAptIndex {
    /// An index fetching through `fetcher`, caching under `cache`.
    pub fn new(fetcher: Arc<dyn HttpFetcher>, cache: PathBuf) -> RemoteAptIndex {
        RemoteAptIndex {
            fetcher,
            cache,
            releases: RefCell::new(BTreeMap::new()),
            refetch: Cell::new(false),
        }
    }

    fn suite_dir(&self, s: &Suite) -> PathBuf {
        self.cache.join(cache_name(&s.mirror)).join(&s.suite)
    }

    /// The release file of suite `s`; `None` when the mirror has no such
    /// suite and it isn't required.
    fn release(&self, s: &Suite) -> Result<Option<Release>, PortError> {
        let url = format!("{}/dists/{}/InRelease", s.mirror, s.suite);
        let cached = self.suite_dir(s).join("InRelease");
        let bytes = if !self.refetch.get() && fresh(&cached, RELEASE_TTL) {
            debug!(message = logmsg::APT_RELEASE_CACHED, url = %url);
            fs::read(&cached).map_err(|e| cache_err(&cached, e))?
        } else {
            match self.fetch_release(&url, &cached) {
                Ok(Some(bytes)) => bytes,
                Ok(None) if s.required => {
                    return Err(PortError::Backend {
                        detail: format!("{}: {} at {}", errmsg::APT_NO_RELEASE, s.suite, s.mirror),
                    })
                }
                Ok(None) => {
                    debug!(message = logmsg::APT_SUITE_ABSENT, url = %url);
                    return Ok(None);
                }
                Err(e) if cached.exists() => {
                    warn!(message = logmsg::APT_STALE_RELEASE, url = %url, error = %e);
                    fs::read(&cached).map_err(|e| cache_err(&cached, e))?
                }
                Err(e) => return Err(e),
            }
        };
        let release = parse_release(&signed_text(&String::from_utf8_lossy(&bytes)))?;
        if !release.is_for(&s.suite) {
            return Err(PortError::Backend {
                detail: format!(
                    "{}: {url} is {}",
                    errmsg::APT_RELEASE_MISMATCH,
                    release.codename.or(release.suite).unwrap_or_default()
                ),
            });
        }
        Ok(Some(release))
    }

    /// Fetch the `InRelease` at `url` and cache it; `None` on a 404.
    fn fetch_release(&self, url: &str, cached: &Path) -> Result<Option<Vec<u8>>, PortError> {
        let resp = self.fetcher.fetch(&HttpRequest::get(url))?;
        match resp.status {
            200..=299 => {
                write_atomic(cached, &resp.body)?;
                Ok(Some(resp.body))
            }
            404 => Ok(None),
            status => Err(PortError::HttpStatus {
                url: url.to_string(),
                status,
            }),
        }
    }

    /// The package index at `path` in suite `s`, checked against `hash`: the
    /// cached copy while it still matches, else fetched (by hash when the
    /// release offers it) and cached.
    fn package_index(
        &self,
        s: &Suite,
        release: &Release,
        path: &str,
        hash: &FileHash,
    ) -> Result<Vec<u8>, PortError> {
        let cached = self.suite_dir(s).join(path.replace('/', "_"));
        if let Ok(bytes) = fs::read(&cached) {
            if hex_sha256(&bytes) == hash.sha256 {
                return Ok(bytes);
            }
        }
        let url = if release.by_hash {
            let dir = path.rsplit_once('/').map_or("", |(dir, _)| dir);
            format!(
                "{}/dists/{}/{dir}/by-hash/SHA256/{}",
                s.mirror, s.suite, hash.sha256
            )
        } else {
            format!("{}/dists/{}/{path}", s.mirror, s.suite)
        };
        let resp = self.fetcher.fetch(&HttpRequest::get(&url))?;
        if !(200..300).contains(&resp.status) {
            return Err(PortError::HttpStatus {
                url,
                status: resp.status,
            });
        }
        let got = hex_sha256(&resp.body);
        if resp.body.len() as u64 != hash.size || got != hash.sha256 {
            return Err(PortError::Backend {
                detail: format!(
                    "{}: {url}: want sha256 {} ({} bytes), got {got} ({} bytes)",
                    errmsg::APT_HASH_MISMATCH,
                    hash.sha256,
                    hash.size,
                    resp.body.len()
                ),
            });
        }
        write_atomic(&cached, &resp.body)?;
        Ok(resp.body)
    }

    /// Every package's versions in `sources`, for `arch`.
    fn read_release(&self, sources: &Sources, arch: Arch) -> Result<Available, PortError> {
        let mut available = Available::new();
        let mut any_index = false;
        for s in &sources.suites {
            let Some(release) = self.release(s)? else {
                continue;
            };
            for component in &sources.components {
                let dir = format!("{component}/binary-{arch}");
                let Some((path, hash, compression)) = index_file(&release, &dir) else {
                    debug!(message = logmsg::APT_NO_INDEX_LISTED, suite = %s.suite, dir = %dir);
                    continue;
                };
                let packed = self.package_index(s, &release, &path, hash)?;
                let text = compression
                    .decompress(&packed)
                    .map_err(|e| PortError::Backend {
                        detail: format!("{}: {path}: {e}", errmsg::APT_DECOMPRESS),
                    })?;
                add_package_versions(&String::from_utf8_lossy(&text), &mut available);
                any_index = true;
            }
        }
        if !any_index {
            let first = &sources.suites[0];
            return Err(PortError::Backend {
                detail: format!(
                    "{}: {} at {} for {arch}",
                    errmsg::APT_NO_INDEX,
                    first.suite,
                    first.mirror
                ),
            });
        }
        Ok(available)
    }
}

/// How a package index file is compressed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Compression {
    /// `Packages.gz`.
    Gzip,
    /// `Packages.xz`.
    Xz,
}

impl Compression {
    /// `packed`, decompressed. gzip members and xz streams are read to the end.
    pub fn decompress(self, packed: &[u8]) -> Result<Vec<u8>, String> {
        let mut text = Vec::new();
        match self {
            Compression::Gzip => {
                std::io::Read::read_to_end(
                    &mut flate2::read::MultiGzDecoder::new(packed),
                    &mut text,
                )
                .map_err(|e| e.to_string())?;
            }
            Compression::Xz => {
                lzma_rs::xz_decompress(&mut &packed[..], &mut text).map_err(|e| e.to_string())?;
            }
        }
        Ok(text)
    }
}

/// The package index a release lists in `dir` (`main/binary-amd64`): its
/// `Packages.gz`, else its `Packages.xz`. gzip first: Ubuntu writes its xz
/// indexes as two streams, which the pure-Rust xz decoder bosun builds with
/// does not read; Debian's security suite lists only xz.
pub fn index_file<'r>(
    release: &'r Release,
    dir: &str,
) -> Option<(String, &'r FileHash, Compression)> {
    [
        ("Packages.gz", Compression::Gzip),
        ("Packages.xz", Compression::Xz),
    ]
    .into_iter()
    .find_map(|(name, compression)| {
        let path = format!("{dir}/{name}");
        release
            .sha256
            .get(&path)
            .map(|hash| (path, hash, compression))
    })
}

/// The distro, codename and arch a deb identity names (§5.6); the arch falls
/// back to the target's.
fn release_of(pkg: &Ref, target: &Target) -> Result<(String, String, Arch), PortError> {
    let needs = || PortError::Backend {
        detail: format!("{}: {}", errmsg::APT_NO_DISTRO, pkg.purl),
    };
    let distro = pkg.purl.namespace().ok_or_else(needs)?;
    let codename = pkg.purl.qualifiers().get("distro").ok_or_else(needs)?;
    let arch = match (pkg.purl.qualifiers().get("arch"), target) {
        (Some(a), _) => a.parse().map_err(|_| needs())?,
        (None, Target::Platform { arch, .. }) => *arch,
        (None, Target::Any) => return Err(needs()),
    };
    Ok((distro.to_string(), codename.clone(), arch))
}

impl Index for RemoteAptIndex {
    fn refresh(&self) -> Result<(), PortError> {
        self.refetch.set(true);
        self.releases.borrow_mut().clear();
        Ok(())
    }

    fn query_available(&self, pkg: &Ref, target: &Target) -> Result<Vec<Version>, PortError> {
        let key = release_of(pkg, target)?;
        let known = self.releases.borrow().get(&key).cloned();
        let available = match known {
            Some(a) => a,
            None => {
                let (distro, codename, arch) = &key;
                let sources =
                    default_sources(distro, codename, *arch).ok_or_else(|| PortError::Backend {
                        detail: format!("{}: {distro}", errmsg::APT_NO_MIRROR),
                    })?;
                let read = Rc::new(self.read_release(&sources, *arch)?);
                self.releases.borrow_mut().insert(key.clone(), read.clone());
                read
            }
        };
        Ok(available
            .get(pkg.purl.name())
            .into_iter()
            .flatten()
            .filter_map(|v| Version::parse(v, Scheme::Debian).ok())
            .collect())
    }
}

#[cfg(test)]
mod tests;
