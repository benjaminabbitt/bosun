//! The `Machine` implementations (§5.5): `LinuxMachine` and `WindowsMachine`
//! for the real host and `FakeMachine` for tests, so nothing above needs to
//! touch the real machine.

use std::path::PathBuf;

use bosun_core::{Arch, Distro, Libc, Os, Scope};
use bosun_ports::{Command, CommandRunner, Elevation, Machine, Privilege, User};

use crate::error::HostError;
#[cfg(unix)]
use crate::{libc, os_release};

/// The machine's architecture (§5.5), read from the machine rather than from
/// bosun's own build, which may run translated or emulated (an x86_64 bosun
/// under Rosetta or qemu on arm64): `uname -m` through `runner` on Linux and
/// macOS, the processor variables on Windows.
#[cfg(not(windows))]
pub fn detect_arch(runner: &dyn CommandRunner) -> Result<Arch, HostError> {
    uname_arch(runner)
}

/// The machine's architecture (§5.5), from the processor variables Windows
/// sets for every process.
#[cfg(windows)]
pub fn detect_arch(_runner: &dyn CommandRunner) -> Result<Arch, HostError> {
    let var = |name| std::env::var(name).ok();
    arch_from_windows(
        var("PROCESSOR_ARCHITEW6432").as_deref(),
        var("PROCESSOR_ARCHITECTURE").as_deref(),
    )
}

/// The machine's architecture by `uname -m`, the native `uname` program
/// answering for the machine (§5.5).
pub fn uname_arch(runner: &dyn CommandRunner) -> Result<Arch, HostError> {
    let unread = |detail: String| HostError::ArchUnread(format!("uname -m: {detail}"));
    let out = runner
        .run(&Command::new("uname", ["-m".to_string()]))
        .map_err(|e| unread(e.to_string()))?;
    if !out.success() {
        return Err(unread(String::from_utf8_lossy(&out.stderr).into_owned()));
    }
    arch_from_uname(&out.stdout_utf8())
}

/// The architecture `uname -m` printed: `x86_64` (`amd64` on the BSDs), or
/// `aarch64` (`arm64` on macOS).
pub fn arch_from_uname(stdout: &str) -> Result<Arch, HostError> {
    match stdout.trim() {
        "x86_64" | "amd64" => Ok(Arch::Amd64),
        "aarch64" | "arm64" => Ok(Arch::Arm64),
        other => Err(HostError::UnknownArch(other.to_string())),
    }
}

/// The architecture Windows' processor variables name: the native one in
/// `PROCESSOR_ARCHITEW6432`, set only for a 32-bit process under WOW64, else
/// the process's own in `PROCESSOR_ARCHITECTURE` (`AMD64`, `ARM64`, `x86`).
pub fn arch_from_windows(
    wow64_native: Option<&str>,
    process: Option<&str>,
) -> Result<Arch, HostError> {
    match wow64_native.or(process) {
        Some("AMD64") => Ok(Arch::Amd64),
        Some("ARM64") => Ok(Arch::Arm64),
        Some(other) => Err(HostError::UnknownArch(other.to_string())),
        None => Err(HostError::ArchUnread(
            "PROCESSOR_ARCHITECTURE is not set".to_string(),
        )),
    }
}

/// The real Linux host. State-file paths are injected (bosun-io resolves the
/// §4.6 locations), so the OS logic here stays free of path policy.
#[cfg(unix)]
pub struct LinuxMachine {
    arch: Arch,
    distro: Option<Distro>,
    libc: Option<Libc>,
    system_state: PathBuf,
    user_state: PathBuf,
    /// The user who elevated this run ([`LinuxMachine::invoked_by`]).
    invoker: Option<User>,
}

#[cfg(unix)]
impl LinuxMachine {
    /// Construct from already-known facts (used by `detect`, and by callers
    /// that resolve the pieces themselves).
    pub fn new(
        arch: Arch,
        distro: Option<Distro>,
        libc: Option<Libc>,
        system_state: PathBuf,
        user_state: PathBuf,
    ) -> LinuxMachine {
        LinuxMachine {
            arch,
            distro,
            libc,
            system_state,
            user_state,
            invoker: None,
        }
    }

