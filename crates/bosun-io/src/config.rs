//! User config (§4.4, §5.3): `<config dir>/bosun/config.yaml`, a strict YAML
//! subset read by annals-yaml through its versioned loader, so an older file
//! migrates in memory. It carries the preferred installer order; its annals
//! diagnostics surface as parse errors (not re-implemented).

use std::fmt;
use std::path::Path;

use annals_diagnostic::{Arg, Diagnostic};
use annals_yaml::versioned::{Loader, MigrationError, Migrations};
use annals_yaml::Value;

use crate::error::IoError;

/// The config format version this build reads.
pub const CONFIG_VERSION: u32 = 1;

/// User preferences from `config.yaml`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct UserConfig {
    /// The preferred installer order (§5.3), e.g. `[brew, cargo]`.
    pub installer: Vec<String>,
}

/// Why a config's structure is invalid (after annals accepted the YAML).
#[derive(Clone, Debug, PartialEq, Eq)]
enum ConfigError {
    UnknownKey { key: String },
    InstallerNotNames,
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConfigError::UnknownKey { key } => write!(f, "unknown key {key:?}"),
            ConfigError::InstallerNotNames => {
                f.write_str("installer must be a list of backend names")
            }
        }
    }
}

impl Diagnostic for ConfigError {
    fn code(&self) -> &'static str {
        match self {
            ConfigError::UnknownKey { .. } => "bosun.config.unknown_key",
            ConfigError::InstallerNotNames => "bosun.config.installer_not_names",
        }
    }

    fn args(&self) -> Vec<(&'static str, Arg)> {
        match self {
            ConfigError::UnknownKey { key } => vec![("key", key.clone().into())],
            ConfigError::InstallerNotNames => vec![],
        }
    }
}

/// The config loader: version 0 (a file written before configs had a
/// `version`) and up, migrated to [`CONFIG_VERSION`].
fn loader() -> Loader {
    Migrations::new("config", CONFIG_VERSION)
        .step(0, v0_to_v1)
        .build()
        .expect("the config migration chain is whole")
}

/// Version 0 → 1: the same keys; the loader stamps the version.
fn v0_to_v1(_root: &mut Value) -> Result<(), MigrationError> {
    Ok(())
}

fn decode(root: &Value) -> Result<UserConfig, ConfigError> {
    let entries = root.as_map().unwrap_or_default();
    if let Some((key, _)) = entries
        .iter()
        .find(|(k, _)| k != "version" && k != "installer")
    {
        return Err(ConfigError::UnknownKey { key: key.clone() });
    }
    let installer = match root.get("installer") {
        None => Vec::new(),
        Some(v) => v
            .as_seq()
            .ok_or(ConfigError::InstallerNotNames)?
            .iter()
            .map(|i| {
                i.as_str()
                    .map(String::from)
                    .ok_or(ConfigError::InstallerNotNames)
            })
            .collect::<Result<_, _>>()?,
    };
    Ok(UserConfig { installer })
}

/// Parse a `config.yaml` body. annals refuses anchors/tags/multiple documents
/// and a newer `version`; those diagnostics surface here as
/// [`IoError::Parse`].
pub fn from_yaml(text: &str, source: &Path) -> Result<UserConfig, IoError> {
    loader()
        .load_as(Some(&source.display().to_string()), text, decode)
        .map_err(|e| IoError::Parse {
            path: source.to_path_buf(),
            detail: e.to_string(),
        })
}

#[cfg(test)]
mod tests {
    use annals_yaml::versioned::Fixture;

    use super::*;

    fn read(text: &str) -> Result<UserConfig, IoError> {
        from_yaml(text, Path::new("config.yaml"))
    }

    #[test]
    fn reads_installer_order() {
        let cfg = read("version: 1\ninstaller: [brew, cargo]\n").unwrap();
        assert_eq!(cfg.installer, vec!["brew", "cargo"]);
    }

    #[test]
    fn absent_installer_is_empty() {
        assert!(read("version: 1\n").unwrap().installer.is_empty());
    }

    #[test]
    fn a_config_without_a_version_is_version_0_and_migrates() {
        loader().assert_fixtures(&[
            Fixture {
                name: "current",
                input: "version: 1\ninstaller: [scoop, winget]\n",
                expected: None,
            },
            Fixture {
                name: "unversioned",
                input: "installer: [scoop, winget]\n",
                expected: Some("version: 1\ninstaller: [scoop, winget]\n"),
            },
            Fixture {
                name: "unversioned and empty",
                input: "{}\n",
                expected: Some("version: 1\n"),
            },
        ]);
        let cfg = read("installer: [brew]\n").unwrap();
        assert_eq!(cfg.installer, vec!["brew"]);
    }

    #[test]
    fn a_newer_config_is_refused() {
        let err = read("version: 2\n").unwrap_err();
        assert!(err.to_string().contains("newer"), "{err}");
    }

    #[test]
    fn an_unknown_key_is_refused() {
        let err = read("version: 1\ninstallers: [brew]\n").unwrap_err();
        assert!(err.to_string().contains("installers"), "{err}");
    }

    #[test]
    fn installer_must_be_a_list_of_names() {
        assert!(read("version: 1\ninstaller: brew\n").is_err());
        assert!(read("version: 1\ninstaller: [[brew]]\n").is_err());
    }

    #[test]
    fn diagnostics_have_stable_codes_and_carry_the_key() {
        let unknown = ConfigError::UnknownKey {
            key: "installers".into(),
        };
        assert_eq!(unknown.code(), "bosun.config.unknown_key");
        let keys: Vec<&str> = unknown.args().iter().map(|(k, _)| *k).collect();
        assert_eq!(keys, vec!["key"]);
        assert_eq!(
            ConfigError::InstallerNotNames.code(),
            "bosun.config.installer_not_names"
        );
    }

    #[test]
    fn rejects_non_subset_yaml() {
        // An anchor is refused by annals; we surface its diagnostic.
        assert!(matches!(
            read("version: 1\ninstaller: &a [brew]\n"),
            Err(IoError::Parse { .. })
        ));
    }
}
