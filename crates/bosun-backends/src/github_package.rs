//! The github-deb and github-rpm backends: install the `.deb` or `.rpm` a
//! project attaches to its GitHub release, through the system's own package
//! manager, so dependencies resolve and the package is the system's to verify
//! and hold. The release is found the github-release way ([`ReleaseLookup`]:
//! tag pattern, asset template, published checksum); the asset template is
//! required, since OS package names have no common default.
//!
//! System scope (§4.6): the download goes to a root-owned cache
//! (`<cache>/<backend>/<package>/<asset>`), then `apt-get install <file>` or
//! `dnf install <file>`. `verify_installed` asks dpkg or rpm by package name
//! (the `package_name` setting, default the binary name) and reports presence
//! only: the package's own version is in its manager's scheme, while bosun
//! compares release versions, so bosun state holds the version (§4.6), as for
//! github-release.

use std::path::PathBuf;
use std::sync::Arc;

use bosun_core::{
    Arch, BackendId, Capabilities, Checksum, HoldSupport, Installed, LibcFamily, Os, Ref, Scheme,
    Scope, Source, Target, Version,
};
use bosun_ports::{
    Backend, BackendFactory, BackendMeta, Command, CommandRunner, HostInfo, HttpFetcher, Index,
    Installer, PortError, Probe, Step, Steps, UserInstalls,
};

use crate::errmsg;
use crate::github_release::{ReleaseFile, ReleaseLookup};

/// Which kind of OS package a release carries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PackageKind {
    /// A Debian package, installed with apt.
    Deb,
    /// An RPM package, installed with dnf.
    Rpm,
}

impl PackageKind {
    fn id(self) -> &'static str {
        match self {
            PackageKind::Deb => "github-deb",
            PackageKind::Rpm => "github-rpm",
        }
    }

    /// The package manager whose presence registers the backend (§5.3).
    fn manager(self) -> &'static str {
        match self {
            PackageKind::Deb => "apt-get",
            PackageKind::Rpm => "dnf",
        }
    }

    fn accepts(self, name: &str) -> bool {
        let lower = name.to_ascii_lowercase();
        match self {
            PackageKind::Deb => lower.ends_with(".deb"),
            PackageKind::Rpm => lower.ends_with(".rpm"),
        }
    }

    fn refusal(self) -> &'static str {
        match self {
            PackageKind::Deb => errmsg::NOT_A_DEB,
            PackageKind::Rpm => errmsg::NOT_AN_RPM,
        }
    }
}

/// Builds github-deb or github-rpm when its package manager is present.
pub struct GithubPackageFactory {
    kind: PackageKind,
    runner: Arc<dyn CommandRunner>,
    fetcher: Arc<dyn HttpFetcher>,
    cache_dir: PathBuf,
    os: Os,
    arch: Arch,
    libc: Option<LibcFamily>,
}

impl GithubPackageFactory {
    /// Construct for `kind`, with the runner the package manager is driven
    /// through, the HTTP fetcher for releases, the root-owned download cache
    /// (`/var/cache/bosun` on Linux) and the host os/arch.
    pub fn new(
        kind: PackageKind,
        runner: Arc<dyn CommandRunner>,
        fetcher: Arc<dyn HttpFetcher>,
        cache_dir: PathBuf,
        os: Os,
        arch: Arch,
    ) -> GithubPackageFactory {
        GithubPackageFactory {
            kind,
            runner,
            fetcher,
            cache_dir,
            os,
            arch,
            libc: None,
        }
    }

    /// The host's C library family, for the `libc` and `rust_target` asset
    /// template variables (§5.4).
    pub fn with_libc(mut self, libc: Option<LibcFamily>) -> GithubPackageFactory {
        self.libc = libc;
        self
    }
}

impl BackendFactory for GithubPackageFactory {
    fn id(&self) -> BackendId {
        BackendId::new(self.kind.id())
    }

    fn meta(&self) -> BackendMeta {
        BackendMeta {
            // The identity is the release's repository; versions are its tags.
            purl_type: "github",
            vers_type: "semver",
            platform_independent: false,
            option_keys: &["asset", "tag", "package_name", "unverified"],
        }
    }