    /// The machine of a run elevated by `user` (found by
    /// [`crate::invoker`], §5.5): it runs for them, so system claims are
    /// theirs, and its user state is theirs, at `user_state` (§4.6).
    pub fn invoked_by(self, user: User, user_state: PathBuf) -> LinuxMachine {
        LinuxMachine {
            user_state,
            invoker: Some(user),
            ..self
        }
    }

    /// Detect architecture, distro and libc (running the loader through
    /// `runner`); take the per-scope state paths.
    pub fn detect(
        runner: &dyn CommandRunner,
        system_state: PathBuf,
        user_state: PathBuf,
    ) -> Result<LinuxMachine, HostError> {
        Ok(LinuxMachine::new(
            detect_arch(runner)?,
            os_release::detect(),
            libc::detect(runner),
            system_state,
            user_state,
        ))
    }
}

#[cfg(unix)]
impl Machine for LinuxMachine {
    fn os(&self) -> Os {
        Os::Linux
    }

    fn arch(&self) -> Arch {
        self.arch
    }

    fn distro(&self) -> Option<&Distro> {
        self.distro.as_ref()
    }

    fn libc(&self) -> Option<&Libc> {
        self.libc.as_ref()
    }

    fn privilege(&self) -> Privilege {
        if rustix::process::geteuid().is_root() {
            Privilege::Elevated
        } else {
            Privilege::Unelevated
        }
    }

    fn invoking_user(&self) -> User {
        // Under sudo or doas the human is the user who elevated (§5.5).
        if let Some(user) = &self.invoker {
            return user.clone();
        }
        let uid = rustix::process::getuid().as_raw();
        let name = std::env::var("USER").unwrap_or_else(|_| uid.to_string());
        User {
            name,
            uid: Some(uid),
        }
    }

    fn state_path(&self, scope: Scope) -> PathBuf {
        match scope {
            Scope::System => self.system_state.clone(),
            Scope::User => self.user_state.clone(),
        }
    }

    fn elevation(&self) -> Elevation {
        Elevation::Sudo
    }
}

/// The integrity-level SIDs of an elevated process: High (an "as
/// administrator" terminal) and System.
const ELEVATED_LABELS: [&str; 2] = ["S-1-16-12288", "S-1-16-16384"];

/// Whether `whoami /groups` output shows an elevated process: its mandatory
/// label is High or System. Membership of Administrators is not enough: under
/// UAC an ordinary terminal holds that group deny-only, at Medium integrity.
pub fn privilege_from_whoami(stdout: &str) -> Privilege {
    if stdout
        .split_whitespace()
        .any(|word| ELEVATED_LABELS.contains(&word))
    {
        Privilege::Elevated
    } else {
        Privilege::Unelevated
    }
}

/// This process's privilege on Windows, by `whoami /groups` (no `unsafe`
/// token query needed). A `whoami` that can't run, or fails, reads as
/// unelevated: bosun then asks for an Administrator terminal rather than
/// assuming one.
pub fn windows_privilege(runner: &dyn CommandRunner) -> Privilege {
    match runner.run(&Command::new("whoami", ["/groups".to_string()])) {
        Ok(out) if out.success() => privilege_from_whoami(&out.stdout_utf8()),
        _ => Privilege::Unelevated,
    }
}

/// The real Windows host (§5.5). Privilege and the invoking user are read
/// once at startup and injected, as are the state paths, so the facts are
/// testable on any OS.
pub struct WindowsMachine {
    arch: Arch,
    privilege: Privilege,
    user: User,
    system_state: PathBuf,
    user_state: PathBuf,
}

impl WindowsMachine {
    /// Construct from already-known facts.
    pub fn new(
        arch: Arch,
        privilege: Privilege,
        user: User,
        system_state: PathBuf,
        user_state: PathBuf,
    ) -> WindowsMachine {
        WindowsMachine {
            arch,
            privilege,
            user,
            system_state,
            user_state,
        }
    }
}

impl Machine for WindowsMachine {
    fn os(&self) -> Os {
        Os::Windows
    }

    fn arch(&self) -> Arch {
        self.arch
    }

    fn distro(&self) -> Option<&Distro> {
        None
    }

    fn libc(&self) -> Option<&Libc> {
        None
    }

    fn privilege(&self) -> Privilege {
        self.privilege
    }

    fn invoking_user(&self) -> User {
        self.user.clone()
    }

