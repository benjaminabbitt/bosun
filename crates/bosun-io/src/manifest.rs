//! Manifest loading (§4.4, §5.4) and the parsed-model hash (§4.5).
//!
//! The manifest is read through `annals_yaml::versioned`: annals enforces the
//! strict YAML subset (anchors, tags and multiple documents are refused) and
//! the root `version`; we only decode the already-validated tree into
//! [`Manifest`], surfacing a [`ManifestError`] (an annals `Diagnostic`) for
//! anything structurally wrong — an unknown key, a bad target or selector, a
//! missing name. No `policy` key exists; specs are vers strings kept verbatim
//! for the resolver.
//!
//! [`manifest_hash`] hashes the *parsed model*, not the file text, so a
//! comment-only edit leaves the hash unchanged (§4.5).

use std::collections::BTreeMap;
use std::fmt;
use std::path::Path;

use annals_diagnostic::{Arg, Diagnostic};
use annals_yaml::versioned::Migrations;
use annals_yaml::Value;
use bosun_core::manifest::{InstallBlock, Manifest, ManifestEntry};
use bosun_core::platform::{Selector, Target};
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::error::IoError;

const MANIFEST_VERSION: u32 = 1;

/// Why a manifest's structure is invalid (after annals accepted the YAML).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ManifestError {
    /// A value expected to be a mapping was not.
    NotAMapping { context: String },
    /// A value expected to be a sequence was not.
    NotASequence { context: String },
    /// A value expected to be a scalar was not.
    NotAScalar { context: String },
    /// A key bosun does not recognise (§5.4: unknown key is an error).
    UnknownKey { context: String, key: String },
    /// A `targets` entry that is not a valid target.
    BadTarget { value: String },
    /// An `install` key that is not a valid selector.
    BadSelector { value: String },
    /// A package entry with no `name`.
    MissingName,
}

impl fmt::Display for ManifestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ManifestError::NotAMapping { context } => write!(f, "{context} must be a mapping"),
            ManifestError::NotASequence { context } => write!(f, "{context} must be a sequence"),
            ManifestError::NotAScalar { context } => write!(f, "{context} must be a scalar"),
            ManifestError::UnknownKey { context, key } => {
                write!(f, "unknown key {key:?} in {context}")
            }
            ManifestError::BadTarget { value } => write!(f, "invalid target {value:?}"),
            ManifestError::BadSelector { value } => write!(f, "invalid selector {value:?}"),
            ManifestError::MissingName => f.write_str("a package entry has no `name`"),
        }
    }
}

impl Diagnostic for ManifestError {
    fn code(&self) -> &'static str {
        match self {
            ManifestError::NotAMapping { .. } => "bosun.manifest.not_a_mapping",
            ManifestError::NotASequence { .. } => "bosun.manifest.not_a_sequence",
            ManifestError::NotAScalar { .. } => "bosun.manifest.not_a_scalar",
            ManifestError::UnknownKey { .. } => "bosun.manifest.unknown_key",
            ManifestError::BadTarget { .. } => "bosun.manifest.bad_target",
            ManifestError::BadSelector { .. } => "bosun.manifest.bad_selector",
            ManifestError::MissingName => "bosun.manifest.missing_name",
        }
    }

    fn args(&self) -> Vec<(&'static str, Arg)> {
        match self {
            ManifestError::NotAMapping { context }
            | ManifestError::NotASequence { context }
            | ManifestError::NotAScalar { context } => vec![("context", context.clone().into())],
            ManifestError::UnknownKey { context, key } => {
                vec![
                    ("context", context.clone().into()),
                    ("key", key.clone().into()),
                ]
            }
            ManifestError::BadTarget { value } | ManifestError::BadSelector { value } => {
                vec![("value", value.clone().into())]
            }
            ManifestError::MissingName => vec![],
        }
    }
}

type Result2<T> = std::result::Result<T, ManifestError>;

fn as_map<'a>(v: &'a Value, context: &str) -> Result2<&'a [(String, Value)]> {
    v.as_map().ok_or_else(|| ManifestError::NotAMapping {
        context: context.to_string(),
    })
}

