//! The github-installer backend (Windows): run the `.msi` or `.exe` installer
//! a project attaches to its GitHub release. The release is found the
//! github-release way ([`ReleaseLookup`]); the asset template is required.
//!
//! - `.msi`: `msiexec /i <file> /qn /norestart`; exit 3010 (done, reboot to
//!   finish) counts as success and is reported.
//! - `.exe`: the file itself with the manifest's `silent_args`. Installer
//!   frameworks disagree on silent flags (NSIS `/S`, Inno Setup
//!   `/VERYSILENT /SUPPRESSMSGBOXES`, …), so bosun never guesses: an `.exe`
//!   without `silent_args` is an error.
//!
//! An installed program is found in the Uninstall registry keys (machine, its
//! 32-bit view, and the user's, read with `reg query`) by its `display_name`,
//! which may end in `*` to match a name that carries its version
//! (`7-Zip*`). Presence is all that is reported, as for the other release
//! backends: the registry's `DisplayVersion` is the installer's own spelling.
//! Uninstall is `msiexec /x <ProductCode>` for an MSI, else the entry's
//! `QuietUninstallString`, else its `UninstallString` with `uninstall_args`.
//!
//! System scope (§4.6): installers write machine-wide and need an
//! Administrator terminal. bosun can't hold such a program (many update
//! themselves) or downgrade it. Unverified on a real Windows host (§11).

use std::path::PathBuf;
use std::sync::Arc;

use bosun_core::{
    Arch, BackendId, Capabilities, Checksum, HoldSupport, Installed, Os, Ref, Scheme, Scope,
    Source, Target, Version,
};
use bosun_ports::{
    Backend, BackendFactory, BackendMeta, Command, CommandRunner, HostInfo, HttpFetcher, Index,
    Installer, OkExit, PortError, Probe, Step, Steps, UserInstalls,
};

use crate::errmsg;
use crate::github_release::{ReleaseFile, ReleaseLookup};

const BACKEND: &str = "github-installer";

/// The Uninstall keys searched, machine-wide (64- and 32-bit views) first.
const UNINSTALL_ROOTS: [&str; 3] = [
    r"HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall",
    r"HKLM\SOFTWARE\WOW6432Node\Microsoft\Windows\CurrentVersion\Uninstall",
    r"HKCU\Software\Microsoft\Windows\CurrentVersion\Uninstall",
];

/// Builds github-installer on Windows hosts.
pub struct GithubInstallerFactory {
    runner: Arc<dyn CommandRunner>,
    fetcher: Arc<dyn HttpFetcher>,
    cache_dir: PathBuf,
    os: Os,
    arch: Arch,
}

impl GithubInstallerFactory {
    /// Construct with the runner (msiexec, installers, `reg query`), the HTTP
    /// fetcher for releases, the machine-wide download cache
    /// (`%ProgramData%\bosun\cache`) and the host os/arch.
    pub fn new(
        runner: Arc<dyn CommandRunner>,
        fetcher: Arc<dyn HttpFetcher>,
        cache_dir: PathBuf,
        os: Os,
        arch: Arch,
    ) -> GithubInstallerFactory {
        GithubInstallerFactory {
            runner,
            fetcher,
            cache_dir,
            os,
            arch,
        }
    }
}

impl BackendFactory for GithubInstallerFactory {
    fn id(&self) -> BackendId {
        BackendId::new(BACKEND)
    }

    fn meta(&self) -> BackendMeta {
        BackendMeta {
            purl_type: "github",
            vers_type: "semver",
            platform_independent: false,
            option_keys: &[
                "asset",
                "tag",
                "display_name",
                "silent_args",
                "uninstall_args",
                "unverified",
            ],
        }
    }

    fn probe(&self, host: &HostInfo) -> Result<Option<Probe>, PortError> {
        // msiexec and the registry are part of Windows.
        if host.os != Os::Windows {
            return Ok(None);
        }
        Ok(Some(Probe {
            backend: self.id(),
            version: Version::parse("1.0.0", Scheme::Semver).expect("a version"),
        }))
    }

    fn build(&self, _probe: Probe) -> Backend {
        let backend = GithubInstallerBackend {
            runner: self.runner.clone(),
            releases: ReleaseLookup::new(self.fetcher.clone(), self.os, self.arch),
            cache_dir: self.cache_dir.clone(),
        };
        Backend {
            index: Box::new(backend.clone()),
            installer: Box::new(backend),
        }
    }
}