    fn probe(&self, host: &HostInfo) -> Result<Option<Probe>, PortError> {
        if host.os != Os::Linux {
            return Ok(None);
        }
        let out = host.runner.run(&Command::new(
            self.kind.manager(),
            ["--version".to_string()],
        ))?;
        if !out.success() {
            return Ok(None);
        }
        Ok(Some(Probe {
            backend: self.id(),
            version: manager_version(&out.stdout_utf8()),
        }))
    }

    fn build(&self, probe: Probe) -> Backend {
        let backend = self.backend(Some(probe.version));
        Backend {
            index: Box::new(backend.clone()),
            installer: Box::new(backend),
        }
    }

    fn remote_index(&self) -> Option<Box<dyn Index>> {
        // The release's packages and checksums come from GitHub's API.
        Some(Box::new(self.backend(None)))
    }
}

impl GithubPackageFactory {
    fn backend(&self, version: Option<Version>) -> GithubPackageBackend {
        GithubPackageBackend {
            kind: self.kind,
            runner: self.runner.clone(),
            releases: ReleaseLookup::new(self.fetcher.clone(), self.os, self.arch)
                .with_libc(self.libc),
            cache_dir: self.cache_dir.clone(),
            _version: version,
        }
    }
}

/// The first word of `--version` output that reads as a version (`apt 2.6.1
/// (amd64)` → `2.6.1`, dnf's `4.18.0` → `4.18.0`), else `0`.
fn manager_version(stdout: &str) -> Version {
    stdout
        .split_whitespace()
        .find(|w| w.starts_with(|c: char| c.is_ascii_digit()))
        .and_then(|w| Version::parse(w, Scheme::Generic).ok())
        .unwrap_or_else(|| Version::parse("0", Scheme::Generic).expect("0 is a version"))
}

#[derive(Clone)]
struct GithubPackageBackend {
    kind: PackageKind,
    runner: Arc<dyn CommandRunner>,
    releases: ReleaseLookup,
    cache_dir: PathBuf,
    // Recorded for future per-version flag gating (Appendix B #224); none
    // for a remote index, which runs no package manager.
    _version: Option<Version>,
}

fn argv(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|s| s.to_string()).collect()
}

/// `DEBIAN_FRONTEND=noninteractive` for apt, nothing for dnf.
fn env_for(kind: PackageKind) -> Vec<(String, String)> {
    match kind {
        PackageKind::Deb => vec![("DEBIAN_FRONTEND".to_string(), "noninteractive".to_string())],
        PackageKind::Rpm => vec![],
    }
}

impl GithubPackageBackend {
    /// The OS package's name: the `package_name` setting, else the binary name.
    fn package_name(pkg: &Ref) -> String {
        pkg.purl
            .qualifiers()
            .get("package_name")
            .cloned()
            .unwrap_or_else(|| ReleaseLookup::binary_of(pkg))
    }

    /// The release file to install, refused when it can't be verified and
    /// the entry hasn't allowed that (it installs as root).
    fn file(&self, pkg: &Ref, version: &Version) -> Result<ReleaseFile, PortError> {
        let file = self.releases.file(
            pkg,
            version,
            |name| self.kind.accepts(name),
            self.kind.refusal(),
        )?;
        ReleaseLookup::require_verifiable(pkg, &file, self.kind.id())?;
        Ok(file)
    }

    fn run(&self, parts: &[&str]) -> Step {
        Step::Run {
            argv: argv(parts),
            env: env_for(self.kind),
            scope: Scope::System,
            ok_exits: Vec::new(),
        }
    }