fn check_keys(entries: &[(String, Value)], context: &str, allowed: &[&str]) -> Result2<()> {
    for (k, _) in entries {
        if !allowed.contains(&k.as_str()) {
            return Err(ManifestError::UnknownKey {
                context: context.to_string(),
                key: k.clone(),
            });
        }
    }
    Ok(())
}

fn string_seq(v: &Value, context: &str) -> Result2<Vec<String>> {
    v.as_seq()
        .ok_or_else(|| ManifestError::NotASequence {
            context: context.to_string(),
        })?
        .iter()
        .map(|e| {
            e.as_str()
                .map(str::to_string)
                .ok_or_else(|| ManifestError::NotAScalar {
                    context: context.to_string(),
                })
        })
        .collect()
}

/// A field that may be written as a scalar (one element) or a sequence.
fn string_or_seq(v: &Value, context: &str) -> Result2<Vec<String>> {
    match v.as_str() {
        Some(s) => Ok(vec![s.to_string()]),
        None => string_seq(v, context),
    }
}

fn install_block(v: &Value) -> Result2<InstallBlock> {
    let entries = as_map(v, "an install block")?;
    check_keys(
        entries,
        "an install block",
        &["backend", "backends", "package", "spec"],
    )?;
    // `backends:` (preferred, a list) or `backend:` (one), scalar or sequence.
    let backends = match v.get("backends").or_else(|| v.get("backend")) {
        Some(b) => string_or_seq(b, "backends")?,
        None => Vec::new(),
    };
    let field = |k| v.get(k).and_then(Value::as_str).map(str::to_string);
    Ok(InstallBlock {
        backends,
        package: field("package"),
        spec: field("spec"),
    })
}

fn selector_map(v: &Value, context: &str) -> Result2<BTreeMap<Selector, InstallBlock>> {
    as_map(v, context)?
        .iter()
        .map(|(sel, block)| {
            let selector = sel
                .parse::<Selector>()
                .map_err(|_| ManifestError::BadSelector { value: sel.clone() })?;
            Ok((selector, install_block(block)?))
        })
        .collect()
}

fn entry(v: &Value) -> Result2<ManifestEntry> {
    let entries = as_map(v, "a package")?;
    check_keys(
        entries,
        "a package",
        &["name", "spec", "formerly", "install"],
    )?;
    let name = v
        .get("name")
        .and_then(Value::as_str)
        .ok_or(ManifestError::MissingName)?
        .to_string();
    let formerly = match v.get("formerly") {
        Some(f) => string_seq(f, "formerly")?,
        None => Vec::new(),
    };
    let install = match v.get("install") {
        Some(m) => selector_map(m, "install")?,
        None => BTreeMap::new(),
    };
    Ok(ManifestEntry {
        name,
        spec: v.get("spec").and_then(Value::as_str).map(str::to_string),
        formerly,
        install,
    })
}

/// Decode an already-version-validated manifest tree into the model.
fn decode(root: &Value) -> Result2<Manifest> {
    let entries = as_map(root, "the manifest root")?;
    check_keys(
        entries,
        "the manifest root",
        &["version", "targets", "defaults", "packages"],
    )?;

    let targets = match root.get("targets") {
        Some(t) => string_seq(t, "targets")?
            .iter()
            .map(|s| {
                s.parse::<Target>()
                    .map_err(|_| ManifestError::BadTarget { value: s.clone() })
            })
            .collect::<Result2<Vec<_>>>()?,
        None => Vec::new(),
    };

    let defaults = match root.get("defaults") {
        Some(d) => {
            let dmap = as_map(d, "defaults")?;
            check_keys(dmap, "defaults", &["install"])?;
            match d.get("install") {
                Some(i) => selector_map(i, "defaults.install")?,
                None => BTreeMap::new(),
            }
        }
        None => BTreeMap::new(),
    };

    let packages = match root.get("packages") {
        Some(p) => p
            .as_seq()
            .ok_or_else(|| ManifestError::NotASequence {
                context: "packages".to_string(),
            })?
            .iter()
            .map(entry)
            .collect::<Result2<Vec<_>>>()?,
        None => Vec::new(),
    };

    Ok(Manifest {
        version: root
            .get("version")
            .and_then(Value::as_str)
            .and_then(|s| s.parse().ok())
            .unwrap_or(MANIFEST_VERSION),
        targets,
        defaults,
        packages,
    })
}