#[derive(Clone)]
struct GithubInstallerBackend {
    runner: Arc<dyn CommandRunner>,
    releases: ReleaseLookup,
    cache_dir: PathBuf,
}

/// An installed program's Uninstall entry.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct UninstallEntry {
    /// The key's last component: an MSI's ProductCode (`{…}`), or a name.
    key: String,
    display_name: String,
    msi: bool,
    uninstall: Option<String>,
    quiet_uninstall: Option<String>,
}

/// The entries in `reg query <root> /s` output: a key line (`HKEY_…\…\<key>`)
/// then its values, four-space indented, `name    TYPE    data`.
fn uninstall_entries(stdout: &str) -> Vec<UninstallEntry> {
    let mut entries: Vec<UninstallEntry> = Vec::new();
    for line in stdout.lines() {
        if line.starts_with("HKEY_") {
            let key = line.rsplit('\\').next().unwrap_or_default().to_string();
            entries.push(UninstallEntry {
                key,
                ..UninstallEntry::default()
            });
            continue;
        }
        let Some(entry) = entries.last_mut() else {
            continue;
        };
        let mut parts = line.trim_start().splitn(3, "    ");
        let (Some(name), Some(_kind), Some(data)) = (parts.next(), parts.next(), parts.next())
        else {
            continue;
        };
        let data = data.trim().to_string();
        match name {
            "DisplayName" => entry.display_name = data,
            "WindowsInstaller" => entry.msi = data == "0x1",
            "UninstallString" => entry.uninstall = Some(data),
            "QuietUninstallString" => entry.quiet_uninstall = Some(data),
            _ => {}
        }
    }
    entries
}

/// Whether `name` is the program `wanted` names: equal, or starting with
/// `wanted`'s text before a trailing `*`.
fn names_match(wanted: &str, name: &str) -> bool {
    match wanted.strip_suffix('*') {
        Some(prefix) => name.starts_with(prefix),
        None => name == wanted,
    }
}

/// A Windows command line as argv: a leading quoted program path, then
/// space-separated arguments (quotes around an argument are dropped).
fn split_command_line(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut word = String::new();
    let mut quoted = false;
    for c in line.chars() {
        match c {
            '"' => quoted = !quoted,
            ' ' if !quoted => {
                if !word.is_empty() {
                    out.push(std::mem::take(&mut word));
                }
            }
            c => word.push(c),
        }
    }
    if !word.is_empty() {
        out.push(word);
    }
    out
}

fn words(s: &str) -> Vec<String> {
    s.split_whitespace().map(str::to_string).collect()
}

fn run(argv: Vec<String>) -> Step {
    Step::Run {
        argv,
        env: vec![],
        scope: Scope::System,
        ok_exits: Vec::new(),
    }
}

/// msiexec's ERROR_SUCCESS_REBOOT_REQUIRED: the install or uninstall worked,
/// and Windows finishes it at the next reboot (`/norestart` never reboots).
const MSI_REBOOT_REQUIRED: OkExit = OkExit {
    code: 3010,
    meaning: "done; Windows needs a reboot to finish",
};

/// An msiexec step, which counts [`MSI_REBOOT_REQUIRED`] as success.
fn msiexec(argv: Vec<String>) -> Step {
    Step::Run {
        argv,
        env: vec![],
        scope: Scope::System,
        ok_exits: vec![MSI_REBOOT_REQUIRED],
    }
}

impl GithubInstallerBackend {
    fn setting(pkg: &Ref, key: &str) -> Option<String> {
        pkg.purl.qualifiers().get(key).cloned()
    }

    fn need(pkg: &Ref, key: &str) -> Result<String, PortError> {
        Self::setting(pkg, key).ok_or_else(|| PortError::Backend {
            detail: format!(
                "{}: {} backends.{BACKEND}.{key}",
                pkg.purl.native_id(),
                errmsg::INSTALLER_NEEDS
            ),
        })
    }

