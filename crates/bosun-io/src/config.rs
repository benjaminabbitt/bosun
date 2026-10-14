//! User config (§4.4, §5.3): `<config dir>/bosun/config.yaml`, a strict YAML
//! subset read by annals-yaml. For M1 it carries the preferred installer
//! order; its annals diagnostics surface as parse errors (not re-implemented).

use std::path::Path;

use crate::error::IoError;

/// User preferences from `config.yaml`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct UserConfig {
    /// The preferred installer order (§5.3), e.g. `[brew, cargo]`.
    pub installer: Vec<String>,
}

/// Parse a `config.yaml` body. annals refuses anchors/tags/multiple documents;
/// those diagnostics surface here as [`IoError::Parse`].
pub fn from_yaml(text: &str, source: &Path) -> Result<UserConfig, IoError> {
    let value = annals_yaml::parse(text).map_err(|e| IoError::Parse {
        path: source.to_path_buf(),
        detail: e.to_string(),
    })?;
    let installer = value
        .get("installer")
        .and_then(|v| v.as_seq())
        .map(|seq| {
            seq.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    Ok(UserConfig { installer })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_installer_order() {
        let cfg = from_yaml("installer: [brew, cargo]\n", Path::new("config.yaml")).unwrap();
        assert_eq!(cfg.installer, vec!["brew", "cargo"]);
    }

    #[test]
    fn absent_installer_is_empty() {
        let cfg = from_yaml("other: 1\n", Path::new("config.yaml")).unwrap();
        assert!(cfg.installer.is_empty());
    }

    #[test]
    fn rejects_non_subset_yaml() {
        // An anchor is refused by annals; we surface its diagnostic.
        assert!(matches!(
            from_yaml("installer: &a [brew]\n", Path::new("config.yaml")),
            Err(IoError::Parse { .. })
        ));
    }
}
