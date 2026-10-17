//! apt's remote index (§4.7): the versions a distro release's apt would offer,
//! for a machine bosun isn't running on (`render --target`, §8.3).
//!
//! A release's sources are the target's own, when bosun was given its root
//! filesystem (`render --image` / `--rootfs`) and the release is the root's;
//! else the suites a stock image of it lists (`bookworm`, `bookworm-updates`
//! and `bookworm-security` for Debian 12).
//!
//! When `apt-get` is on this machine, apt itself resolves them, in a scratch
//! root ([`super::scratch`]), so its priorities, pins and phasing apply.
//! Without it, bosun reads them itself, each suite fetched the way apt
//! fetches it: the suite's `InRelease`, then each component's `Packages.gz`
//! (else `.xz`), checked against the size and SHA256 the `InRelease` lists
//! (by hash when the release offers `Acquire-By-Hash`), each version given
//! the priority apt gives a release without pins. Either way, the choice
//! within a spec is apt's rule over those priorities ([`Index::select`]),
//! and a release file with no key to check it against is trusted as fetched
//! over HTTPS (the image's apt checks its signature when it installs).
//!
//! Fetched files are cached: an `InRelease` is used for [`RELEASE_TTL`] before
//! it is fetched again (an older one is used, with a warning, when the mirror
//! can't be reached), and a package index for as long as its hash is the one
//! the current `InRelease` lists. A scratch root's lists are updated after the
//! same time.

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use bosun_core::{Arch, Ref, Scheme, Target, Version, Want};
use bosun_ports::{CommandRunner, HttpFetcher, HttpRequest, Index, PortError, Selection};
use sha2::{Digest, Sha256};
use tracing::{debug, info, warn};

use super::policy::{Entry, Policy};
use super::scratch::{self, Scratch};
use super::sources::{self, SourceEntry};
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

/// One suite on one mirror, and the components read from it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Suite {
    /// The mirror's base URL (`https://deb.debian.org/debian`).
    pub mirror: String,
    /// The suite (`bookworm-updates`), or a flat repository's path (`./`).
    pub suite: String,
    /// The components (`main`, …); none for a flat repository.
    pub components: Vec<String>,
    /// Whether a missing suite is an error (the release itself) or skipped
    /// (its updates and security suites, which a release may not have yet).
    pub required: bool,
}

impl Suite {
    /// Whether this is a flat repository: no `dists/`, a path for a suite.
    pub fn is_flat(&self) -> bool {
        self.suite.ends_with('/')
    }

    /// The URL its release file and package indexes are under.
    pub fn dist_url(&self) -> String {
        if !self.is_flat() {
            return format!("{}/dists/{}", self.mirror, self.suite);
        }
        let path = self.suite.trim_end_matches('/').trim_start_matches("./");
        match path {
            "" | "." => self.mirror.clone(),
            path => format!("{}/{path}", self.mirror),
        }
    }

    /// The directories of its package indexes for `arch`, relative to
    /// [`Suite::dist_url`]: `<component>/binary-<arch>`, or the top for a
    /// flat repository.
    fn index_dirs(&self, arch: Arch) -> Vec<String> {
        if self.is_flat() {
            return vec![String::new()];
        }
        self.components
            .iter()
            .map(|c| format!("{c}/binary-{arch}"))
            .collect()
    }
}

/// The apt sources of a distro release: its suites, the release's own first.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Sources {
    /// The suites.
    pub suites: Vec<Suite>,
}

fn suite(mirror: &str, suite: String, components: &[&str], required: bool) -> Suite {
    Suite {
        mirror: mirror.to_string(),
        suite,
        components: components.iter().map(|c| c.to_string()).collect(),
        required,
    }
}

const UBUNTU_COMPONENTS: &[&str] = &["main", "restricted", "universe", "multiverse"];