/// Load a manifest from YAML text (§4.4). annals enforces the strict subset
/// and the `version`; structural problems surface as [`IoError::Parse`]
/// carrying the [`ManifestError`] diagnostic.
pub fn load_manifest(text: &str, source: &Path) -> Result<Manifest, IoError> {
    let loader = Migrations::new("bosun", MANIFEST_VERSION)
        .build()
        .expect("manifest migration chain has no steps");
    loader
        .load_as(Some(&source.display().to_string()), text, decode)
        .map_err(|e| IoError::Parse {
            path: source.to_path_buf(),
            detail: e.to_string(),
        })
}

// --- canonical hash --------------------------------------------------------

#[derive(Serialize)]
struct CanonManifest {
    version: u32,
    targets: Vec<String>,
    defaults: BTreeMap<String, CanonBlock>,
    packages: Vec<CanonEntry>,
}

#[derive(Serialize)]
struct CanonEntry {
    name: String,
    spec: Option<String>,
    formerly: Vec<String>,
    install: BTreeMap<String, CanonBlock>,
}

#[derive(Serialize)]
struct CanonBlock {
    backends: Vec<String>,
    package: Option<String>,
    spec: Option<String>,
}

fn canon_block(b: &InstallBlock) -> CanonBlock {
    CanonBlock {
        backends: b.backends.clone(),
        package: b.package.clone(),
        spec: b.spec.clone(),
    }
}

fn canon_selector_map(m: &BTreeMap<Selector, InstallBlock>) -> BTreeMap<String, CanonBlock> {
    m.iter()
        .map(|(s, b)| (s.to_string(), canon_block(b)))
        .collect()
}

