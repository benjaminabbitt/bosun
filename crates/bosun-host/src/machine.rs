//! The `Machine` implementations (§5.5): `LinuxMachine` and `WindowsMachine`
//! for the real host and `FakeMachine` for tests, so nothing above needs to
//! touch the real machine.

use std::path::PathBuf;

use bosun_core::{Arch, Distro, Libc, Os, Scope};
use bosun_ports::{Command, CommandRunner, Elevation, Machine, Privilege, User};

use crate::error::HostError;
#[cfg(unix)]
use crate::{libc, os_release};

/// Map the running target's architecture name to an [`Arch`] (§5.5).
pub fn detect_arch() -> Result<Arch, HostError> {
    arch_from_str(std::env::consts::ARCH)
}

fn arch_from_str(s: &str) -> Result<Arch, HostError> {
    match s {
        "x86_64" => Ok(Arch::Amd64),
        "aarch64" => Ok(Arch::Arm64),
        other => Err(HostError::UnknownArch(other.to_string())),
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
            detect_arch()?,
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
        // Under sudo the real uid is root's; the human is in SUDO_USER (§5.5).
        if let Ok(name) = std::env::var("SUDO_USER") {
            let uid = std::env::var("SUDO_UID").ok().and_then(|s| s.parse().ok());
            return User { name, uid };
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

    #[test]
    fn arch_maps_known_names_and_rejects_others() {
        assert_eq!(arch_from_str("x86_64").unwrap(), Arch::Amd64);
        assert_eq!(arch_from_str("aarch64").unwrap(), Arch::Arm64);
        assert!(matches!(
            arch_from_str("riscv64"),
            Err(HostError::UnknownArch(a)) if a == "riscv64"
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