    /// The installer at `version`; an `.exe` must come with `silent_args`, and
    /// every package with a `display_name`, before anything downloads.
    fn file(&self, pkg: &Ref, version: &Version) -> Result<ReleaseFile, PortError> {
        Self::need(pkg, "display_name")?;
        let file = self.releases.file(
            pkg,
            version,
            |name| {
                let lower = name.to_ascii_lowercase();
                lower.ends_with(".msi") || lower.ends_with(".exe")
            },
            errmsg::NOT_AN_INSTALLER,
        )?;
        if file.name.to_ascii_lowercase().ends_with(".exe") {
            Self::need(pkg, "silent_args")?;
        }
        // It runs as Administrator: an unverifiable installer needs consent.
        ReleaseLookup::require_verifiable(pkg, &file, BACKEND)?;
        Ok(file)
    }

    /// The installed program's entry, from the first Uninstall key that has it.
    fn entry(&self, pkg: &Ref) -> Result<Option<UninstallEntry>, PortError> {
        let wanted = Self::need(pkg, "display_name")?;
        for root in UNINSTALL_ROOTS {
            let out = self.runner.run(&Command::new(
                "reg",
                ["query".to_string(), root.to_string(), "/s".to_string()],
            ))?;
            if !out.success() {
                continue; // a view that doesn't exist on this machine
            }
            if let Some(e) = uninstall_entries(&out.stdout_utf8())
                .into_iter()
                .find(|e| names_match(&wanted, &e.display_name))
            {
                return Ok(Some(e));
            }
        }
        Ok(None)
    }
}

impl Index for GithubInstallerBackend {
    fn refresh(&self) -> Result<(), PortError> {
        Ok(())
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

impl Installer for GithubInstallerBackend {
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            supports_exact_version: true,
            hold: HoldSupport::Unsupported,
            supports_downgrade: false,
        }
    }

    fn scope(&self) -> Scope {
        Scope::System
    }

    fn ensure_source(&self, _source: &Source) -> Result<Steps, PortError> {
        Ok(vec![])
    }

    fn install_exact(&self, pkg: &Ref, version: &Version) -> Result<Steps, PortError> {
        let file = self.file(pkg, version)?;
        let dest = self
            .cache_dir
            .join(BACKEND)
            .join(pkg.purl.native_id().replace('/', "_"))
            .join(&file.name);
        let path = dest.display().to_string();
        let install = if file.name.to_ascii_lowercase().ends_with(".msi") {
            msiexec(vec![
                "msiexec".to_string(),
                "/i".to_string(),
                path,
                "/qn".to_string(),
                "/norestart".to_string(),
            ])
        } else {
            let mut argv = vec![path];
            argv.extend(words(&Self::need(pkg, "silent_args")?));
            run(argv)
        };
        Ok(vec![
            Step::Fetch {
                url: file.url,
                sha256: file.sha256,
                dest,
            },
            install,
        ])
    }

    fn verify_installed(&self, pkg: &Ref) -> Result<Installed, PortError> {
        Ok(match self.entry(pkg)? {
            Some(_) => Installed::Present { version: None },
            None => Installed::Absent,
        })
    }

    fn hold(&self, _pkg: &Ref, _version: &Version) -> Result<Steps, PortError> {
        Ok(vec![])
    }

    fn downgrade(&self, pkg: &Ref, version: &Version) -> Result<Steps, PortError> {
        Err(PortError::Backend {
            detail: format!(
                "{}: an installer can't be downgraded to {}; uninstall it first",
                pkg.purl.native_id(),
                version.as_str()
            ),
        })
    }

    fn uninstall(&self, pkg: &Ref) -> Result<Steps, PortError> {
        let entry = self.entry(pkg)?.ok_or_else(|| PortError::Backend {
            detail: format!(
                "{}: no installed program named {:?}",
                pkg.purl.native_id(),
                Self::setting(pkg, "display_name").unwrap_or_default()
            ),
        })?;
        if entry.msi || (entry.key.starts_with('{') && entry.key.ends_with('}')) {
            return Ok(vec![msiexec(vec![
                "msiexec".to_string(),
                "/x".to_string(),
                entry.key,
                "/qn".to_string(),
                "/norestart".to_string(),
            ])]);
        }
        let argv = match (&entry.quiet_uninstall, &entry.uninstall) {
            (Some(quiet), _) => split_command_line(quiet),
            (None, Some(plain)) => {
                let mut argv = split_command_line(plain);
                argv.extend(words(
                    &Self::setting(pkg, "uninstall_args").unwrap_or_default(),
                ));
                argv
            }
            (None, None) => {
                return Err(PortError::Backend {
                    detail: format!("{}: the program has no uninstaller", entry.display_name),
                })
            }
        };
        Ok(vec![run(argv)])
    }

