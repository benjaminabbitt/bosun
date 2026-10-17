//! The build-args driver (§8.3): a lock's versions as the variables a container
//! build reads. Each manifest entry gives `<VAR>_PACKAGE` (the backend's own
//! package id, which differs per platform) and `<VAR>_VERSION`; `<VAR>` is the
//! entry's `var`, else its logical name upper-cased with every other character
//! an underscore. Pure: the caller hands in the manifest and, per target
//! platform, the lock that platform resolved.

use std::collections::BTreeMap;

use bosun_core::manifest::{is_variable_name, Manifest};
use bosun_core::record::LockDocument;
use bosun_core::Target;
use serde::Serialize;

use crate::errmsg;

/// Why a manifest's variables can't be rendered.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum RenderError {
    /// Two entries give the same variable stem.
    #[error("{kind}: {var} ({first}, {second})", kind = errmsg::VAR_CLASH)]
    VarClash {
        /// The shared stem.
        var: String,
        /// The first entry's logical name.
        first: String,
        /// The second entry's logical name.
        second: String,
    },
    /// A logical name whose derived stem is not a variable name.
    #[error("{kind}: {name} gives {var}; set `var`", kind = errmsg::BAD_DERIVED_VAR)]
    BadDerivedVar {
        /// The entry's logical name.
        name: String,
        /// The stem derived from it.
        var: String,
    },
}

/// The variable stem a logical name gives: upper-cased, every character but
/// an ASCII letter or digit an underscore (`fd-find` → `FD_FIND`).
pub fn derived_var(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_uppercase()
            } else {
                '_'
            }
        })
        .collect()
}

/// Each entry's variable stem, in manifest order: its `var`, else the one its
/// name gives. Checked across every entry, applicable here or not, so a
/// manifest renders (or fails to) the same on every machine.
pub fn var_stems(manifest: &Manifest) -> Result<Vec<(String, String)>, RenderError> {
    let mut stems: Vec<(String, String)> = Vec::new();
    for entry in &manifest.packages {
        let var = match &entry.var {
            Some(var) => var.clone(),
            None => {
                let var = derived_var(&entry.name);
                if !is_variable_name(&var) {
                    return Err(RenderError::BadDerivedVar {
                        name: entry.name.clone(),
                        var,
                    });
                }
                var
            }
        };
        if let Some((first, _)) = stems.iter().find(|(_, v)| *v == var) {
            return Err(RenderError::VarClash {
                var,
                first: first.clone(),
                second: entry.name.clone(),
            });
        }
        stems.push((entry.name.clone(), var));
    }
    Ok(stems)
}

/// The rendered versions file (§8.3): shaped like the lock, each package with
/// its value per target platform.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Rendered {
    /// The packages, in manifest order; one with no target is left out.
    pub packages: Vec<RenderedPackage>,
}

/// One package's rendered values.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct RenderedPackage {
    /// The logical name.
    pub name: String,
    /// The variable stem (`FD`).
    pub var: String,
    /// The value per target platform (`linux/amd64`).
    pub targets: BTreeMap<String, RenderedLeaf>,
}

/// One package on one target platform.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct RenderedLeaf {
    /// The backend that installs it there.
    pub backend: String,
    /// The backend's own package id (`<VAR>_PACKAGE`).
    pub package: String,
    /// The canonical identity.
    pub purl: String,
    /// The locked version (`<VAR>_VERSION`).
    pub version: String,
}

/// Render `manifest`'s entries from each target platform's lock: an entry's
/// value on a platform is the lock's leaf under that platform, else under
/// `any`. An entry a platform's lock lacks (no block applies there) has no
/// value for it.
pub fn render(
    manifest: &Manifest,
    locks: &[(Target, &LockDocument)],
) -> Result<Rendered, RenderError> {
    let stems = var_stems(manifest)?;
    let mut packages = Vec::new();
    for (name, var) in stems {
        let mut targets = BTreeMap::new();
        for (platform, lock) in locks {
            let leaf = lock
                .packages
                .iter()
                .find(|p| p.name == name)
                .and_then(|p| p.targets.get(platform).or(p.targets.get(&Target::Any)));
            if let Some(leaf) = leaf {
                targets.insert(
                    platform.to_string(),
                    RenderedLeaf {
                        backend: leaf.placement.backend.as_str().to_string(),
                        package: leaf.placement.purl.native_id(),
                        purl: leaf.placement.purl.to_string(),
                        version: leaf.resolved.as_str().to_string(),
                    },
                );
            }
        }
        if !targets.is_empty() {
            packages.push(RenderedPackage { name, var, targets });
        }
    }
    Ok(Rendered { packages })
}