    /// Fetch the package file, then hand it to the package manager to install
    /// or downgrade. apt refuses to change a held package under `-y`, and
    /// bosun holds the `.deb`s it installs, so apt releases the hold first.
    fn fetch_and(&self, pkg: &Ref, version: &Version, downgrade: bool) -> Result<Steps, PortError> {
        let file = self.file(pkg, version)?;
        let dest = self
            .cache_dir
            .join(self.kind.id())
            .join(Self::package_name(pkg))
            .join(&file.name);
        let path = dest.display().to_string();
        let install = match (self.kind, downgrade) {
            (PackageKind::Deb, false) => {
                self.run(&["apt-get", "install", "-y", "--no-install-recommends", &path])
            }
            (PackageKind::Deb, true) => {
                self.run(&["apt-get", "install", "-y", "--allow-downgrades", &path])
            }
            (PackageKind::Rpm, false) => self.run(&["dnf", "install", "-y", &path]),
            (PackageKind::Rpm, true) => self.run(&["dnf", "downgrade", "-y", &path]),
        };
        let mut steps = vec![Step::Fetch {
            url: file.url,
            sha256: file.sha256,
            dest,
        }];
        if self.kind == PackageKind::Deb {
            steps.push(self.run(&["apt-mark", "unhold", &Self::package_name(pkg)]));
        }
        steps.push(install);
        Ok(steps)
    }
}

impl Index for GithubPackageBackend {
    fn refresh(&self) -> Result<(), PortError> {
        Ok(()) // releases are fetched fresh per query
    }

    fn query_available(&self, pkg: &Ref, target: &Target) -> Result<Vec<Version>, PortError> {
        self.releases.versions(pkg, target)
    }

    fn checksum(
        &self,
        pkg: &Ref,
        version: &Version,
        _target: &Target,
    ) -> Result<Option<Checksum>, PortError> {
        let file = self.file(pkg, version)?;
        Ok(Some(Checksum(self.releases.recorded_sha256(pkg, &file)?)))
    }
}