    fn user_installs(&self) -> Result<UserInstalls, PortError> {
        // The registry lists every program, however it came; only bosun
        // knows which release each of these came from.
        Ok(UserInstalls::BosunState)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use bosun_ports::testing::{FakeCommandRunner, FakeHttpFetcher};
    use bosun_ports::{OkExit, Output};
    use sha2::Digest;

    const TAG_URL: &str = "https://api.github.com/repos/o/app/releases/tags/v2.0.0";
    const RELEASE: &str = r#"{"tag_name":"v2.0.0","assets":[
        {"name":"app-2.0.0-x64.msi","browser_download_url":"https://x/app.msi"},
        {"name":"app-setup-2.0.0.exe","browser_download_url":"https://x/app.exe"},
        {"name":"app-2.0.0.zip","browser_download_url":"https://x/app.zip"}]}"#;

    const REG_MACHINE: &str = r#"
HKEY_LOCAL_MACHINE\SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall\{0D2C5D3B-1111-2222-3333-444455556666}
    DisplayName    REG_SZ    App 2.0
    DisplayVersion    REG_SZ    2.0.0.0
    UninstallString    REG_EXPAND_SZ    MsiExec.exe /X{0D2C5D3B-1111-2222-3333-444455556666}
    WindowsInstaller    REG_DWORD    0x1

HKEY_LOCAL_MACHINE\SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall\Tool_is1
    DisplayName    REG_SZ    Tool version 3.1
    QuietUninstallString    REG_SZ    "C:\Program Files\Tool\unins000.exe" /SILENT
    UninstallString    REG_SZ    "C:\Program Files\Tool\unins000.exe"

HKEY_LOCAL_MACHINE\SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall\Nsis
    DisplayName    REG_SZ    Nsis App
    UninstallString    REG_SZ    "C:\Program Files\Nsis App\uninstall.exe"