impl Rendered {
    /// The `(NAME, value)` pairs for one platform, in manifest order:
    /// `<VAR>_PACKAGE` then `<VAR>_VERSION` per package that has a value there.
    pub fn variables(&self, platform: &Target) -> Vec<(String, String)> {
        let key = platform.to_string();
        let mut vars = Vec::new();
        for p in &self.packages {
            if let Some(leaf) = p.targets.get(&key) {
                vars.push((format!("{}_PACKAGE", p.var), leaf.package.clone()));
                vars.push((format!("{}_VERSION", p.var), leaf.version.clone()));
            }
        }
        vars
    }

    /// `--build-arg NAME=value …` for `docker build` (§8.3), one line.
    pub fn build_args(&self, platform: &Target) -> String {
        self.variables(platform)
            .iter()
            .map(|(name, value)| format!("--build-arg {name}={value}"))
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// `NAME=value` lines for an env file (§8.3).
    pub fn env_lines(&self, platform: &Target) -> String {
        self.variables(platform)
            .iter()
            .map(|(name, value)| format!("{name}={value}"))
            .collect::<Vec<_>>()
            .join("\n")
    }
}

#[cfg(test)]
mod tests {
    use bosun_core::manifest::ManifestEntry;
    use bosun_core::record::{LockLeaf, Package, Placement};
    use bosun_core::{Arch, BackendId, Os, Purl, Scheme, Spec, Version};

    use super::*;

    const AMD64: Target = Target::Platform {
        os: Os::Linux,
        arch: Arch::Amd64,
    };
    const ARM64: Target = Target::Platform {
        os: Os::Linux,
        arch: Arch::Arm64,
    };

    fn entry(name: &str, var: Option<&str>) -> ManifestEntry {
        ManifestEntry {
            name: name.to_string(),
            spec: None,
            formerly: Vec::new(),
            install: BTreeMap::new(),
            var: var.map(str::to_string),
        }
    }

    fn manifest(entries: Vec<ManifestEntry>) -> Manifest {
        Manifest {
            version: 2,
            defaults: BTreeMap::new(),
            packages: entries,
        }
    }

    fn leaf(backend: &str, purl: &str, version: &str) -> LockLeaf {
        LockLeaf {
            placement: Placement {
                backend: BackendId::new(backend),
                purl: purl.parse::<Purl>().unwrap(),
            },
            spec: None,
            resolved: Version::parse(version, Scheme::Generic).unwrap(),
            index_snapshot: None,
            checksum: None,
        }
    }

    fn lock(packages: Vec<(&str, Target, LockLeaf)>) -> LockDocument {
        LockDocument {
            lockfile_version: 2,
            manifest: "/p/bosun.yaml".to_string(),
            manifest_hash: "sha256:0".to_string(),
            packages: packages
                .into_iter()
                .map(|(name, target, leaf)| Package {
                    name: name.to_string(),
                    meta: Spec::new("*", "generic").unwrap(),
                    targets: BTreeMap::from([(target, leaf)]),
                })
                .collect(),
        }
    }

    #[test]
    fn a_derived_stem_is_the_name_upper_cased_with_underscores() {
        assert_eq!(derived_var("fd"), "FD");
        assert_eq!(derived_var("fd-find"), "FD_FIND");
        assert_eq!(derived_var("github.cli+x"), "GITHUB_CLI_X");
        assert_eq!(derived_var("ripgrep2"), "RIPGREP2");
        assert_eq!(derived_var("caf\u{e9}"), "CAF_");
    }

    #[test]
    fn stems_are_each_entrys_var_else_its_names() {
        let m = manifest(vec![entry("fd-find", None), entry("jq", Some("JQ_TOOL"))]);
        assert_eq!(
            var_stems(&m).unwrap(),
            vec![
                ("fd-find".to_string(), "FD_FIND".to_string()),
                ("jq".to_string(), "JQ_TOOL".to_string()),
            ]
        );
    }

    #[test]
    fn two_entries_with_one_stem_are_a_clash_naming_both() {
        let m = manifest(vec![
            entry("fd-find", None),
            entry("jq", None),
            entry("fd_find", None),
        ]);
        assert_eq!(
            var_stems(&m),
            Err(RenderError::VarClash {
                var: "FD_FIND".to_string(),
                first: "fd-find".to_string(),
                second: "fd_find".to_string(),
            })
        );
        let by_var = manifest(vec![entry("fd", None), entry("fd-find", Some("FD"))]);
        assert!(matches!(
            var_stems(&by_var),
            Err(RenderError::VarClash { .. })
        ));
    }