/// The sources a stock container image of `distro` `codename` lists, for
/// `arch`: the release, its `-updates` and its `-security` suites. Debian's
/// `main`; Ubuntu's four components, from the ports mirror on arm64.
/// Backports are left out: apt never picks them unless asked. `None` for a
/// distro bosun knows no mirror for.
pub fn default_sources(distro: &str, codename: &str, arch: Arch) -> Option<Sources> {
    let (main, security, components): (_, _, &[&str]) = match (distro, arch) {
        ("debian", _) => (DEBIAN, DEBIAN_SECURITY, &["main"]),
        ("ubuntu", Arch::Amd64) => (UBUNTU, UBUNTU_SECURITY, UBUNTU_COMPONENTS),
        ("ubuntu", Arch::Arm64) => (UBUNTU_PORTS, UBUNTU_PORTS, UBUNTU_COMPONENTS),
        _ => return None,
    };
    Some(Sources {
        suites: vec![
            suite(main, codename.to_string(), components, true),
            suite(main, format!("{codename}-updates"), components, false),
            suite(security, format!("{codename}-security"), components, false),
        ],
    })
}

/// `uri` as bosun fetches it: an `http://` mirror is read over HTTPS, since
/// bosun trusts a release file for being fetched over HTTPS (its signature
/// is not checked).
pub fn https(uri: &str) -> String {
    match uri.strip_prefix("http://") {
        Some(rest) => format!("https://{rest}"),
        None => uri.to_string(),
    }
}

/// The suites `entries` name for `arch`, in order, each read over HTTPS. An
/// entry for other architectures is left out; entries naming one suite of
/// one mirror are read together. Every suite is required: the target's apt
/// fails its update without it.
pub fn sources_from(entries: &[SourceEntry], arch: Arch) -> Sources {
    let arch = arch.to_string();
    let mut suites: Vec<Suite> = Vec::new();
    for e in entries.iter().filter(|e| e.applies_to(&arch)) {
        let mirror = https(&e.uri);
        if mirror != e.uri {
            debug!(message = logmsg::APT_HTTPS, uri = %e.uri);
        }
        match suites
            .iter_mut()
            .find(|s| s.mirror == mirror && s.suite == e.suite)
        {
            Some(known) => {
                for c in &e.components {
                    if !known.components.contains(c) {
                        known.components.push(c.clone());
                    }
                }
            }
            None => suites.push(Suite {
                mirror,
                suite: e.suite.clone(),
                components: e.components.clone(),
                required: true,
            }),
        }
    }
    Sources { suites }
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
    /// `NotAutomatic: yes` (backports, experimental).
    pub not_automatic: bool,
    /// `ButAutomaticUpgrades: yes`.
    pub but_automatic_upgrades: bool,
    /// The `SHA256` section, by path (`main/binary-amd64/Packages.xz`).
    pub sha256: BTreeMap<String, FileHash>,
}

impl Release {
    /// Whether this is the release file of `suite`: apt's suites are named by
    /// codename (`bookworm`, `bookworm-security`) or by suite (`noble-updates`).
    pub fn is_for(&self, suite: &str) -> bool {
        self.suite.as_deref() == Some(suite) || self.codename.as_deref() == Some(suite)
    }

    /// The priority apt gives this release's versions with no pin
    /// (apt_preferences(5)): 1 when `NotAutomatic`, 100 when also
    /// `ButAutomaticUpgrades`, else 500.
    pub fn priority(&self) -> i32 {
        match (self.not_automatic, self.but_automatic_upgrades) {
            (false, _) => 500,
            (true, true) => 100,
            (true, false) => 1,
        }
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
            "NotAutomatic" => release.not_automatic = value == "yes",
            "ButAutomaticUpgrades" => release.but_automatic_upgrades = value == "yes",
            _ => {}
        }
    }
    Ok(release)
}