"#;

    /// app with the given options. The fixture release publishes no checksum,
    /// so it also allows an unverified install; see [`strict_pkg`].
    fn pkg(pairs: &[(&str, &str)]) -> Ref {
        let mut all = vec![("unverified", "allow")];
        all.extend_from_slice(pairs);
        strict_pkg(&all)
    }

    /// app with exactly the given options.
    fn strict_pkg(pairs: &[(&str, &str)]) -> Ref {
        let mut q = BTreeMap::from([("binary".to_string(), "app".to_string())]);
        for (k, v) in pairs {
            q.insert(k.to_string(), v.to_string());
        }
        Ref::new(
            bosun_core::Purl::from_components("github", Some("o"), "app", None, q, None).unwrap(),
        )
    }

    fn backend(runner: Arc<FakeCommandRunner>) -> GithubInstallerBackend {
        let fetcher = Arc::new(
            FakeHttpFetcher::new()
                .with_body(TAG_URL, RELEASE)
                .with_body("https://x/app.msi", "the .msi"),
        );
        GithubInstallerBackend {
            runner,
            releases: ReleaseLookup::new(fetcher, Os::Windows, Arch::Amd64),
            cache_dir: PathBuf::from(r"C:\ProgramData\bosun\cache"),
        }
    }

    fn quiet() -> Arc<FakeCommandRunner> {
        Arc::new(FakeCommandRunner::new())
    }

    /// `reg query` answering `first` for the machine key and nothing (absent
    /// views) for the other two.
    fn registry(first: &str) -> Arc<FakeCommandRunner> {
        let absent = || Output {
            code: Some(1),
            stdout: Vec::new(),
            stderr: b"ERROR: The system was unable to find the specified registry key".to_vec(),
        };
        Arc::new(
            FakeCommandRunner::new()
                .with_stdout(first)
                .with_output(absent())
                .with_output(absent()),
        )
    }

    fn v(s: &str) -> Version {
        Version::parse(s, Scheme::Semver).unwrap()
    }

    fn strs(parts: &[&str]) -> Vec<String> {
        let mut out = Vec::new();
        for p in parts {
            out.push(String::from(*p));
        }
        out
    }

    fn system(argv: &[&str]) -> Step {
        Step::Run {
            argv: strs(argv),
            env: vec![],
            scope: Scope::System,
            ok_exits: Vec::new(),
        }
    }

    /// An msiexec step: 3010 (installed, reboot to finish) is success too.
    fn msiexec(argv: &[&str]) -> Step {
        Step::Run {
            argv: strs(argv),
            env: vec![],
            scope: Scope::System,
            ok_exits: vec![OkExit {
                code: 3010,
                meaning: "done; Windows needs a reboot to finish",
            }],
        }
    }

    #[test]
    fn what_the_user_installed_is_what_bosun_recorded() {
        assert_eq!(
            backend(quiet()).user_installs().unwrap(),
            UserInstalls::BosunState
        );
    }

    #[test]
    fn an_msi_is_installed_quietly_with_msiexec() {
        let b = backend(quiet());
        let p = pkg(&[
            ("asset", "{{ binary }}-{{ version }}-x64.msi"),
            ("display_name", "App*"),
        ]);
        let steps = b.install_exact(&p, &v("2.0.0")).unwrap();
        let path = PathBuf::from(r"C:\ProgramData\bosun\cache")
            .join("github-installer")
            .join("o_app")
            .join("app-2.0.0-x64.msi");
        assert_eq!(
            steps,
            vec![
                Step::Fetch {
                    url: "https://x/app.msi".into(),
                    sha256: None,
                    dest: path.clone(),
                },
                msiexec(&[
                    "msiexec",
                    "/i",
                    &path.display().to_string(),
                    "/qn",
                    "/norestart"
                ]),
            ]
        );
    }

    #[test]
    fn an_exe_runs_with_the_manifests_silent_args_and_never_without() {
        let b = backend(quiet());
        let p = pkg(&[
            ("asset", "{{ binary }}-setup-{{ version }}.exe"),
            ("display_name", "App*"),
            ("silent_args", "/VERYSILENT /SUPPRESSMSGBOXES"),
        ]);
        let steps = b.install_exact(&p, &v("2.0.0")).unwrap();
        let Step::Run { argv, .. } = &steps[1] else {
            panic!("{steps:?}")
        };
        assert!(argv[0].ends_with("app-setup-2.0.0.exe"), "{argv:?}");
        assert_eq!(argv[1..], strs(&["/VERYSILENT", "/SUPPRESSMSGBOXES"]));

        let unsilent = pkg(&[
            ("asset", "{{ binary }}-setup-{{ version }}.exe"),
            ("display_name", "App*"),
        ]);
        let err = b.install_exact(&unsilent, &v("2.0.0")).unwrap_err();
        assert!(matches!(err, PortError::Backend { detail }
            if detail.contains(errmsg::INSTALLER_NEEDS) && detail.contains("silent_args")));
    }

    #[test]
    fn a_display_name_is_required_and_the_asset_must_be_an_installer() {
        let b = backend(quiet());
        let err = b
            .checksum(
                &pkg(&[("asset", "{{ binary }}-{{ version }}-x64.msi")]),
                &v("2.0.0"),
                &"windows/amd64".parse().unwrap(),
            )
            .unwrap_err();
        assert!(matches!(err, PortError::Backend { detail } if detail.contains("display_name")));
        let err = b
            .checksum(
                &pkg(&[
                    ("asset", "{{ binary }}-{{ version }}.zip"),
                    ("display_name", "App"),
                ]),
                &v("2.0.0"),
                &"windows/amd64".parse().unwrap(),
            )
            .unwrap_err();
        assert!(
            matches!(err, PortError::Backend { detail } if detail.contains(errmsg::NOT_AN_INSTALLER))
        );
    }

    #[test]
    fn registry_entries_are_read_from_reg_query_output() {
        let entries = uninstall_entries(REG_MACHINE);
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].key, "{0D2C5D3B-1111-2222-3333-444455556666}");
        assert_eq!(entries[0].display_name, "App 2.0");
        assert!(entries[0].msi);
        assert_eq!(
            entries[1].quiet_uninstall.as_deref(),
            Some(r#""C:\Program Files\Tool\unins000.exe" /SILENT"#)
        );
        assert!(!entries[1].msi);
        assert_eq!(entries[2].quiet_uninstall, None);
        assert!(uninstall_entries("garbage\n    no key yet    REG_SZ    x").is_empty());
    }

    #[test]
    fn a_display_name_matches_exactly_or_by_its_prefix_before_a_star() {
        assert!(names_match("App 2.0", "App 2.0"));
        assert!(!names_match("App", "App 2.0"));
        assert!(names_match("App*", "App 2.0"));
        assert!(!names_match("Ap*x", "App"));
        assert!(!names_match("Tool*", "App 2.0"));
    }

    #[test]
    fn presence_is_an_uninstall_entry_in_any_view() {
        let p = pkg(&[("display_name", "Tool version*")]);
        assert_eq!(
            backend(registry(REG_MACHINE)).verify_installed(&p).unwrap(),
            Installed::Present { version: None }
        );
        let none = pkg(&[("display_name", "Absent App")]);
        assert_eq!(
            backend(registry(REG_MACHINE))
                .verify_installed(&none)
                .unwrap(),
            Installed::Absent
        );
        // Found in the user's view after two empty machine views.
        let user_only = Arc::new(
            FakeCommandRunner::new()
                .with_stdout("")
                .with_stdout("")
                .with_stdout(REG_MACHINE),
        );
        assert_eq!(
            backend(user_only.clone()).verify_installed(&p).unwrap(),
            Installed::Present { version: None }
        );
        let calls = user_only.calls();
        assert_eq!(calls.len(), 3);
        assert_eq!(calls[2].program, "reg");
        assert_eq!(calls[2].args, strs(&["query", UNINSTALL_ROOTS[2], "/s"]));
    }

    #[test]
    fn an_msi_uninstalls_by_its_product_code() {
        let steps = backend(registry(REG_MACHINE))
            .uninstall(&pkg(&[("display_name", "App*")]))
            .unwrap();
        assert_eq!(
            steps,
            vec![msiexec(&[
                "msiexec",
                "/x",
                "{0D2C5D3B-1111-2222-3333-444455556666}",
                "/qn",
                "/norestart"
            ])]
        );
    }

    #[test]
    fn an_msi_is_known_by_its_flag_or_by_a_product_code_key() {
        // A GUID key with no WindowsInstaller value, and a flagged key named
        // otherwise, both uninstall through msiexec.
        let guid_only = "HKEY_LOCAL_MACHINE\\X\\{AAAA-BBBB}\n    DisplayName    REG_SZ    G\n    UninstallString    REG_SZ    x.exe\n";
        assert_eq!(
            backend(registry(guid_only))
                .uninstall(&pkg(&[("display_name", "G")]))
                .unwrap(),
            vec![msiexec(&[
                "msiexec",
                "/x",
                "{AAAA-BBBB}",
                "/qn",
                "/norestart"
            ])]
        );
        let flagged = "HKEY_LOCAL_MACHINE\\X\\Named\n    DisplayName    REG_SZ    F\n    WindowsInstaller    REG_DWORD    0x1\n";
        assert_eq!(
            backend(registry(flagged))
                .uninstall(&pkg(&[("display_name", "F")]))
                .unwrap(),
            vec![msiexec(&["msiexec", "/x", "Named", "/qn", "/norestart"])]
        );
        // A key that only starts with a brace is not a product code.
        let half = "HKEY_LOCAL_MACHINE\\X\\{half\n    DisplayName    REG_SZ    H\n    UninstallString    REG_SZ    u.exe\n";
        assert_eq!(
            backend(registry(half))
                .uninstall(&pkg(&[("display_name", "H")]))
                .unwrap(),
            vec![system(&["u.exe"])]
        );
    }

    #[test]
    fn versions_are_the_releases_tags() {
        let list = "https://api.github.com/repos/o/app/releases?per_page=100&page=1";
        let fetcher = Arc::new(FakeHttpFetcher::new().with_body(
            list,
            r#"[{"tag_name":"v2.0.0","assets":[]},{"tag_name":"v1.0.0","assets":[]}]"#,
        ));
        let mut b = backend(quiet());
        b.releases = ReleaseLookup::new(fetcher, Os::Windows, Arch::Amd64);
        let versions = b
            .query_available(&pkg(&[]), &"windows/amd64".parse().unwrap())
            .unwrap();
        assert_eq!(versions, vec![v("2.0.0"), v("1.0.0")]);
    }

    #[test]
    fn an_exe_uninstalls_quietly_or_with_the_manifests_args() {
        let quiet_steps = backend(registry(REG_MACHINE))
            .uninstall(&pkg(&[("display_name", "Tool version*")]))
            .unwrap();
        assert_eq!(
            quiet_steps,
            vec![system(&[r"C:\Program Files\Tool\unins000.exe", "/SILENT"])]
        );
        let nsis = backend(registry(REG_MACHINE))
            .uninstall(&pkg(&[
                ("display_name", "Nsis App"),
                ("uninstall_args", "/S"),
            ]))
            .unwrap();
        assert_eq!(
            nsis,
            vec![system(&[r"C:\Program Files\Nsis App\uninstall.exe", "/S"])]
        );
        let err = backend(registry(REG_MACHINE))
            .uninstall(&pkg(&[("display_name", "Absent App")]))
            .unwrap_err();
        assert!(matches!(err, PortError::Backend { detail } if detail.contains("Absent App")));
    }

    #[test]
    fn a_program_without_any_uninstaller_is_an_error() {
        let reg = "HKEY_LOCAL_MACHINE\\X\\Plain\n    DisplayName    REG_SZ    Plain\n";
        let err = backend(registry(reg))
            .uninstall(&pkg(&[("display_name", "Plain")]))
            .unwrap_err();
        assert!(matches!(err, PortError::Backend { detail } if detail.contains("no uninstaller")));
    }

    #[test]
    fn command_lines_split_with_a_quoted_program() {
        assert_eq!(
            split_command_line(r#""C:\Program Files\A\u.exe" /S  /D=x"#),
            strs(&[r"C:\Program Files\A\u.exe", "/S", "/D=x"])
        );
        assert_eq!(split_command_line("u.exe"), strs(&["u.exe"]));
    }

    #[test]
    fn machine_scope_no_hold_no_downgrade_and_windows_only() {
        let b = backend(quiet());
        assert_eq!(b.scope(), Scope::System);
        let c = b.capabilities();
        assert_eq!(c.hold, HoldSupport::Unsupported);
        assert!(!c.supports_downgrade);
        assert!(b.hold(&pkg(&[]), &v("2.0.0")).unwrap().is_empty());
        assert!(b.downgrade(&pkg(&[]), &v("1.0.0")).is_err());
        let f = GithubInstallerFactory::new(
            quiet(),
            Arc::new(FakeHttpFetcher::new()),
            PathBuf::new(),
            Os::Windows,
            Arch::Amd64,
        );
        let probe = |os| {
            f.probe(&HostInfo {
                os,
                arch: Arch::Amd64,
                distro: None,
                runner: &FakeCommandRunner::new(),
            })
            .unwrap()
        };
        assert!(probe(Os::Windows).is_some());
        assert!(probe(Os::Linux).is_none());
        assert_eq!(f.id().as_str(), "github-installer");
        assert!(f.meta().option_keys.contains(&"silent_args"));
        assert!(f.meta().option_keys.contains(&"unverified"));
    }

    const MSI: &str = "{{ binary }}-{{ version }}-x64.msi";

    #[test]
    fn an_installer_without_a_checksum_is_refused_unless_the_entry_allows_it() {
        let b = backend(quiet());
        let strict = strict_pkg(&[("asset", MSI), ("display_name", "App*")]);
        let at = "windows/amd64".parse().unwrap();
        for err in [
            b.install_exact(&strict, &v("2.0.0")).unwrap_err(),
            b.checksum(&strict, &v("2.0.0"), &at).unwrap_err(),
        ] {
            let text = err.to_string();
            assert!(
                text.contains("backends.github-installer.unverified: allow"),
                "{text}"
            );
        }
    }

    #[test]
    fn an_allowed_unverified_installer_locks_the_sha256_it_downloads_as() {
        let b = backend(quiet());
        let got = b
            .checksum(
                &pkg(&[("asset", MSI), ("display_name", "App*")]),
                &v("2.0.0"),
                &"windows/amd64".parse().unwrap(),
            )
            .unwrap();
        let want: String = sha2::Sha256::digest(b"the .msi")
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        assert_eq!(got, Some(Checksum(want)));
    }
}