    #[test]
    fn a_name_whose_stem_starts_with_a_digit_needs_a_var() {
        let m = manifest(vec![entry("7zip", None)]);
        let err = var_stems(&m).unwrap_err();
        assert_eq!(
            err,
            RenderError::BadDerivedVar {
                name: "7zip".to_string(),
                var: "7ZIP".to_string(),
            }
        );
        assert!(err.to_string().contains("set `var`"), "{err}");
        let named = manifest(vec![entry("7zip", Some("SEVENZIP"))]);
        assert!(var_stems(&named).is_ok());
    }

    #[test]
    fn render_takes_the_platform_leaf_else_any_and_skips_entries_with_neither() {
        let m = manifest(vec![
            entry("fd", None),
            entry("winfetch", None),
            entry("ripgrep", None),
        ]);
        let l = lock(vec![
            (
                "fd",
                AMD64,
                leaf("apt", "pkg:deb/debian/fd-find?arch=amd64", "9.0.0-1"),
            ),
            (
                "ripgrep",
                Target::Any,
                leaf("cargo", "pkg:cargo/ripgrep", "14.1.1"),
            ),
        ]);
        let r = render(&m, &[(AMD64, &l)]).unwrap();
        assert_eq!(
            r.packages
                .iter()
                .map(|p| p.name.as_str())
                .collect::<Vec<_>>(),
            vec!["fd", "ripgrep"]
        );
        assert_eq!(
            r.packages[0].targets["linux/amd64"],
            RenderedLeaf {
                backend: "apt".to_string(),
                package: "fd-find".to_string(),
                purl: "pkg:deb/debian/fd-find?arch=amd64".to_string(),
                version: "9.0.0-1".to_string(),
            }
        );
        assert_eq!(r.packages[1].targets["linux/amd64"].version, "14.1.1");
    }

    #[test]
    fn a_github_package_is_its_owner_and_repo() {
        let m = manifest(vec![entry("just", None)]);
        let l = lock(vec![(
            "just",
            AMD64,
            leaf("github-release", "pkg:github/casey/just", "1.40.0"),
        )]);
        let r = render(&m, &[(AMD64, &l)]).unwrap();
        assert_eq!(r.packages[0].targets["linux/amd64"].package, "casey/just");
    }

    #[test]
    fn render_keys_each_platform_by_its_own_lock() {
        let m = manifest(vec![entry("fd", None)]);
        let amd = lock(vec![(
            "fd",
            AMD64,
            leaf("apt", "pkg:deb/debian/fd-find?arch=amd64", "9.0.0-1"),
        )]);
        let arm = lock(vec![(
            "fd",
            ARM64,
            leaf("apt", "pkg:deb/debian/fd-find?arch=arm64", "9.0.0-2"),
        )]);
        let r = render(&m, &[(AMD64, &amd), (ARM64, &arm)]).unwrap();
        let targets = &r.packages[0].targets;
        assert_eq!(targets["linux/amd64"].version, "9.0.0-1");
        assert_eq!(targets["linux/arm64"].version, "9.0.0-2");
        // A leaf under another platform is not this platform's.
        let r = render(&m, &[(ARM64, &amd)]).unwrap();
        assert!(r.packages.is_empty());
    }

    #[test]
    fn render_fails_on_a_clash() {
        let m = manifest(vec![entry("a-b", None), entry("a_b", None)]);
        assert!(render(&m, &[]).is_err());
    }

    fn two() -> Rendered {
        let m = manifest(vec![entry("fd", None), entry("ripgrep", Some("RG"))]);
        let l = lock(vec![
            (
                "fd",
                AMD64,
                leaf("apt", "pkg:deb/debian/fd-find?arch=amd64", "9.0.0-1"),
            ),
            (
                "ripgrep",
                Target::Any,
                leaf("cargo", "pkg:cargo/ripgrep", "14.1.1"),
            ),
        ]);
        render(&m, &[(AMD64, &l)]).unwrap()
    }

    #[test]
    fn build_args_are_one_line_of_flags_package_before_version() {
        assert_eq!(
            two().build_args(&AMD64),
            "--build-arg FD_PACKAGE=fd-find --build-arg FD_VERSION=9.0.0-1 \
             --build-arg RG_PACKAGE=ripgrep --build-arg RG_VERSION=14.1.1"
        );
        assert_eq!(two().build_args(&ARM64), "");
    }

    #[test]
    fn env_lines_are_one_assignment_per_line() {
        assert_eq!(
            two().env_lines(&AMD64),
            "FD_PACKAGE=fd-find\nFD_VERSION=9.0.0-1\nRG_PACKAGE=ripgrep\nRG_VERSION=14.1.1"
        );
    }
}
