//! Default file locations (§4.4, §4.6). XDG on Linux/macOS; the system state
//! file sits beside the OS package managers' own state. (M1 implements the
//! Linux locations; other platforms come with their `Machine` impls.)

use std::path::PathBuf;

use bosun_core::Scope;
use etcetera::base_strategy::{BaseStrategy, Xdg};

use crate::error::IoError;

fn xdg() -> Result<Xdg, IoError> {
    Xdg::new().map_err(|e| IoError::Convert {
        detail: format!("no home directory: {e}"),
    })
}

/// The machine-state file for `scope` (§4.6): system state beside the OS
/// package managers (`/var/lib/bosun`), user state under `$XDG_STATE_HOME`.
pub fn state_path(scope: Scope) -> Result<PathBuf, IoError> {
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
    Ok(xdg()?.config_dir().join("bosun").join("config.yaml"))
}