    fn state_path(&self, scope: Scope) -> PathBuf {
        match scope {
            Scope::System => self.system_state.clone(),
            Scope::User => self.user_state.clone(),
        }
    }

    fn elevation(&self) -> Elevation {
        Elevation::AdministratorTerminal
    }
}

/// A fully-configurable [`Machine`] for tests.
#[derive(Clone, Debug)]
pub struct FakeMachine {
    /// Reported OS.
    pub os: Os,
    /// Reported architecture.
    pub arch: Arch,
    /// Reported distro facts.
    pub distro: Option<Distro>,
    /// Reported C library.
    pub libc: Option<Libc>,
    /// Reported privilege.
    pub privilege: Privilege,
    /// Reported invoking user.
    pub invoking_user: User,
    /// System-scope state path.
    pub system_state: PathBuf,
    /// User-scope state path.
    pub user_state: PathBuf,
    /// Reported elevation mechanism.
    pub elevation: Elevation,
}

impl Default for FakeMachine {
    fn default() -> FakeMachine {
        FakeMachine {
            os: Os::Linux,
            arch: Arch::Amd64,
            distro: None,
            libc: None,
            privilege: Privilege::Unelevated,
            invoking_user: User {
                name: "tester".to_string(),
                uid: Some(1000),
            },
            system_state: PathBuf::from("/var/lib/bosun/state.json"),
            user_state: PathBuf::from("/home/tester/.local/state/bosun/state.json"),
            elevation: Elevation::Sudo,
        }
    }
}

