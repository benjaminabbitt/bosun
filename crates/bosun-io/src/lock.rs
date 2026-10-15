//! Lockfile JSON (§4.5): one machine's lock for one manifest, kept in the
//! machine's state directory. Byte-stable canonical output: camelCase keys in
//! fixed field order, target keys and packages sorted, pretty-printed. The wire
//! shape lives here (serde DTOs); the domain model in bosun-core stays
//! wire-free.
//!
//! Version 1 was the shared, committed `bosun.lock` with a header per
//! platform; version 2 is per machine. A lock is bosun's own regenerable
//! output, so a version this build doesn't write is refused, and `bosun lock`
//! writes a fresh one; there is no migration.

use std::collections::BTreeMap;
use std::path::Path;

use bosun_core::record::{LockDocument, LockLeaf, LockPackage, Package, Placement};
use bosun_core::{BackendId, Checksum, Purl, Scheme, Spec, Target, Version};
use serde::{Deserialize, Serialize};

use crate::error::IoError;
use crate::BackendSchemes;

/// The newest lockfile format version this build reads and writes.
pub const LOCKFILE_VERSION: u32 = 2;

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct LockDoc {
    lockfile_version: u32,
    manifest: String,
    manifest_hash: String,
    packages: Vec<PkgEntry>,
}

