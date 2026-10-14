//! Machine-state JSON (§4.6) and the `StateStore` port. Same package shape as
//! the lockfile. Writes are atomic (temp file in the same dir, fsync, rename)
//! and serialized by an advisory lock, so two bosun runs never interleave or
//! leave a torn file.

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};

use bosun_core::record::{Package, Placement, StateDocument, StateLeaf, StatePackage};
use bosun_core::vers::scheme_for_type;
use bosun_core::{BackendId, Claim, Origin, Purl, Scope, Version};
use bosun_ports::{PortError, StateStore};
use fs2::FileExt;
use serde::{Deserialize, Serialize};

use crate::error::IoError;

/// The newest machine-state format version this build reads and writes.
pub const STATE_VERSION: u32 = 1;

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct StateDoc {
    state_version: u32,
    packages: Vec<StatePkg>,
}

#[derive(Serialize, Deserialize)]
struct StatePkg {
    name: String,
    targets: BTreeMap<String, LeafEntry>,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct LeafEntry {
    backend: String,
    purl: String,
    installed: String,
    origin: String,
    claimed_by: Vec<ClaimEntry>,
}

#[derive(Serialize, Deserialize)]
struct ClaimEntry {
    manifest: String,
    spec: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    user: Option<String>,
}

fn origin_str(o: Origin) -> &'static str {
    match o {
        Origin::Installed => "installed",
        Origin::Accepted => "accepted",
    }
}

fn parse_origin(s: &str) -> Result<Origin, IoError> {
    match s {
        "installed" => Ok(Origin::Installed),
        "accepted" => Ok(Origin::Accepted),
        _ => Err(IoError::Convert {
            detail: format!("origin {s:?}"),
        }),
    }
}

impl StateDoc {
    fn from_core(doc: &StateDocument) -> StateDoc {
        let mut packages: Vec<StatePkg> = doc
            .packages
            .iter()
            .map(|p| StatePkg {
                name: p.name.clone(),
                targets: p
                    .targets
                    .iter()
                    .map(|(t, leaf)| {
                        (
                            t.to_string(),
                            LeafEntry {
                                backend: leaf.placement.backend.as_str().to_string(),
                                purl: leaf.placement.purl.to_string(),
                                installed: leaf.installed.as_str().to_string(),
                                origin: origin_str(leaf.origin).to_string(),
                                claimed_by: leaf
                                    .claimed_by
                                    .iter()
                                    .map(|c| ClaimEntry {
                                        manifest: c.manifest.display().to_string(),
                                        spec: c.spec.clone(),
                                        user: c.user.clone(),
                                    })
                                    .collect(),
                            },
                        )
                    })
                    .collect(),
            })
            .collect();
        packages.sort_by(|a, b| a.name.cmp(&b.name));
        StateDoc {
            state_version: doc.state_version,
            packages,
        }
    }

    fn into_core(self) -> Result<StateDocument, IoError> {
        let packages = self
            .packages
            .into_iter()
            .map(|p| {
                let targets = p
                    .targets
                    .into_iter()
                    .map(|(t, e)| Ok((parse_target(&t)?, leaf(e)?)))
                    .collect::<Result<_, IoError>>()?;
                Ok(Package {
                    name: p.name,
                    meta: (),
                    targets,
                })
            })
            .collect::<Result<Vec<StatePackage>, IoError>>()?;
        Ok(StateDocument {
            state_version: self.state_version,
            packages,
        })
    }
}

fn leaf(e: LeafEntry) -> Result<StateLeaf, IoError> {
    let purl: Purl = e.purl.parse().map_err(|err| IoError::Convert {
        detail: format!("purl {:?}: {err}", e.purl),
    })?;
    let installed = Version::parse(&e.installed, scheme_for_type(purl.ty())).map_err(|err| {
        IoError::Convert {
            detail: format!("version {:?}: {err}", e.installed),
        }
    })?;
    Ok(StateLeaf {
        placement: Placement {
            backend: BackendId::new(e.backend),
            purl,
        },
        installed,
        origin: parse_origin(&e.origin)?,
        claimed_by: e
            .claimed_by
            .into_iter()
            .map(|c| Claim {
                manifest: PathBuf::from(c.manifest),
                spec: c.spec,
                user: c.user,
            })
            .collect(),
    })
}

fn parse_target(t: &str) -> Result<bosun_core::Target, IoError> {
    t.parse().map_err(|err| IoError::Convert {
        detail: format!("target {t:?}: {err}"),
    })
}

/// Serialize machine state to canonical JSON text.
pub fn to_json(doc: &StateDocument) -> String {
    serde_json::to_string_pretty(&StateDoc::from_core(doc))
        .expect("state serialization is infallible")
}

/// Parse machine state from JSON text.
pub fn from_json(text: &str, source: &Path) -> Result<StateDocument, IoError> {
    let doc: StateDoc = serde_json::from_str(text).map_err(|e| IoError::Parse {
        path: source.to_path_buf(),
        detail: e.to_string(),
    })?;
    doc.into_core()
}

/// A [`StateStore`] backed by one JSON file per scope (§4.6).
pub struct FileStateStore {
    system: PathBuf,
    user: PathBuf,
}