impl Machine for FakeMachine {
    fn os(&self) -> Os {
        self.os
    }
    fn arch(&self) -> Arch {
        self.arch
    }
    fn distro(&self) -> Option<&Distro> {
        self.distro.as_ref()
    }
    fn libc(&self) -> Option<&Libc> {
        self.libc.as_ref()
    }
    fn privilege(&self) -> Privilege {
        self.privilege
    }
    fn invoking_user(&self) -> User {
        self.invoking_user.clone()
    }
    fn state_path(&self, scope: Scope) -> PathBuf {
        match scope {
            Scope::System => self.system_state.clone(),
            Scope::User => self.user_state.clone(),
        }
    }
    fn elevation(&self) -> Elevation {
        self.elevation
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use bosun_ports::testing::FakeCommandRunner;
    use bosun_ports::Output;

    #[test]
    fn uname_names_map_to_bosuns_architectures() {
        // `uname -m` on Debian amd64 and arm64, macOS on Apple silicon.
        assert_eq!(arch_from_uname("x86_64\n").unwrap(), Arch::Amd64);
        assert_eq!(arch_from_uname("aarch64\n").unwrap(), Arch::Arm64);
        assert_eq!(arch_from_uname("arm64\n").unwrap(), Arch::Arm64);
        // 32-bit ARM and x86 are machines bosun has no target for.
        for other in ["armv7l", "i686"] {
            assert_eq!(
                arch_from_uname(&format!("{other}\n")),
                Err(HostError::UnknownArch(other.to_string()))
            );
        }
        assert_eq!(
            HostError::UnknownArch("armv7l".into()).to_string(),
            "unsupported architecture: armv7l"
        );
    }

    #[cfg(not(windows))]
    #[test]
    fn the_machine_arch_is_what_uname_says() {
        let runner = FakeCommandRunner::new().with_stdout("aarch64\n");
        assert_eq!(detect_arch(&runner).unwrap(), Arch::Arm64);
    }

    #[test]
    fn uname_m_is_asked_through_the_runner() {
        let runner = FakeCommandRunner::new().with_stdout("aarch64\n");
        assert_eq!(uname_arch(&runner).unwrap(), Arch::Arm64);
        let calls = runner.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].program, "uname");
        assert_eq!(calls[0].args, vec!["-m".to_string()]);
    }

    #[test]
    fn a_uname_that_fails_or_cannot_run_is_an_error() {
        let failing = FakeCommandRunner::new().with_output(Output {
            code: Some(1),
            stdout: Vec::new(),
            stderr: b"uname: extra operand".to_vec(),
        });
        assert!(matches!(
            uname_arch(&failing),
            Err(HostError::ArchUnread(_))
        ));
        let missing = FakeCommandRunner::new();
        match uname_arch(&missing) {
            Err(HostError::ArchUnread(detail)) => assert!(detail.contains("uname"), "{detail}"),
            other => panic!("expected an unread arch, got {other:?}"),
        }
        assert!(HostError::ArchUnread("x".into())
            .to_string()
            .starts_with("could not read the machine's architecture: "));
    }

    #[test]
    fn windows_names_its_processor_and_wow64_the_native_one() {
        // A 32-bit process on 64-bit Windows sees x86, and the native
        // architecture in PROCESSOR_ARCHITEW6432.
        assert_eq!(
            arch_from_windows(Some("AMD64"), Some("x86")).unwrap(),
            Arch::Amd64
        );
        assert_eq!(
            arch_from_windows(Some("ARM64"), Some("x86")).unwrap(),
            Arch::Arm64
        );
        assert_eq!(arch_from_windows(None, Some("AMD64")).unwrap(), Arch::Amd64);
        assert_eq!(arch_from_windows(None, Some("ARM64")).unwrap(), Arch::Arm64);
        assert_eq!(
            arch_from_windows(None, Some("x86")),
            Err(HostError::UnknownArch("x86".to_string()))
        );
        assert!(matches!(
            arch_from_windows(None, None),
            Err(HostError::ArchUnread(_))
        ));
    }

    #[test]
    fn fake_machine_reports_configured_facts() {
        let m = FakeMachine {
            privilege: Privilege::Elevated,
            distro: Some(Distro {
                id: "debian".into(),
                id_like: vec![],
                version_id: Some("12".into()),
                version_codename: Some("bookworm".into()),
                base_codename: None,
            }),
            libc: Some(Libc {
                family: bosun_core::LibcFamily::Musl,
                version: Some("1.2.5".into()),
            }),
            ..Default::default()
        };
        assert_eq!(m.os(), Os::Linux);
        assert_eq!(m.arch(), Arch::Amd64);
        assert_eq!(m.privilege(), Privilege::Elevated);
        assert_eq!(m.distro().unwrap().id, "debian");
        assert_eq!(m.libc().unwrap().to_string(), "musl 1.2.5");
        assert_eq!(m.elevation(), Elevation::Sudo);
    }

    #[cfg(unix)]
    #[test]
    fn linux_machine_reports_injected_facts() {
        let m = LinuxMachine::new(
            Arch::Arm64,
            Some(Distro {
                id: "ubuntu".into(),
                id_like: vec!["debian".into()],
                version_id: Some("24.04".into()),
                version_codename: Some("noble".into()),
                base_codename: None,
            }),
            Some(Libc {
                family: bosun_core::LibcFamily::Gnu,
                version: Some("2.39".into()),
            }),
            PathBuf::from("/var/lib/bosun/state.json"),
            PathBuf::from("/home/x/.local/state/bosun/state.json"),
        );
        assert_eq!(m.os(), Os::Linux);
        assert_eq!(m.arch(), Arch::Arm64);
        assert_eq!(m.distro().unwrap().id, "ubuntu");
        assert_eq!(m.libc().unwrap().to_string(), "gnu 2.39");
        assert_eq!(m.elevation(), Elevation::Sudo);
        assert_eq!(
            m.state_path(Scope::System),
            PathBuf::from("/var/lib/bosun/state.json")
        );
        assert_eq!(
            m.state_path(Scope::User),
            PathBuf::from("/home/x/.local/state/bosun/state.json")
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_linux_machine_invoked_by_a_user_runs_for_them_with_their_user_state() {
        let own = LinuxMachine::new(
            Arch::Amd64,
            None,
            None,
            PathBuf::from("/s"),
            PathBuf::from("/root/u"),
        );
        let me = own.invoking_user();
        assert_eq!(me.uid, Some(rustix::process::getuid().as_raw()));
        let ann = User {
            name: "ann".into(),
            uid: Some(1001),
        };
        let m = own.invoked_by(ann.clone(), PathBuf::from("/home/ann/u"));
        assert_eq!(m.invoking_user(), ann);
        assert_eq!(m.state_path(Scope::User), PathBuf::from("/home/ann/u"));
        assert_eq!(m.state_path(Scope::System), PathBuf::from("/s"));
    }

    #[cfg(unix)]
    #[test]
    fn linux_machine_without_distro_reports_none() {
        let m = LinuxMachine::new(
            Arch::Amd64,
            None,
            None,
            PathBuf::from("/s"),
            PathBuf::from("/u"),
        );
        assert!(m.distro().is_none());
        assert!(m.libc().is_none());
    }

    #[test]
    fn fake_machine_state_path_is_per_scope() {
        let m = FakeMachine::default();
        assert_eq!(m.state_path(Scope::System), m.system_state);
        assert_eq!(m.state_path(Scope::User), m.user_state);
        assert_ne!(m.state_path(Scope::System), m.state_path(Scope::User));
    }

    // --- Windows ---------------------------------------------------------------

    /// `whoami /groups` as an elevated (High Mandatory Level) process prints it.
    const WHOAMI_ELEVATED: &str = "\
GROUP INFORMATION
-----------------

Group Name                           Type             SID          Attributes
==================================== ================ ============ ==================================================
Everyone                             Well-known group S-1-1-0      Mandatory group, Enabled by default, Enabled group
BUILTIN\\Administrators               Alias            S-1-5-32-544 Mandatory group, Enabled by default, Enabled group, Group owner
Mandatory Label\\High Mandatory Level Label            S-1-16-12288
";

    /// The same administrator in an ordinary (Medium) terminal: the group is
    /// there but deny-only, and the label is Medium.
    const WHOAMI_UNELEVATED: &str = "\
Group Name                             Type             SID          Attributes
====================================== ================ ============ ==================================================
BUILTIN\\Administrators                 Alias            S-1-5-32-544 Group used for deny only
Mandatory Label\\Medium Mandatory Level Label            S-1-16-8192
";

    #[test]
    fn whoami_shows_elevation_by_the_high_or_system_integrity_label() {
        assert_eq!(privilege_from_whoami(WHOAMI_ELEVATED), Privilege::Elevated);
        assert_eq!(
            privilege_from_whoami(WHOAMI_UNELEVATED),
            Privilege::Unelevated
        );
        assert_eq!(
            privilege_from_whoami("Mandatory Label\\System Mandatory Level Label S-1-16-16384"),
            Privilege::Elevated
        );
        // S-1-16-12288 must be the whole SID, not a prefix of another.
        assert_eq!(
            privilege_from_whoami("x S-1-16-122880 y"),
            Privilege::Unelevated
        );
        assert_eq!(privilege_from_whoami(""), Privilege::Unelevated);
    }

    #[test]
    fn windows_privilege_asks_whoami_and_treats_a_failure_as_unelevated() {
        use bosun_ports::testing::FakeCommandRunner;
        let elevated = FakeCommandRunner::new().with_stdout(WHOAMI_ELEVATED);
        assert_eq!(windows_privilege(&elevated), Privilege::Elevated);
        assert_eq!(elevated.calls()[0].program, "whoami");
        assert_eq!(elevated.calls()[0].args, vec!["/groups".to_string()]);
        let broken = FakeCommandRunner::new();
        assert_eq!(windows_privilege(&broken), Privilege::Unelevated);
        // A failing whoami is not believed, whatever it printed.
        let failed = FakeCommandRunner::new().with_output(bosun_ports::Output {
            code: Some(1),
            stdout: WHOAMI_ELEVATED.as_bytes().to_vec(),
            stderr: Vec::new(),
        });
        assert_eq!(windows_privilege(&failed), Privilege::Unelevated);
    }

    #[test]
    fn windows_machine_reports_its_facts() {
        let m = WindowsMachine::new(
            Arch::Amd64,
            Privilege::Elevated,
            User {
                name: "ben".into(),
                uid: None,
            },
            PathBuf::from(r"C:\ProgramData\bosun\state.json"),
            PathBuf::from(r"C:\Users\ben\AppData\Local\bosun\state.json"),
        );
        assert_eq!(m.os(), Os::Windows);
        assert_eq!(m.arch(), Arch::Amd64);
        assert!(m.distro().is_none());
        assert!(m.libc().is_none());
        assert_eq!(m.privilege(), Privilege::Elevated);
        assert_eq!(m.invoking_user().name, "ben");
        assert_eq!(m.elevation(), Elevation::AdministratorTerminal);
        assert_eq!(
            m.state_path(Scope::System),
            PathBuf::from(r"C:\ProgramData\bosun\state.json")
        );
        assert_eq!(
            m.state_path(Scope::User),
            PathBuf::from(r"C:\Users\ben\AppData\Local\bosun\state.json")
        );
    }
}