/// Just the version, read first: a document of another version is refused
/// before its shape is decoded.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct VersionOnly {
    lockfile_version: u32,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PkgEntry {
    name: String,
    spec: String,
    targets: BTreeMap<String, LeafEntry>,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct LeafEntry {
    backend: String,
    purl: String,
    resolved: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    spec: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    index_snapshot: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    checksum: Option<String>,
}

impl LockDoc {
    fn from_core(doc: &LockDocument) -> LockDoc {
        let mut packages: Vec<PkgEntry> = doc
            .packages
            .iter()
            .map(|p| PkgEntry {
                name: p.name.clone(),
                spec: p.meta.canonical(),
                targets: p
                    .targets
                    .iter()
                    .map(|(t, leaf)| {
                        (
                            t.to_string(),
                            LeafEntry {
                                backend: leaf.placement.backend.as_str().to_string(),
                                purl: leaf.placement.purl.to_string(),
                                resolved: leaf.resolved.as_str().to_string(),
                                spec: leaf.spec.as_ref().map(Spec::canonical),
                                index_snapshot: leaf.index_snapshot.clone(),
                                checksum: leaf.checksum.as_ref().map(|c| c.0.clone()),
                            },
                        )
                    })
                    .collect(),
            })
            .collect();
        // Logical names are unique per manifest; sort for a stable diff.
        packages.sort_by(|a, b| a.name.cmp(&b.name));
        LockDoc {
            lockfile_version: doc.lockfile_version,
            manifest: doc.manifest.clone(),
            manifest_hash: doc.manifest_hash.clone(),
            packages,
        }
    }

    fn into_core(self, schemes: &BackendSchemes, source: &Path) -> Result<LockDocument, IoError> {
        let packages = self
            .packages
            .into_iter()
            .map(|p| {
                let meta = spec(&p.spec)?;
                let leaves = p
                    .targets
                    .into_iter()
                    .map(|(t, e)| {
                        // §6: the version's scheme comes from the leaf's backend's
                        // declared vers type, not the purl type.
                        let scheme = crate::scheme_of(schemes, &e.backend, source)?;
                        Ok((parse_target(&t)?, leaf(e, scheme)?))
                    })
                    .collect::<Result<_, IoError>>()?;
                Ok(Package {
                    name: p.name,
                    meta,
                    targets: leaves,
                })
            })
            .collect::<Result<Vec<LockPackage>, IoError>>()?;
        Ok(LockDocument {
            lockfile_version: self.lockfile_version,
            manifest: self.manifest,
            manifest_hash: self.manifest_hash,
            packages,
        })
    }
}

fn leaf(e: LeafEntry, scheme: Scheme) -> Result<LockLeaf, IoError> {
    let purl: Purl = e.purl.parse().map_err(|err| IoError::Convert {
        detail: format!("purl {:?}: {err}", e.purl),
    })?;
    let resolved = Version::parse(&e.resolved, scheme).map_err(|err| IoError::Convert {
        detail: format!("version {:?}: {err}", e.resolved),
    })?;
    Ok(LockLeaf {
        placement: Placement {
            backend: BackendId::new(e.backend),
            purl,
        },
        spec: e.spec.as_deref().map(spec).transpose()?,
        resolved,
        index_snapshot: e.index_snapshot,
        checksum: e.checksum.map(Checksum),
    })
}

fn spec(s: &str) -> Result<Spec, IoError> {
    // A lockfile spec is always canonical `vers:<type>/…`, so the type arg is
    // unused; pass an empty placeholder.
    Spec::new(s, "").map_err(|err| IoError::Convert {
        detail: format!("spec {s:?}: {err}"),
    })
}

fn parse_target(t: &str) -> Result<Target, IoError> {
    t.parse().map_err(|err| IoError::Convert {
        detail: format!("target {t:?}: {err}"),
    })
}

/// Serialize a lockfile to its canonical JSON text.
pub fn to_json(doc: &LockDocument) -> String {
    serde_json::to_string_pretty(&LockDoc::from_core(doc))
        .expect("lock serialization is infallible")
}

/// Parse a lockfile from JSON text, refusing any version but
/// [`LOCKFILE_VERSION`]. `schemes` maps each backend to its version scheme
/// (§6), so a leaf's `resolved` parses under the backend's vers type rather
/// than its purl type.
pub fn from_json(
    text: &str,
    source: &Path,
    schemes: &BackendSchemes,
) -> Result<LockDocument, IoError> {
    let parse_error = |e: serde_json::Error| IoError::Parse {
        path: source.to_path_buf(),
        detail: e.to_string(),
    };
    let VersionOnly { lockfile_version } = serde_json::from_str(text).map_err(parse_error)?;
    if lockfile_version != LOCKFILE_VERSION {
        return Err(IoError::LockfileVersion {
            found: lockfile_version,
            max: LOCKFILE_VERSION,
        });
    }
    let doc: LockDoc = serde_json::from_str(text).map_err(parse_error)?;
    doc.into_core(schemes, source)
}

#[cfg(test)]
mod tests {
    use super::*;
    use bosun_core::Scheme;
    use std::collections::BTreeMap;

    /// The backend→scheme map a reader uses (§6): apt compares as Debian,
    /// github-release as semver though its purl type is `github`.
    fn schemes() -> BackendSchemes {
        BackendSchemes::from([
            ("apt".to_string(), Scheme::Debian),
            ("github-release".to_string(), Scheme::Semver),
        ])
    }

    fn sample() -> LockDocument {
        let t = |s: &str| -> Target { s.parse().unwrap() };
        let leaf = LockLeaf {
            placement: Placement {
                backend: BackendId::new("apt"),
                purl: "pkg:deb/ubuntu/jq?arch=amd64&distro=noble".parse().unwrap(),
            },
            spec: None,
            resolved: Version::parse("1.7.1-3ubuntu0.1", Scheme::Debian).unwrap(),
            index_snapshot: Some("2026-10-13T19:00:00Z".into()),
            checksum: None,
        };
        LockDocument {
            lockfile_version: 2,
            manifest: "/home/me/proj/bosun.yaml".into(),
            manifest_hash: "sha256:9f2c".into(),
            packages: vec![Package {
                name: "jq".into(),
                // Canonical form: a lockfile stores and reads back canonical
                // vers, so a round-trip preserves this exactly.
                meta: Spec::new("vers:deb/>=1.7.1|<1.7.2~", "deb").unwrap(),
                targets: BTreeMap::from([(t("linux/amd64"), leaf)]),
            }],
        }
    }

    const GOLDEN: &str = r#"{
  "lockfileVersion": 2,
  "manifest": "/home/me/proj/bosun.yaml",
  "manifestHash": "sha256:9f2c",
  "packages": [
    {
      "name": "jq",
      "spec": "vers:deb/>=1.7.1|<1.7.2~",
      "targets": {
        "linux/amd64": {
          "backend": "apt",
          "purl": "pkg:deb/ubuntu/jq?arch=amd64&distro=noble",
          "resolved": "1.7.1-3ubuntu0.1",
          "indexSnapshot": "2026-10-13T19:00:00Z"
        }
      }
    }
  ]
}"#;

    #[test]
    fn serializes_to_byte_stable_golden() {
        assert_eq!(to_json(&sample()), GOLDEN);
    }

    #[test]
    fn roundtrips_through_json() {
        let doc = sample();
        let back = from_json(&to_json(&doc), Path::new("bosun.lock"), &schemes()).unwrap();
        assert_eq!(back, doc);
    }

    #[test]
    fn refuses_a_newer_version() {
        let text = r#"{"lockfileVersion":3,"anything":"else"}"#;
        assert!(matches!(
            from_json(text, Path::new("lock.json"), &schemes()),
            Err(IoError::LockfileVersion { found: 3, max: 2 })
        ));
    }

    #[test]
    fn refuses_the_shared_version_1_lock() {
        // Version 1 is the committed multi-platform bosun.lock; it is refused by
        // its version, before its different shape is decoded.
        let text = r#"{"lockfileVersion":1,"targets":{},"packages":[]}"#;
        assert!(matches!(
            from_json(text, Path::new("lock.json"), &schemes()),
            Err(IoError::LockfileVersion { found: 1, max: 2 })
        ));
    }

    #[test]
    fn a_document_without_a_version_is_a_parse_error() {
        assert!(matches!(
            from_json("{}", Path::new("lock.json"), &schemes()),
            Err(IoError::Parse { .. })
        ));
    }

    #[test]
    fn resolved_version_reads_under_the_backend_vers_scheme_not_the_purl_type() {
        // Regression (slice 12-1/12-2 dogfood): a github lock's purl type is
        // `github` (Generic scheme) but its versions compare as semver. The
        // resolved version must parse under the backend's declared vers type
        // (from the schemes map), or `--frozen` reports the locked version
        // excluded by its own spec (cross-scheme `Version` comparison orders by
        // scheme rank, never equal).
        let text = r#"{
  "lockfileVersion": 2,
  "manifest": "/p/bosun.yaml",
  "manifestHash": "sha256:abcd",
  "packages": [
    {
      "name": "ctxloom",
      "spec": "vers:semver/>=0.6.4|<0.7.0",
      "targets": {
        "linux/amd64": {
          "backend": "github-release",
          "purl": "pkg:github/ctxloom/ctxloom?binary=ctxloom",
          "resolved": "0.6.4"
        }
      }
    }
  ]
}"#;
        let doc = from_json(text, Path::new("bosun.lock"), &schemes()).unwrap();
        let pkg = &doc.packages[0];
        let leaf = pkg.targets.values().next().unwrap();
        assert_eq!(leaf.resolved.scheme(), Scheme::Semver);
        assert!(
            pkg.meta.range().contains(&leaf.resolved),
            "the locked version must satisfy its own spec"
        );
    }

    #[test]
    fn an_unknown_backend_is_a_load_error_naming_the_id_and_file() {
        // A leaf naming a backend this build does not know cannot be read: its
        // version scheme is unknown, so fail rather than guess Generic.
        let text = r#"{
  "lockfileVersion": 2,
  "manifest": "/p/bosun.yaml",
  "manifestHash": "sha256:x",
  "packages": [
    {
      "name": "x",
      "spec": "vers:deb/>=1",
      "targets": { "linux/amd64": {
        "backend": "mystery",
        "purl": "pkg:deb/debian/x?arch=amd64",
        "resolved": "1.0"
      } }
    }
  ]
}"#;
        let err = from_json(text, Path::new("bosun.lock"), &schemes()).unwrap_err();
        assert!(matches!(&err, IoError::UnknownBackend { backend, .. } if backend == "mystery"));
        let msg = err.to_string();
        assert!(
            msg.contains("mystery") && msg.contains("bosun.lock"),
            "message names the id and file: {msg}"
        );
    }
}
