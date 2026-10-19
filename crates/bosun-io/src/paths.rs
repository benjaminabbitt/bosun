//! Default file locations (§4.4, §4.6). XDG on Linux/macOS, with the system
//! state file beside the OS package managers' own state; on Windows the known
//! folders: `%ProgramData%` (system state), `%LOCALAPPDATA%` (user state),
//! `%APPDATA%` (config).

use std::path::{Path, PathBuf};

use bosun_core::Scope;
use etcetera::base_strategy::{BaseStrategy, Xdg};
use sha2::{Digest, Sha256};

use crate::error::IoError;

fn xdg() -> Result<Xdg, IoError> {
    Xdg::new().map_err(|e| IoError::Convert {
        detail: format!("no home directory: {e}"),
    })
}

/// An environment variable, unset when empty.
pub fn env_var(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

/// `%<var>%\bosun\<file>`, reading the folder variable through `env`.
fn known_folder(
    var: &str,
    file: &str,
    env: impl Fn(&str) -> Option<String>,
) -> Result<PathBuf, IoError> {
    let base = env(var).ok_or_else(|| IoError::Convert {
        detail: format!("%{var}% is not set"),
    })?;
    Ok(PathBuf::from(base).join("bosun").join(file))
}

/// The Windows machine-state file for `scope` (§4.6): system state under
/// `%ProgramData%` (shared, admin-owned), user state under `%LOCALAPPDATA%`.
pub fn windows_state_path(
    scope: Scope,
    env: impl Fn(&str) -> Option<String>,
) -> Result<PathBuf, IoError> {
    match scope {
        Scope::System => known_folder("ProgramData", "state.json", env),
        Scope::User => known_folder("LOCALAPPDATA", "state.json", env),
    }
}

/// The Windows user config file, `%APPDATA%\bosun\config.yaml` (§4.4).
pub fn windows_config_path(env: impl Fn(&str) -> Option<String>) -> Result<PathBuf, IoError> {
    known_folder("APPDATA", "config.yaml", env)
}

/// The machine-state file for `scope` (§4.6): system state beside the OS
/// package managers (`/var/lib/bosun`), user state under `$XDG_STATE_HOME`;
/// the known folders on Windows ([`windows_state_path`]).
pub fn state_path(scope: Scope) -> Result<PathBuf, IoError> {
    if cfg!(windows) {
        return windows_state_path(scope, env_var);
    }
    match scope {
        Scope::System => Ok(PathBuf::from("/var/lib/bosun/state.json")),
        Scope::User => {
            let x = xdg()?;
            let state = x.state_dir().unwrap_or_else(|| x.data_dir());
            Ok(state.join("bosun").join("state.json"))
        }
    }
}

/// The user config file, `<config dir>/bosun/config.yaml` (§4.4).
pub fn config_path() -> Result<PathBuf, IoError> {
    if cfg!(windows) {
        return windows_config_path(env_var);
    }
    Ok(xdg()?.config_dir().join("bosun").join("config.yaml"))
}

/// The Windows cache directory, `%LOCALAPPDATA%\bosun\cache`.
pub fn windows_cache_dir(env: impl Fn(&str) -> Option<String>) -> Result<PathBuf, IoError> {
    known_folder("LOCALAPPDATA", "cache", env)
}

/// Where bosun caches what it fetched (§4.7: remote package indexes):
/// `$XDG_CACHE_HOME/bosun` (default `~/.cache/bosun`), `%LOCALAPPDATA%\bosun\cache`
/// on Windows ([`windows_cache_dir`]).
pub fn cache_dir() -> Result<PathBuf, IoError> {
    if cfg!(windows) {
        return windows_cache_dir(env_var);
    }
    Ok(xdg()?.cache_dir().join("bosun"))
}

/// Where a user's locks live (§4.5): `locks/` in the directory holding their
/// machine state file. A lock is the machine's own, so it is never in the
/// project.
pub fn locks_beside(state_file: &Path) -> PathBuf {
    state_file
        .parent()
        .map_or_else(|| PathBuf::from("locks"), |dir| dir.join("locks"))
}

/// The locks of the user whose home is `home`, in the default state dir:
/// `<home>/.local/state/bosun/locks`. Where an elevated run finds the lock
/// of the user who elevated it (§4.5): their `XDG_STATE_HOME` is not in the
/// environment sudo leaves, and one that is cannot be told from root's.
pub fn locks_under_home(home: &Path) -> PathBuf {
    home.join(".local")
        .join("state")
        .join("bosun")
        .join("locks")
}

/// The lock for the manifest at `manifest` (its canonical path) in `dir`:
/// named by a hash of the path, so every project gets its own and none needs
/// a name that is safe on every filesystem. The lock records the path too,
/// so a lock is never read for a manifest it wasn't written for.
pub fn lock_file(dir: &Path, manifest: &Path) -> PathBuf {
    let digest = Sha256::digest(manifest.to_string_lossy().as_bytes());
    let name: String = digest.iter().take(8).map(|b| format!("{b:02x}")).collect();
    dir.join(format!("{name}.json"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn system_state_path_sits_beside_the_os_package_managers() {
        assert_eq!(
            state_path(Scope::System).unwrap(),
            PathBuf::from("/var/lib/bosun/state.json")
        );
    }

    #[test]
    fn user_state_path_is_bosun_state_json_under_an_xdg_dir() {
        let p = state_path(Scope::User).unwrap();
        assert!(p.ends_with("bosun/state.json"), "got {p:?}");
    }

    fn windows_env(var: &str) -> Option<String> {
        match var {
            "ProgramData" => Some(r"C:\ProgramData".to_string()),
            "LOCALAPPDATA" => Some(r"C:\Users\ben\AppData\Local".to_string()),
            "APPDATA" => Some(r"C:\Users\ben\AppData\Roaming".to_string()),
            _ => None,
        }
    }

    #[test]
    fn on_windows_state_is_under_programdata_and_localappdata() {
        assert_eq!(
            windows_state_path(Scope::System, windows_env).unwrap(),
            PathBuf::from(r"C:\ProgramData")
                .join("bosun")
                .join("state.json")
        );
        assert_eq!(
            windows_state_path(Scope::User, windows_env).unwrap(),
            PathBuf::from(r"C:\Users\ben\AppData\Local")
                .join("bosun")
                .join("state.json")
        );
    }

    #[test]
    fn on_windows_config_is_under_appdata() {
        assert_eq!(
            windows_config_path(windows_env).unwrap(),
            PathBuf::from(r"C:\Users\ben\AppData\Roaming")
                .join("bosun")
                .join("config.yaml")
        );
    }

    #[test]
    fn env_var_reads_a_set_variable_and_not_an_unset_one() {
        // PATH is set and non-empty wherever tests run.
        assert_eq!(env_var("PATH"), std::env::var("PATH").ok());
        assert!(env_var("PATH").is_some());
        assert_eq!(env_var("BOSUN_SURELY_UNSET_VARIABLE"), None);
    }

    #[test]
    fn a_missing_windows_folder_variable_is_an_error_naming_it() {
        let err = windows_state_path(Scope::User, |_| None).unwrap_err();
        assert!(err.to_string().contains("LOCALAPPDATA"), "{err}");
        assert!(windows_config_path(|_| None).is_err());
    }

    #[test]
    fn locks_sit_beside_the_user_state() {
        assert_eq!(
            locks_beside(Path::new("/s/bosun/state.json")),
            PathBuf::from("/s/bosun/locks")
        );
    }

    #[test]
    fn a_users_locks_under_their_home_are_in_the_default_state_dir() {
        assert_eq!(
            locks_under_home(Path::new("/home/ben")),
            PathBuf::from("/home/ben/.local/state/bosun/locks")
        );
    }

    #[test]
    fn each_manifest_path_has_its_own_lock_file_and_the_same_path_the_same_one() {
        let dir = Path::new("/s/locks");
        let a = lock_file(dir, Path::new("/home/me/a/bosun.yaml"));
        let b = lock_file(dir, Path::new("/home/me/b/bosun.yaml"));
        assert_ne!(a, b);
        assert_eq!(a, lock_file(dir, Path::new("/home/me/a/bosun.yaml")));
        assert_eq!(a.parent(), Some(dir));
        let name = a.file_name().unwrap().to_string_lossy().to_string();
        assert_eq!(name.len(), "0123456789abcdef.json".len(), "{name}");
        assert!(name.ends_with(".json"));
    }

    #[test]
    fn the_cache_is_bosun_under_the_cache_dir() {
        assert!(cache_dir().unwrap().ends_with("bosun"));
        assert_eq!(
            windows_cache_dir(windows_env).unwrap(),
            PathBuf::from(r"C:\Users\ben\AppData\Local")
                .join("bosun")
                .join("cache")
        );
        assert!(windows_cache_dir(|_| None).is_err());
    }

    #[test]
    fn config_path_is_bosun_config_yaml_under_the_config_dir() {
        let p = config_path().unwrap();
        assert!(p.ends_with("bosun/config.yaml"), "got {p:?}");
    }
}