impl FileStateStore {
    /// Construct with explicit per-scope paths (injected by the machine).
    pub fn new(system: PathBuf, user: PathBuf) -> FileStateStore {
        FileStateStore { system, user }
    }

    /// Construct with the default §4.6 locations for this host.
    pub fn with_default_paths() -> Result<FileStateStore, IoError> {
        Ok(FileStateStore {
            system: crate::paths::state_path(Scope::System)?,
            user: crate::paths::state_path(Scope::User)?,
        })
    }

    fn path(&self, scope: Scope) -> &Path {
        match scope {
            Scope::System => &self.system,
            Scope::User => &self.user,
        }
    }
}

impl StateStore for FileStateStore {
    fn load(&self, scope: Scope) -> Result<StateDocument, PortError> {
        let path = self.path(scope);
        match fs::read_to_string(path) {
            Ok(text) => Ok(from_json(&text, path)?),
            // A machine with nothing recorded yet is an empty document.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(StateDocument {
                state_version: STATE_VERSION,
                packages: Vec::new(),
            }),
            Err(source) => Err(IoError::Read {
                path: path.to_path_buf(),
                source,
            }
            .into()),
        }
    }

    fn save(&self, scope: Scope, state: &StateDocument) -> Result<(), PortError> {
        let path = self.path(scope).to_path_buf();
        save_atomic(&path, to_json(state).as_bytes())?;
        Ok(())
    }
}

/// Write `bytes` to `path` atomically, under an exclusive advisory lock so
/// concurrent bosun runs serialize rather than interleave.
fn save_atomic(path: &Path, bytes: &[u8]) -> Result<(), IoError> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(dir).map_err(|source| IoError::Write {
        path: dir.to_path_buf(),
        source,
    })?;

    // Hold an exclusive lock on a sibling file for the whole write. fs2's
    // advisory lock releases when the file closes (process exit included), so
    // there is no stale lock to clean up.
    let lock_path = path.with_extension("lock");
    let lockf = File::create(&lock_path).map_err(|source| IoError::Lock {
        path: lock_path.clone(),
        source,
    })?;
    lockf.lock_exclusive().map_err(|source| IoError::Lock {
        path: lock_path.clone(),
        source,
    })?;

    let write = |p: &Path, src| IoError::Write {
        path: p.to_path_buf(),
        source: src,
    };
    let tmp = path.with_extension("json.tmp");
    let mut f = File::create(&tmp).map_err(|e| write(&tmp, e))?;
    f.write_all(bytes).map_err(|e| write(&tmp, e))?;
    f.sync_all().map_err(|e| write(&tmp, e))?;
    drop(f);
    fs::rename(&tmp, path).map_err(|e| write(path, e))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use bosun_core::Scheme;

    fn sample() -> StateDocument {
        let t: bosun_core::Target = "linux/amd64".parse().unwrap();
        StateDocument {
            state_version: 1,
            packages: vec![Package {
                name: "jq".into(),
                meta: (),
                targets: BTreeMap::from([(
                    t,
                    StateLeaf {
                        placement: Placement {
                            backend: BackendId::new("apt"),
                            purl: "pkg:deb/ubuntu/jq?arch=amd64".parse().unwrap(),
                        },
                        installed: Version::parse("1.7.1-3ubuntu0.1", Scheme::Debian).unwrap(),
                        origin: Origin::Installed,
                        claimed_by: vec![Claim {
                            manifest: PathBuf::from("/w/verprop/bosun.yaml"),
                            spec: "vers:deb/>=1.7.1|<1.7.2~".into(),
                            user: None,
                        }],
                    },
                )]),
            }],
        }
    }

    const GOLDEN: &str = r#"{
  "stateVersion": 1,
  "packages": [
    {
      "name": "jq",
      "targets": {
        "linux/amd64": {
          "backend": "apt",
          "purl": "pkg:deb/ubuntu/jq?arch=amd64",
          "installed": "1.7.1-3ubuntu0.1",
          "origin": "installed",
          "claimedBy": [
            {
              "manifest": "/w/verprop/bosun.yaml",
              "spec": "vers:deb/>=1.7.1|<1.7.2~"
            }
          ]
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
        let back = from_json(&to_json(&sample()), Path::new("state.json")).unwrap();
        assert_eq!(back, sample());
    }

    #[test]
    fn store_save_then_load_roundtrips_and_creates_dirs() {
        let tmp = tempfile::tempdir().unwrap();
        let store = FileStateStore::new(
            tmp.path().join("sys/state.json"),
            tmp.path().join("user/state.json"),
        );
        store.save(Scope::User, &sample()).unwrap();
        assert_eq!(store.load(Scope::User).unwrap(), sample());
    }

    #[test]
    fn load_missing_is_empty_document() {
        let tmp = tempfile::tempdir().unwrap();
        let store =
            FileStateStore::new(tmp.path().join("sys.json"), tmp.path().join("absent.json"));
        let doc = store.load(Scope::User).unwrap();
        assert_eq!(doc.state_version, STATE_VERSION);
        assert!(doc.packages.is_empty());
    }
}