/// Each package's versions in a `Packages` file of a release whose versions
/// apt gives `priority`, added to `into`. A version listed more than once
/// keeps the highest priority, as apt's does.
pub fn add_package_versions(packages: &str, priority: i32, into: &mut Available) {
    let mut name: Option<&str> = None;
    let mut version: Option<&str> = None;
    for line in packages.lines().chain(std::iter::once("")) {
        if line.is_empty() {
            if let (Some(n), Some(v)) = (name.take(), version.take()) {
                let versions = into.entry(n.to_string()).or_default();
                match versions.iter_mut().find(|(known, _)| known == v) {
                    Some((_, p)) => *p = (*p).max(priority),
                    None => versions.push((v.to_string(), priority)),
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
pub(super) fn fresh(path: &Path, ttl: Duration) -> bool {
    fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|written| SystemTime::now().duration_since(written).ok())
        .is_some_and(|age| age < ttl)
}

/// Package name → its versions across a release's suites and components,
/// each with the priority apt gives it.
pub type Available = BTreeMap<String, Vec<(String, i32)>>;

/// A release, as its distro, codename and arch.
type ReleaseKey = (String, String, Arch);

/// The machine a render targets, read from its root filesystem (an image's
/// files, or a directory): the release it is, and the root holding its
/// `etc/apt`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TargetRoot {
    /// The distro's `ID` (`debian`).
    pub distro: String,
    /// Its `VERSION_CODENAME` (`bookworm`).
    pub codename: String,
    /// The root directory.
    pub path: PathBuf,
}

/// apt's [`Index`] over a release's mirror (§4.7). The release comes from the
/// identity: the purl's namespace is the distro (`debian`), its `distro`
/// qualifier the codename, its `arch` qualifier the architecture. Its
/// sources are the target root's when it is that release, else the ones a
/// stock image of the release lists.
pub struct RemoteAptIndex {
    fetcher: Arc<dyn HttpFetcher>,
    cache: PathBuf,
    root: Option<TargetRoot>,
    /// Runs this machine's apt, when it is to be asked.
    apt: Option<Arc<dyn CommandRunner>>,
    /// Each release read this run, by distro, codename and arch.
    releases: RefCell<BTreeMap<ReleaseKey, Rc<Available>>>,
    /// Each release's scratch root this run; `None` when apt can't read it.
    scratches: RefCell<BTreeMap<ReleaseKey, Option<Rc<Scratch>>>>,
    /// Set by `refresh`: fetch every `InRelease` again, however fresh.
    refetch: Cell<bool>,
}

/// Where a release's sources come from.
enum Chosen<'a> {
    /// The target's root lists them.
    Root(&'a Path, Vec<SourceEntry>),
    /// What a stock image of the release lists.
    Stock(Sources),
}

/// Whether the root at `path` pins packages: an `etc/apt/preferences`, or
/// any file in `etc/apt/preferences.d`.
fn has_pins(path: &Path) -> bool {
    let apt = path.join("etc/apt");
    apt.join("preferences").is_file()
        || fs::read_dir(apt.join("preferences.d")).is_ok_and(|mut d| d.next().is_some())
}

/// `sources` as source list entries.
fn entries_of(sources: &Sources) -> Vec<SourceEntry> {
    sources
        .suites
        .iter()
        .map(|s| SourceEntry {
            uri: s.mirror.clone(),
            suite: s.suite.clone(),
            components: s.components.clone(),
            ..SourceEntry::default()
        })
        .collect()
}

impl RemoteAptIndex {
    /// An index fetching through `fetcher`, caching under `cache`.
    pub fn new(fetcher: Arc<dyn HttpFetcher>, cache: PathBuf) -> RemoteAptIndex {
        RemoteAptIndex {
            fetcher,
            cache,
            root: None,
            apt: None,
            releases: RefCell::new(BTreeMap::new()),
            scratches: RefCell::new(BTreeMap::new()),
            refetch: Cell::new(false),
        }
    }

    /// Read the sources of `root`'s release from its `etc/apt`.
    pub fn with_root(mut self, root: Option<TargetRoot>) -> RemoteAptIndex {
        self.root = root;
        self
    }

    /// Through this machine's apt, when `apt-get` is here (§4.7): it resolves
    /// in a scratch root under the cache, run through `runner`, and bosun's
    /// own reading of the mirrors is used only without it.
    pub fn with_local_apt(mut self, runner: Option<Arc<dyn CommandRunner>>) -> RemoteAptIndex {
        self.apt = runner;
        self
    }

    /// The sources of `distro` `codename` for `arch`: the target root's when
    /// it is that release, else a stock image's.
    fn chosen(&self, distro: &str, codename: &str, arch: Arch) -> Result<Chosen<'_>, PortError> {
        match &self.root {
            Some(root) if root.distro == distro && root.codename == codename => {
                let entries = sources::read_root(&root.path);
                let arch_name = arch.to_string();
                if !entries.iter().any(|e| e.applies_to(&arch_name)) {
                    return Err(PortError::Backend {
                        detail: format!(
                            "{}: {} for {arch}",
                            errmsg::APT_NO_SOURCES,
                            root.path.display()
                        ),
                    });
                }
                Ok(Chosen::Root(&root.path, entries))
            }
            _ => {
                let sources =
                    default_sources(distro, codename, arch).ok_or_else(|| PortError::Backend {
                        detail: format!("{}: {distro}", errmsg::APT_NO_MIRROR),
                    })?;
                info!(message = logmsg::APT_STOCK_SOURCES, distro = %distro, release = %codename);
                Ok(Chosen::Stock(sources))
            }
        }
    }

    /// The versions of `name` and their priorities as this machine's apt
    /// gives them for `key`'s release; `None` when apt isn't here, or when
    /// it can't read a stock image's sources (a release without an updates
    /// suite), which bosun then reads itself.
    fn through_apt(&self, key: &ReleaseKey, name: &str) -> Result<Option<Policy>, PortError> {
        let Some(runner) = &self.apt else {
            return Ok(None);
        };
        let known = self.scratches.borrow().get(key).cloned();
        let scratch = match known {
            Some(s) => s,
            None => {
                let (distro, codename, arch) = key;
                let chosen = self.chosen(distro, codename, *arch)?;
                let plan = match &chosen {
                    Chosen::Root(path, entries) => scratch::plan(Some(path), entries, *arch),
                    Chosen::Stock(sources) => scratch::plan(None, &entries_of(sources), *arch),
                };
                let s = Scratch::write(&self.cache, &plan)?;
                let s = match s.update(runner.as_ref(), self.refetch.get()) {
                    Ok(()) => {
                        info!(message = logmsg::APT_THROUGH_LOCAL_APT, dir = %s.dir().display());
                        Some(Rc::new(s))
                    }
                    Err(PortError::CommandNotFound { .. }) => {
                        info!(message = logmsg::APT_NO_LOCAL_APT);
                        None
                    }
                    Err(e) if matches!(chosen, Chosen::Stock(_)) => {
                        info!(message = logmsg::APT_SCRATCH_STOCK, error = %e);
                        None
                    }
                    Err(e) => return Err(e),
                };
                self.scratches.borrow_mut().insert(key.clone(), s.clone());
                s
            }
        };
        let Some(s) = scratch else {
            return Ok(None);
        };
        match s.policy(runner.as_ref(), name) {
            Ok(policy) => Ok(Some(policy)),
            // Lists fetched before, but no apt here now.
            Err(PortError::CommandNotFound { .. }) => {
                info!(message = logmsg::APT_NO_LOCAL_APT);
                self.scratches.borrow_mut().insert(key.clone(), None);
                Ok(None)
            }
            Err(e) => Err(e),
        }
    }

    fn suite_dir(&self, s: &Suite) -> PathBuf {
        self.cache.join(cache_name(&s.mirror)).join(&s.suite)
    }

    /// The release file of suite `s`; `None` when the mirror has no such
    /// suite and it isn't required.
    fn release(&self, s: &Suite) -> Result<Option<Release>, PortError> {
        let url = format!("{}/InRelease", s.dist_url());
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
        // A flat repository's release file names no suite of its path.
        if !s.is_flat() && !release.is_for(&s.suite) {
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
        let base = s.dist_url();
        let url = if release.by_hash {
            let dir = path.rsplit_once('/').map_or("", |(dir, _)| dir);
            let dir = if dir.is_empty() {
                String::new()
            } else {
                format!("{dir}/")
            };
            format!("{base}/{dir}by-hash/SHA256/{}", hash.sha256)
        } else {
            format!("{base}/{path}")
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
            for dir in s.index_dirs(arch) {
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
                add_package_versions(
                    &String::from_utf8_lossy(&text),
                    release.priority(),
                    &mut available,
                );
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
        let path = if dir.is_empty() {
            name.to_string()
        } else {
            format!("{dir}/{name}")
        };
        release
            .sha256
            .get(&path)
            .map(|hash| (path, hash, compression))
    })
}

/// The distro, codename and arch a deb identity names (§5.6); the arch falls
/// back to the target's.
fn release_of(pkg: &Ref, target: &Target) -> Result<ReleaseKey, PortError> {
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

impl RemoteAptIndex {
    /// `pkg`'s versions on `target` and their priorities: as this machine's
    /// apt gives them, else as bosun reads the mirrors (apt's priorities with
    /// no pins).
    fn policy(&self, pkg: &Ref, target: &Target) -> Result<Policy, PortError> {
        let key = release_of(pkg, target)?;
        let name = pkg.purl.name();
        if let Some(policy) = self.through_apt(&key, name)? {
            return Ok(policy);
        }
        let known = self.releases.borrow().get(&key).cloned();
        let available = match known {
            Some(a) => a,
            None => {
                let (distro, codename, arch) = &key;
                let sources = match self.chosen(distro, codename, *arch)? {
                    Chosen::Root(path, entries) => {
                        if has_pins(path) {
                            warn!(message = logmsg::APT_PINS_IGNORED, root = %path.display());
                        }
                        sources_from(&entries, *arch)
                    }
                    Chosen::Stock(sources) => sources,
                };
                let read = Rc::new(self.read_release(&sources, *arch)?);
                self.releases.borrow_mut().insert(key.clone(), read.clone());
                read
            }
        };
        Ok(policy_of(
            available.get(name).map(Vec::as_slice).unwrap_or_default(),
        ))
    }
}

/// `versions` and their priorities as apt's policy: every one offered,
/// nothing installed.
fn policy_of(versions: &[(String, i32)]) -> Policy {
    Policy {
        installed: None,
        entries: versions
            .iter()
            .filter_map(|(v, priority)| {
                Some(Entry {
                    version: Version::parse(v, Scheme::Debian).ok()?,
                    priority: *priority,
                    offered: true,
                })
            })
            .collect(),
    }
}

impl Index for RemoteAptIndex {
    fn refresh(&self) -> Result<(), PortError> {
        self.refetch.set(true);
        self.releases.borrow_mut().clear();
        self.scratches.borrow_mut().clear();
        Ok(())
    }

    fn query_available(&self, pkg: &Ref, target: &Target) -> Result<Vec<Version>, PortError> {
        Ok(self.policy(pkg, target)?.offered())
    }

    /// apt's choice among the versions `want` admits (§5.6): the highest
    /// priority, then the newest, a locked version first while offered.
    fn select(
        &self,
        pkg: &Ref,
        target: &Target,
        want: &Want,
        prefer: &[Version],
    ) -> Result<Selection, PortError> {
        let policy = self.policy(pkg, target)?;
        Ok(Selection {
            chosen: policy.choose(want, prefer),
            available: policy.offered(),
        })
    }
}

#[cfg(test)]
mod tests;
