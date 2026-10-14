//! The `Machine` implementations (§5.5): `LinuxMachine` for the real host and
//! `FakeMachine` for tests, so nothing above needs to touch the real machine.

use std::path::PathBuf;

use bosun_core::{Arch, Distro, Os, Scope};
use bosun_ports::{Elevation, Machine, Privilege, User};

use crate::error::HostError;
use crate::os_release;

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
pub struct LinuxMachine {
    arch: Arch,
    distro: Option<Distro>,
    system_state: PathBuf,
    user_state: PathBuf,
}

impl LinuxMachine {
    /// Construct from already-known facts (used by `detect`, and by callers
    /// that resolve the pieces themselves).
    pub fn new(
        arch: Arch,
        distro: Option<Distro>,
        system_state: PathBuf,
        user_state: PathBuf,
    ) -> LinuxMachine {
        LinuxMachine {
            arch,
            distro,
            system_state,
            user_state,
        }
    }

    /// Detect architecture and distro; take the per-scope state paths.
    pub fn detect(system_state: PathBuf, user_state: PathBuf) -> Result<LinuxMachine, HostError> {
        Ok(LinuxMachine::new(
            detect_arch()?,
            os_release::detect(),
            system_state,
            user_state,
        ))
    }
}

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

/// A fully-configurable [`Machine`] for tests.
#[derive(Clone, Debug)]
pub struct FakeMachine {
    /// Reported OS.
    pub os: Os,
    /// Reported architecture.
    pub arch: Arch,
    /// Reported distro facts.
    pub distro: Option<Distro>,
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
            ..Default::default()
        };
        assert_eq!(m.os(), Os::Linux);
        assert_eq!(m.arch(), Arch::Amd64);
        assert_eq!(m.privilege(), Privilege::Elevated);
        assert_eq!(m.distro().unwrap().id, "debian");
        assert_eq!(m.elevation(), Elevation::Sudo);
    }

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
            PathBuf::from("/var/lib/bosun/state.json"),
            PathBuf::from("/home/x/.local/state/bosun/state.json"),
        );
        assert_eq!(m.os(), Os::Linux);
        assert_eq!(m.arch(), Arch::Arm64);
        assert_eq!(m.distro().unwrap().id, "ubuntu");
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

    #[test]
    fn linux_machine_without_distro_reports_none() {
        let m = LinuxMachine::new(Arch::Amd64, None, PathBuf::from("/s"), PathBuf::from("/u"));
        assert!(m.distro().is_none());
    }

    #[test]
    fn fake_machine_state_path_is_per_scope() {
        let m = FakeMachine::default();
        assert_eq!(m.state_path(Scope::System), m.system_state);
        assert_eq!(m.state_path(Scope::User), m.user_state);
        assert_ne!(m.state_path(Scope::System), m.state_path(Scope::User));
    }
}