/// A sha256 over a deterministic serialization of the parsed model (§4.5).
/// Because it hashes the model (comments already stripped by annals), a
/// comment-only edit does not change it.
pub fn manifest_hash(m: &Manifest) -> String {
    let canon = CanonManifest {
        version: m.version,
        targets: m.targets.iter().map(Target::to_string).collect(),
        defaults: canon_selector_map(&m.defaults),
        packages: m
            .packages
            .iter()
            .map(|e| CanonEntry {
                name: e.name.clone(),
                spec: e.spec.clone(),
                formerly: e.formerly.clone(),
                install: canon_selector_map(&e.install),
            })
            .collect(),
    };
    let bytes = serde_json::to_vec(&canon).expect("canonical manifest serialization is infallible");
    let hex: String = Sha256::digest(&bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    format!("sha256:{hex}")
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"version: 1
targets: [linux/amd64, linux/arm64]
defaults:
  install:
    linux: {backend: cargo}
packages:
  - name: fd
    spec: "10.2.0"
    formerly: [fdfind]
    install:
      linux:debian: {backend: apt, package: fd-find, spec: ">=9|<10"}
  - name: jq
    spec: "vers:deb/>=1.7.1|<1.7.2~"
"#;

    fn load(text: &str) -> Manifest {
        load_manifest(text, Path::new("bosun.yaml")).unwrap()
    }

    #[test]
    fn loads_targets_defaults_and_packages() {
        let m = load(SAMPLE);
        assert_eq!(m.version, 1);
        assert_eq!(
            m.targets,
            vec![
                "linux/amd64".parse().unwrap(),
                "linux/arm64".parse().unwrap()
            ]
        );
        assert_eq!(m.defaults.len(), 1);
        assert_eq!(m.packages.len(), 2);
        let fd = &m.packages[0];
        assert_eq!(fd.name, "fd");
        assert_eq!(fd.spec.as_deref(), Some("10.2.0"));
        assert_eq!(fd.formerly, vec!["fdfind"]);
        let deb = &fd.install[&"linux:debian".parse::<Selector>().unwrap()];
        assert_eq!(deb.backends, vec!["apt"]);
        assert_eq!(deb.package.as_deref(), Some("fd-find"));
        assert_eq!(deb.spec.as_deref(), Some(">=9|<10"));
    }

    #[test]
    fn backends_accepts_scalar_or_sequence() {
        let one =
            "version: 1\npackages:\n  - name: x\n    install:\n      linux: {backend: cargo}\n";
        let seq = "version: 1\npackages:\n  - name: x\n    install:\n      linux: {backends: [cargo, apt]}\n";
        let sel = "linux".parse::<Selector>().unwrap();
        assert_eq!(load(one).packages[0].install[&sel].backends, vec!["cargo"]);
        assert_eq!(
            load(seq).packages[0].install[&sel].backends,
            vec!["cargo", "apt"]
        );
    }

    #[test]
    fn backends_order_changes_the_hash() {
        // Preference order is semantic: a different order is a different manifest.
        let ab = "version: 1\npackages:\n  - name: x\n    install:\n      linux: {backends: [cargo, apt]}\n";
        let ba = "version: 1\npackages:\n  - name: x\n    install:\n      linux: {backends: [apt, cargo]}\n";
        assert_ne!(manifest_hash(&load(ab)), manifest_hash(&load(ba)));
    }

    #[test]
    fn unknown_key_is_rejected() {
        // `policy` was removed; any unknown key is an error.
        let text = "version: 1\npackages:\n  - name: fd\n    policy: exact\n";
        let err = load_manifest(text, Path::new("bosun.yaml")).unwrap_err();
        assert!(matches!(err, IoError::Parse { .. }));
        assert!(format!("{err}").contains("policy"), "message: {err}");
    }

    #[test]
    fn non_subset_yaml_is_refused_by_annals() {
        let text = "version: 1\ntargets: &a [linux/amd64]\n";
        assert!(matches!(
            load_manifest(text, Path::new("bosun.yaml")),
            Err(IoError::Parse { .. })
        ));
    }

    #[test]
    fn bad_target_and_selector_are_rejected() {
        assert!(load_manifest("version: 1\ntargets: [not/an/arch]\n", Path::new("m")).is_err());
        let bad_sel =
            "version: 1\npackages:\n  - name: x\n    install:\n      plan9: {backend: cargo}\n";
        assert!(load_manifest(bad_sel, Path::new("m")).is_err());
    }

    #[test]
    fn hash_ignores_comments_but_not_content() {
        let with_comments = format!("# a comment\n{SAMPLE}# trailing\n");
        assert_eq!(
            manifest_hash(&load(SAMPLE)),
            manifest_hash(&load(&with_comments)),
            "comment-only edits must not change the hash"
        );
        // A real content change does change it.
        let changed = SAMPLE.replace("10.2.0", "10.3.0");
        assert_ne!(manifest_hash(&load(SAMPLE)), manifest_hash(&load(&changed)));
    }

    #[test]
    fn hash_is_sha256_prefixed() {
        assert!(manifest_hash(&load(SAMPLE)).starts_with("sha256:"));
    }

    #[test]
    fn diagnostic_codes_are_stable_per_variant() {
        // The codes are the diagnostic's identity for annals; pin them so a
        // rename is a deliberate, reviewed change, not a silent one.
        assert_eq!(
            ManifestError::NotAMapping {
                context: "x".into()
            }
            .code(),
            "bosun.manifest.not_a_mapping"
        );
        assert_eq!(
            ManifestError::UnknownKey {
                context: "x".into(),
                key: "k".into()
            }
            .code(),
            "bosun.manifest.unknown_key"
        );
        assert_eq!(
            ManifestError::BadSelector { value: "s".into() }.code(),
            "bosun.manifest.bad_selector"
        );
        assert_eq!(
            ManifestError::MissingName.code(),
            "bosun.manifest.missing_name"
        );
    }

    #[test]
    fn diagnostic_args_carry_the_offending_values() {
        // `args` feeds the rendered message; an empty list would drop the key
        // and context the diagnostic names.
        let args = ManifestError::UnknownKey {
            context: "the manifest root".into(),
            key: "policy".into(),
        }
        .args();
        let keys: Vec<&str> = args.iter().map(|(k, _)| *k).collect();
        assert_eq!(keys, vec!["context", "key"]);
    }
}