impl Installer for GithubPackageBackend {
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            supports_exact_version: true,
            // apt holds natively; dnf needs its versionlock plugin, which a
            // host may not have, so github-rpm does not claim a hold.
            hold: match self.kind {
                PackageKind::Deb => HoldSupport::Supported,
                PackageKind::Rpm => HoldSupport::Unsupported,
            },
            supports_downgrade: true,
        }
    }

    fn scope(&self) -> Scope {
        Scope::System
    }

    fn ensure_source(&self, _source: &Source) -> Result<Steps, PortError> {
        Ok(vec![]) // the repository in the purl is the source
    }

    fn install_exact(&self, pkg: &Ref, version: &Version) -> Result<Steps, PortError> {
        self.fetch_and(pkg, version, false)
    }

    fn verify_installed(&self, pkg: &Ref) -> Result<Installed, PortError> {
        let name = Self::package_name(pkg);
        let present = match self.kind {
            PackageKind::Deb => {
                // `${Status}` is `install ok installed` only for a package that
                // is installed (not removed with config files left).
                let out = self.runner.run(&Command::new(
                    "dpkg-query",
                    argv(&["-W", "-f=${Status}", &name]),
                ))?;
                out.success() && out.stdout_utf8().trim() == "install ok installed"
            }
            PackageKind::Rpm => self
                .runner
                .run(&Command::new("rpm", argv(&["-q", &name])))?
                .success(),
        };
        Ok(if present {
            Installed::Present { version: None }
        } else {
            Installed::Absent
        })
    }

    fn hold(&self, pkg: &Ref, _version: &Version) -> Result<Steps, PortError> {
        Ok(match self.kind {
            PackageKind::Deb => vec![self.run(&["apt-mark", "hold", &Self::package_name(pkg)])],
            PackageKind::Rpm => vec![],
        })
    }

    fn unhold(&self, pkg: &Ref) -> Result<Steps, PortError> {
        Ok(match self.kind {
            PackageKind::Deb => vec![self.run(&["apt-mark", "unhold", &Self::package_name(pkg)])],
            PackageKind::Rpm => vec![],
        })
    }

    fn downgrade(&self, pkg: &Ref, version: &Version) -> Result<Steps, PortError> {
        self.fetch_and(pkg, version, true)
    }

    fn uninstall(&self, pkg: &Ref) -> Result<Steps, PortError> {
        let name = Self::package_name(pkg);
        Ok(match self.kind {
            // Release the hold first, as apt does (§9).
            PackageKind::Deb => vec![
                self.run(&["apt-mark", "unhold", &name]),
                self.run(&["apt-get", "remove", "-y", &name]),
            ],
            PackageKind::Rpm => vec![self.run(&["dnf", "remove", "-y", &name])],
        })
    }

    fn user_installs(&self) -> Result<UserInstalls, PortError> {
        // apt and dnf list these as the user's own, but only bosun knows the
        // release each came from.
        Ok(UserInstalls::BosunState)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bosun_ports::testing::{FakeCommandRunner, FakeHttpFetcher};
    use bosun_ports::Output;
    use sha2::Digest;

    const TAG_URL: &str = "https://api.github.com/repos/sharkdp/bat/releases/tags/v0.26.1";
    const RELEASE: &str = r#"{"tag_name":"v0.26.1","assets":[
        {"name":"bat_0.26.1_amd64.deb","browser_download_url":"https://x/bat_0.26.1_amd64.deb"},
        {"name":"bat-0.26.1-1.x86_64.rpm","browser_download_url":"https://x/bat.rpm"},
        {"name":"bat-v0.26.1-x86_64-unknown-linux-musl.tar.gz","browser_download_url":"https://x/bat.tgz"}]}"#;

    /// bat with the given options. The fixture release publishes no checksum,
    /// so it also allows an unverified install; see [`strict_pkg`].
    fn pkg(pairs: &[(&str, &str)]) -> Ref {
        let mut all = vec![("unverified", "allow")];
        all.extend_from_slice(pairs);
        strict_pkg(&all)
    }

    /// bat with exactly the given options.
    fn strict_pkg(pairs: &[(&str, &str)]) -> Ref {
        let mut q = std::collections::BTreeMap::from([("binary".to_string(), "bat".to_string())]);
        for (k, v) in pairs {
            q.insert(k.to_string(), v.to_string());
        }
        Ref::new(
            bosun_core::Purl::from_components("github", Some("sharkdp"), "bat", None, q, None)
                .unwrap(),
        )
    }

    fn backend(kind: PackageKind, runner: FakeCommandRunner) -> GithubPackageBackend {
        let fetcher = Arc::new(
            FakeHttpFetcher::new()
                .with_body(TAG_URL, RELEASE)
                .with_body("https://x/bat_0.26.1_amd64.deb", "the .deb"),
        );
        GithubPackageBackend {
            kind,
            runner: Arc::new(runner),
            releases: ReleaseLookup::new(fetcher, Os::Linux, Arch::Amd64),
            cache_dir: PathBuf::from("/var/cache/bosun"),
            _version: Some(Version::parse("2.6.1", Scheme::Generic).unwrap()),
        }
    }

    fn v(s: &str) -> Version {
        Version::parse(s, Scheme::Semver).unwrap()
    }

    fn sys(argv: &[&str], env: &[(&str, &str)]) -> Step {
        Step::Run {
            argv: argv.iter().map(|s| s.to_string()).collect(),
            env: env
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            scope: Scope::System,
            ok_exits: Vec::new(),
        }
    }

    const NONINTERACTIVE: &[(&str, &str)] = &[("DEBIAN_FRONTEND", "noninteractive")];

    #[test]
    fn a_deb_is_fetched_into_the_cache_then_unheld_and_installed_with_apt() {
        let b = backend(PackageKind::Deb, FakeCommandRunner::new());
        let steps = b
            .install_exact(
                &pkg(&[("asset", "{{ binary }}_{{ version }}_{{ arch }}.deb")]),
                &v("0.26.1"),
            )
            .unwrap();
        let path = "/var/cache/bosun/github-deb/bat/bat_0.26.1_amd64.deb";
        assert_eq!(
            steps,
            vec![
                Step::Fetch {
                    url: "https://x/bat_0.26.1_amd64.deb".into(),
                    sha256: None,
                    dest: PathBuf::from(path),
                },
                sys(&["apt-mark", "unhold", "bat"], NONINTERACTIVE),
                sys(
                    &["apt-get", "install", "-y", "--no-install-recommends", path],
                    NONINTERACTIVE
                ),
            ]
        );
    }

    #[test]
    fn an_rpm_is_installed_with_dnf_under_its_package_name() {
        let b = backend(PackageKind::Rpm, FakeCommandRunner::new());
        let p = pkg(&[
            ("asset", "{{ binary }}-{{ version }}-1.{{ arch_uname }}.rpm"),
            ("package_name", "bat-cat"),
        ]);
        let steps = b.install_exact(&p, &v("0.26.1")).unwrap();
        let path = "/var/cache/bosun/github-rpm/bat-cat/bat-0.26.1-1.x86_64.rpm";
        assert_eq!(steps[1], sys(&["dnf", "install", "-y", path], &[]));
        assert_eq!(
            b.downgrade(&p, &v("0.26.1")).unwrap()[1],
            sys(&["dnf", "downgrade", "-y", path], &[])
        );
        assert_eq!(
            b.uninstall(&p).unwrap(),
            vec![sys(&["dnf", "remove", "-y", "bat-cat"], &[])]
        );
    }

    #[test]
    fn a_deb_downgrades_holds_and_uninstalls_through_apt() {
        let b = backend(PackageKind::Deb, FakeCommandRunner::new());
        let p = pkg(&[("asset", "{{ binary }}_{{ version }}_{{ arch }}.deb")]);
        assert_eq!(
            b.downgrade(&p, &v("0.26.1")).unwrap()[1..],
            [
                sys(&["apt-mark", "unhold", "bat"], NONINTERACTIVE),
                sys(
                    &[
                        "apt-get",
                        "install",
                        "-y",
                        "--allow-downgrades",
                        "/var/cache/bosun/github-deb/bat/bat_0.26.1_amd64.deb"
                    ],
                    NONINTERACTIVE
                ),
            ]
        );
        assert_eq!(
            b.hold(&p, &v("0.26.1")).unwrap(),
            vec![sys(&["apt-mark", "hold", "bat"], NONINTERACTIVE)]
        );
        assert_eq!(
            b.unhold(&p).unwrap(),
            vec![sys(&["apt-mark", "unhold", "bat"], NONINTERACTIVE)]
        );
        assert_eq!(
            b.uninstall(&p).unwrap(),
            vec![
                sys(&["apt-mark", "unhold", "bat"], NONINTERACTIVE),
                sys(&["apt-get", "remove", "-y", "bat"], NONINTERACTIVE),
            ]
        );
    }

    #[test]
    fn what_the_user_installed_is_what_bosun_recorded() {
        for kind in [PackageKind::Deb, PackageKind::Rpm] {
            assert_eq!(
                backend(kind, FakeCommandRunner::new())
                    .user_installs()
                    .unwrap(),
                UserInstalls::BosunState
            );
        }
    }

    #[test]
    fn an_asset_of_the_wrong_kind_is_refused() {
        let deb = backend(PackageKind::Deb, FakeCommandRunner::new());
        let err = deb
            .checksum(
                &pkg(&[("asset", "{{ binary }}-{{ version }}-1.{{ arch_uname }}.rpm")]),
                &v("0.26.1"),
                &"linux/amd64".parse().unwrap(),
            )
            .unwrap_err();
        assert!(matches!(err, PortError::Backend { detail } if detail.contains(errmsg::NOT_A_DEB)));
        let rpm = backend(PackageKind::Rpm, FakeCommandRunner::new());
        let err = rpm
            .install_exact(
                &pkg(&[("asset", "{{ binary }}_{{ version }}_{{ arch }}.deb")]),
                &v("0.26.1"),
            )
            .unwrap_err();
        assert!(
            matches!(err, PortError::Backend { detail } if detail.contains(errmsg::NOT_AN_RPM))
        );
    }

    #[test]
    fn without_an_asset_template_there_is_nothing_to_look_for() {
        let b = backend(PackageKind::Deb, FakeCommandRunner::new());
        let err = b.install_exact(&pkg(&[]), &v("0.26.1")).unwrap_err();
        assert!(
            matches!(err, PortError::Backend { detail } if detail.contains(errmsg::ASSET_REQUIRED))
        );
    }

    #[test]
    fn dpkg_reports_presence_only_for_an_installed_package() {
        let installed = FakeCommandRunner::new().with_stdout("install ok installed");
        let p = pkg(&[]);
        assert_eq!(
            backend(PackageKind::Deb, installed)
                .verify_installed(&p)
                .unwrap(),
            Installed::Present { version: None }
        );
        let config_left = FakeCommandRunner::new().with_stdout("deinstall ok config-files");
        assert_eq!(
            backend(PackageKind::Deb, config_left)
                .verify_installed(&p)
                .unwrap(),
            Installed::Absent
        );
        let unknown = FakeCommandRunner::new().with_output(Output {
            code: Some(1),
            stdout: Vec::new(),
            stderr: b"no packages found".to_vec(),
        });
        assert_eq!(
            backend(PackageKind::Deb, unknown)
                .verify_installed(&p)
                .unwrap(),
            Installed::Absent
        );
    }

    #[test]
    fn rpm_reports_presence_by_its_exit_status() {
        let p = pkg(&[]);
        let yes = FakeCommandRunner::new().with_stdout("bat-0.26.1-1.x86_64");
        assert_eq!(
            backend(PackageKind::Rpm, yes).verify_installed(&p).unwrap(),
            Installed::Present { version: None }
        );
        let no = FakeCommandRunner::new().with_output(Output {
            code: Some(1),
            stdout: b"package bat is not installed".to_vec(),
            stderr: Vec::new(),
        });
        assert_eq!(
            backend(PackageKind::Rpm, no).verify_installed(&p).unwrap(),
            Installed::Absent
        );
    }

    #[test]
    fn versions_are_the_releases_tags() {
        let list = "https://api.github.com/repos/sharkdp/bat/releases?per_page=100&page=1";
        let fetcher = Arc::new(FakeHttpFetcher::new().with_body(
            list,
            r#"[{"tag_name":"v0.26.1","assets":[]},{"tag_name":"v0.25.0","assets":[]}]"#,
        ));
        let mut b = backend(PackageKind::Deb, FakeCommandRunner::new());
        b.releases = ReleaseLookup::new(fetcher, Os::Linux, Arch::Amd64);
        let versions: Vec<String> = b
            .query_available(&pkg(&[]), &"linux/amd64".parse().unwrap())
            .unwrap()
            .iter()
            .map(|v| v.as_str().to_string())
            .collect();
        assert_eq!(versions, vec!["0.26.1", "0.25.0"]);
    }

    const DEB: &str = "{{ binary }}_{{ version }}_{{ arch }}.deb";

    #[test]
    fn a_release_without_a_checksum_is_refused_unless_the_entry_allows_it() {
        // github-deb installs as root: an unverifiable package needs consent.
        let b = backend(PackageKind::Deb, FakeCommandRunner::new());
        let strict = strict_pkg(&[("asset", DEB)]);
        let at = "linux/amd64".parse().unwrap();
        for err in [
            b.install_exact(&strict, &v("0.26.1")).unwrap_err(),
            b.checksum(&strict, &v("0.26.1"), &at).unwrap_err(),
        ] {
            let text = err.to_string();
            assert!(text.contains("bat_0.26.1_amd64.deb"), "{text}");
            assert!(
                text.contains("backends.github-deb.unverified: allow"),
                "{text}"
            );
        }
    }

    #[test]
    fn an_allowed_unverified_release_locks_the_sha256_it_downloads_as() {
        let b = backend(PackageKind::Deb, FakeCommandRunner::new());
        let got = b
            .checksum(
                &pkg(&[("asset", DEB)]),
                &v("0.26.1"),
                &"linux/amd64".parse().unwrap(),
            )
            .unwrap();
        let want: String = sha2::Sha256::digest(b"the .deb")
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        assert_eq!(got, Some(Checksum(want)));
    }

    #[test]
    fn unverified_takes_only_allow() {
        let b = backend(PackageKind::Deb, FakeCommandRunner::new());
        let err = b
            .install_exact(
                &strict_pkg(&[("asset", DEB), ("unverified", "yes")]),
                &v("0.26.1"),
            )
            .unwrap_err();
        assert!(err.to_string().contains("\"yes\""), "{err}");
    }

    #[test]
    fn a_deb_holds_and_an_rpm_does_not() {
        let deb = backend(PackageKind::Deb, FakeCommandRunner::new()).capabilities();
        assert_eq!(deb.hold, HoldSupport::Supported);
        let rpm = backend(PackageKind::Rpm, FakeCommandRunner::new());
        assert_eq!(rpm.capabilities().hold, HoldSupport::Unsupported);
        assert!(rpm.hold(&pkg(&[]), &v("0.26.1")).unwrap().is_empty());
        assert_eq!(rpm.scope(), Scope::System);
    }

    fn factory(kind: PackageKind) -> GithubPackageFactory {
        GithubPackageFactory::new(
            kind,
            Arc::new(FakeCommandRunner::new()),
            Arc::new(FakeHttpFetcher::new()),
            PathBuf::from("/var/cache/bosun"),
            Os::Linux,
            Arch::Amd64,
        )
    }

    fn host<'a>(os: Os, runner: &'a FakeCommandRunner) -> HostInfo<'a> {
        HostInfo {
            os,
            arch: Arch::Amd64,
            distro: None,
            runner,
        }
    }

    #[test]
    fn the_factory_carries_the_hosts_libc_to_its_release_lookup() {
        assert_eq!(factory(PackageKind::Deb).libc, None);
        let musl = factory(PackageKind::Rpm).with_libc(Some(LibcFamily::Musl));
        assert_eq!(musl.libc, Some(LibcFamily::Musl));
    }

    #[test]
    fn each_registers_only_where_its_package_manager_answers() {
        let apt = FakeCommandRunner::new().with_stdout("apt 2.6.1 (amd64)");
        let probe = factory(PackageKind::Deb)
            .probe(&host(Os::Linux, &apt))
            .unwrap()
            .unwrap();
        assert_eq!(probe.backend.as_str(), "github-deb");
        assert_eq!(probe.version.as_str(), "2.6.1");
        assert_eq!(apt.calls()[0].program, "apt-get");

        let no_dnf = FakeCommandRunner::new().with_output(Output {
            code: Some(127),
            stdout: Vec::new(),
            stderr: Vec::new(),
        });
        assert!(factory(PackageKind::Rpm)
            .probe(&host(Os::Linux, &no_dnf))
            .unwrap()
            .is_none());
        assert_eq!(no_dnf.calls()[0].program, "dnf");
        let unused = FakeCommandRunner::new();
        assert!(factory(PackageKind::Deb)
            .probe(&host(Os::Macos, &unused))
            .unwrap()
            .is_none());
    }

    #[test]
    fn the_manager_version_is_its_first_numeric_word() {
        assert_eq!(manager_version("apt 2.6.1 (amd64)").as_str(), "2.6.1");
        assert_eq!(
            manager_version("4.18.0\n  Installed: dnf").as_str(),
            "4.18.0"
        );
        assert_eq!(manager_version("dnf5 version 5.2.6").as_str(), "5.2.6");
        assert_eq!(manager_version("unreadable").as_str(), "0");
    }

    #[test]
    fn meta_declares_the_github_identity_and_its_settings() {
        let f = factory(PackageKind::Rpm);
        assert_eq!(f.id().as_str(), "github-rpm");
        let meta = f.meta();
        assert_eq!((meta.purl_type, meta.vers_type), ("github", "semver"));
        assert_eq!(
            meta.option_keys,
            &["asset", "tag", "package_name", "unverified"]
        );
    }

    #[test]
    fn a_remote_index_reads_the_release_list_and_runs_nothing() {
        let runner = Arc::new(FakeCommandRunner::new());
        let list = "https://api.github.com/repos/o/r/releases?per_page=100&page=1";
        let fetcher = Arc::new(
            FakeHttpFetcher::new().with_body(list, r#"[{"tag_name":"v2.0.0","assets":[]}]"#),
        );
        let index = GithubPackageFactory::new(
            PackageKind::Deb,
            runner.clone(),
            fetcher,
            PathBuf::from("/c"),
            Os::Linux,
            Arch::Amd64,
        )
        .remote_index()
        .expect("a remote index");
        let pkg = Ref::new("pkg:github/o/r".parse().unwrap());
        let versions = index.query_available(&pkg, &Target::Any).unwrap();
        assert_eq!(
            versions,
            vec![Version::parse("2.0.0", Scheme::Semver).unwrap()]
        );
        assert!(runner.calls().is_empty());
    }
}
