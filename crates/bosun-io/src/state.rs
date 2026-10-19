//! Machine-state JSON (§4.6) and the transactional `StateStore` port. Same
//! package shape as the lockfile. A transaction holds an exclusive advisory
//! lock across read-modify-write, so two bosun runs never interleave; the
//! commit writes atomically (temp file in the same dir, fsync, rename) so a
//! reader never sees a torn file. Locking uses std `File::lock` (Rust 1.89+).

use std::collections::BTreeMap;
use std::fs::{File, TryLockError};
use std::path::{Path, PathBuf};
use std::rc::Rc;

use bosun_core::record::{Package, Placement, StateDocument, StateLeaf, StatePackage};
use bosun_core::{BackendId, Claim, Origin, Purl, Scheme, Scope, Version};
use bosun_ports::{FileAccess, LockOpen, PortError, StateStore, StateTransaction};
use serde::{Deserialize, Serialize};

use crate::error::IoError;
use crate::files::DirectFiles;
use crate::BackendSchemes;

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
    /// Absent in state written before bosun recorded its holds, when it held
    /// every package it installed.
    #[serde(default)]
    held: Option<bool>,
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
                                held: Some(leaf.held),
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

    fn into_core(self, schemes: &BackendSchemes, source: &Path) -> Result<StateDocument, IoError> {
        let packages = self
            .packages
            .into_iter()
            .map(|p| {
                let targets = p
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

fn leaf(e: LeafEntry, scheme: Scheme) -> Result<StateLeaf, IoError> {
    let purl: Purl = e.purl.parse().map_err(|err| IoError::Convert {
        detail: format!("purl {:?}: {err}", e.purl),
    })?;
    let installed = Version::parse(&e.installed, scheme).map_err(|err| IoError::Convert {
        detail: format!("version {:?}: {err}", e.installed),
    })?;
    let origin = parse_origin(&e.origin)?;
    Ok(StateLeaf {
        placement: Placement {
            backend: BackendId::new(e.backend),
            purl,
        },
        installed,
        origin,
        held: e.held.unwrap_or(origin == Origin::Installed),
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

/// Parse machine state from JSON text. `schemes` maps each backend to its
/// version scheme (§6), so a leaf's `installed` version parses under the
/// backend's vers type rather than its purl type.
pub fn from_json(
    text: &str,
    source: &Path,
    schemes: &BackendSchemes,
) -> Result<StateDocument, IoError> {
    let doc: StateDoc = serde_json::from_str(text).map_err(|e| IoError::Parse {
        path: source.to_path_buf(),
        detail: e.to_string(),
    })?;
    doc.into_core(schemes, source)
}

/// A [`StateStore`] backed by one JSON file per scope (§4.6). Carries the
/// backend→scheme map (§6) used to read versions under each backend's vers type,
/// and how each scope's files are read and written: the system state by this
/// process, the user state as its user ([`FileStateStore::with_user_files`]).
pub struct FileStateStore {
    system: PathBuf,
    user: PathBuf,
    schemes: BackendSchemes,
    user_files: Rc<dyn FileAccess>,
}

impl FileStateStore {
    /// Construct with explicit per-scope paths (injected by the machine) and the
    /// backend→scheme map for reading versions (§6). Both scopes' files are
    /// this process's own until [`FileStateStore::with_user_files`].
    pub fn new(system: PathBuf, user: PathBuf, schemes: BackendSchemes) -> FileStateStore {
        FileStateStore {
            system,
            user,
            schemes,
            user_files: Rc::new(DirectFiles),
        }
    }

    /// Construct with the default §4.6 locations for this host.
    pub fn with_default_paths(schemes: BackendSchemes) -> Result<FileStateStore, IoError> {
        Ok(FileStateStore::new(
            crate::paths::state_path(Scope::System)?,
            crate::paths::state_path(Scope::User)?,
            schemes,
        ))
    }

    /// Read and write the user state through `files`: as the user who
    /// elevated the run, whose state it is (§4.6).
    pub fn with_user_files(self, files: Rc<dyn FileAccess>) -> FileStateStore {
        FileStateStore {
            user_files: files,
            ..self
        }
    }

    fn path(&self, scope: Scope) -> &Path {
        match scope {
            Scope::System => &self.system,
            Scope::User => &self.user,
        }
    }

    fn files(&self, scope: Scope) -> Rc<dyn FileAccess> {
        match scope {
            Scope::System => Rc::new(DirectFiles),
            Scope::User => self.user_files.clone(),
        }
    }

    /// Open (creating, with its directory) the lock file beside `scope`'s
    /// state, for an exclusive lock.
    fn open_lock(&self, scope: Scope) -> Result<Opened, IoError> {
        let path = self.path(scope).to_path_buf();
        let files = self.files(scope);
        let lock_path = path.with_extension("lock");
        let lock = files
            .open_lock(&lock_path, LockOpen::Exclusive)
            .map_err(|source| IoError::Lock {
                path: lock_path,
                source,
            })?;
        Ok(Opened { path, files, lock })
    }
}

/// A scope's state file, how it is read and written, and its opened lock.
struct Opened {
    path: PathBuf,
    files: Rc<dyn FileAccess>,
    lock: File,
}

impl Opened {
    /// The transaction over the state, once its lock is held.
    fn locked(self, schemes: &BackendSchemes) -> Result<FileStateTxn, IoError> {
        let state = read_or_empty(self.files.as_ref(), &self.path, schemes)?;
        Ok(FileStateTxn {
            path: self.path,
            files: self.files,
            lock: self.lock,
            state,
        })
    }
}

impl StateStore for FileStateStore {
    type Transaction = FileStateTxn;

    fn begin(&self, scope: Scope) -> Result<FileStateTxn, PortError> {
        let opened = self.open_lock(scope)?;
        opened.lock.lock().map_err(|source| IoError::Lock {
            path: opened.path.with_extension("lock"),
            source,
        })?;
        Ok(opened.locked(&self.schemes)?)
    }

    fn try_begin(&self, scope: Scope) -> Result<Option<FileStateTxn>, PortError> {
        let opened = self.open_lock(scope)?;
        match opened.lock.try_lock() {
            Ok(()) => Ok(Some(opened.locked(&self.schemes)?)),
            Err(TryLockError::WouldBlock) => Ok(None),
            Err(TryLockError::Error(source)) => Err(IoError::Lock {
                path: opened.path.with_extension("lock"),
                source,
            }
            .into()),
        }
    }

    fn read(&self, scope: Scope) -> Result<StateDocument, PortError> {
        let path = self.path(scope);
        let files = self.files(scope);
        match files.exists(path) {
            Ok(true) => {}
            Ok(false) => return Ok(empty_state()),
            Err(source) => return Err(read_error(path, source).into()),
        }
        let lock_path = path.with_extension("lock");
        let lock = open_for_shared_lock(files.as_ref(), &lock_path)?;
        lock.lock_shared().map_err(|source| IoError::Lock {
            path: lock_path,
            source,
        })?;
        Ok(read_or_empty(files.as_ref(), path, &self.schemes)?)
    }
}

/// Open the lock file for a shared lock: read-only, since an elevated run
/// leaves it writable only by root while every user may read the state
/// (§4.6). Created only when missing.
fn open_for_shared_lock(files: &dyn FileAccess, lock_path: &Path) -> Result<File, IoError> {
    files
        .open_lock(lock_path, LockOpen::Shared)
        .map_err(|source| match source.kind() {
            std::io::ErrorKind::PermissionDenied => IoError::Denied {
                path: lock_path.to_path_buf(),
            },
            _ => IoError::Lock {
                path: lock_path.to_path_buf(),
                source,
            },
        })
}

/// A failure reading `path`: [`IoError::Denied`] when this user may not.
fn read_error(path: &Path, source: std::io::Error) -> IoError {
    match source.kind() {
        std::io::ErrorKind::PermissionDenied => IoError::Denied {
            path: path.to_path_buf(),
        },
        _ => IoError::Read {
            path: path.to_path_buf(),
            source,
        },
    }
}

/// A held read-modify-write transaction (§4.6): the exclusive lock, the path,
/// how it is written, and the state loaded when it began.
pub struct FileStateTxn {
    path: PathBuf,
    files: Rc<dyn FileAccess>,
    lock: File,
    state: StateDocument,
}

impl FileStateTxn {
    /// Atomically replace the state file with `state`, through its files.
    fn save(&self, state: &StateDocument) -> Result<(), IoError> {
        self.files
            .write(&self.path, to_json(state).as_bytes())
            .map_err(|source| IoError::Write {
                path: self.path.clone(),
                source,
            })
    }
}

impl StateTransaction for FileStateTxn {
    fn state(&self) -> &StateDocument {
        &self.state
    }

    fn checkpoint(&mut self, state: &StateDocument) -> Result<(), PortError> {
        // Atomic write; the lock stays held so the next action continues the
        // same transaction (Appendix B crash-safety).
        self.save(state)?;
        Ok(())
    }

    fn commit(self, state: StateDocument) -> Result<(), PortError> {
        // The lock is still held across the atomic write; released below.
        self.save(&state)?;
        drop(self.lock);
        Ok(())
    }
}

fn empty_state() -> StateDocument {
    StateDocument {
        state_version: STATE_VERSION,
        packages: Vec::new(),
    }
}

fn read_or_empty(
    files: &dyn FileAccess,
    path: &Path,
    schemes: &BackendSchemes,
) -> Result<StateDocument, IoError> {
    match files.read(path) {
        Ok(bytes) => {
            let text = String::from_utf8(bytes).map_err(|e| IoError::Parse {
                path: path.to_path_buf(),
                detail: e.to_string(),
            })?;
            from_json(&text, path, schemes)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(empty_state()),
        Err(source) => Err(read_error(path, source)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bosun_core::Scheme;
    use std::fs;

    /// The sample records an apt (deb) package, so its version reads as Debian.
    fn schemes() -> BackendSchemes {
        BackendSchemes::from([("apt".to_string(), Scheme::Debian)])
    }

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
                        held: true,
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
          "held": true,
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
        let back = from_json(&to_json(&sample()), Path::new("state.json"), &schemes()).unwrap();
        assert_eq!(back, sample());
    }

    #[test]
    fn roundtrips_the_accepted_origin() {
        let mut doc = sample();
        doc.packages[0].targets.values_mut().next().unwrap().origin = Origin::Accepted;
        let back = from_json(&to_json(&doc), Path::new("state.json"), &schemes()).unwrap();
        assert_eq!(
            back.packages[0].targets.values().next().unwrap().origin,
            Origin::Accepted
        );
    }

    #[test]
    fn roundtrips_an_unheld_package() {
        let mut doc = sample();
        doc.packages[0].targets.values_mut().next().unwrap().held = false;
        let back = from_json(&to_json(&doc), Path::new("state.json"), &schemes()).unwrap();
        assert_eq!(back, doc);
    }

    #[test]
    fn state_without_holds_recorded_reads_installed_packages_as_held() {
        // bosun once held everything it installed and recorded no hold, so a
        // package it installed is read as held, one it accepted as not.
        let leaf = |origin: &str| {
            let text = format!(
                r#"{{"stateVersion":1,"packages":[{{"name":"jq","targets":{{"linux/amd64":{{
                "backend":"apt","purl":"pkg:deb/debian/jq?arch=amd64","installed":"1.7.1-3","origin":"{origin}","claimedBy":[]}}}}}}]}}"#
            );
            let doc = from_json(&text, Path::new("state.json"), &schemes()).unwrap();
            doc.packages[0].targets.values().next().unwrap().held
        };
        assert!(leaf("installed"));
        assert!(!leaf("accepted"));
    }

    #[test]
    fn an_unknown_backend_is_a_load_error_naming_the_id_and_file() {
        // A leaf for a backend this build does not know cannot be read: its
        // version scheme is unknown, so fail rather than guess Generic.
        let text = r#"{"stateVersion":1,"packages":[{"name":"x","targets":{"linux/amd64":{
            "backend":"mystery","purl":"pkg:deb/debian/x?arch=amd64","installed":"1.0","origin":"installed","claimedBy":[]}}}]}"#;
        let err = from_json(text, Path::new("state.json"), &schemes()).unwrap_err();
        assert!(matches!(&err, IoError::UnknownBackend { backend, .. } if backend == "mystery"));
        let msg = err.to_string();
        assert!(
            msg.contains("mystery") && msg.contains("state.json"),
            "message names the id and file: {msg}"
        );
    }

    #[test]
    fn a_known_backend_reads_its_version_under_its_own_scheme() {
        // The map carries apt's scheme even when apt isn't present this run; its
        // state entry reads as Debian, not Generic.
        let text = r#"{"stateVersion":1,"packages":[{"name":"jq","targets":{"linux/amd64":{
            "backend":"apt","purl":"pkg:deb/debian/jq?arch=amd64","installed":"1.7.1-3","origin":"installed","claimedBy":[]}}}]}"#;
        let doc = from_json(text, Path::new("state.json"), &schemes()).unwrap();
        let leaf = doc.packages[0].targets.values().next().unwrap();
        assert_eq!(leaf.installed.scheme(), Scheme::Debian);
    }

    #[test]
    fn read_surfaces_an_error_that_is_not_a_missing_file() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("state-is-a-dir");
        std::fs::create_dir(&dir).unwrap();
        // Reading a directory errors with a kind other than NotFound, so it
        // must surface rather than be treated as "empty state".
        assert!(read_or_empty(&DirectFiles, &dir, &schemes()).is_err());
    }

    fn store(tmp: &tempfile::TempDir) -> FileStateStore {
        FileStateStore::new(
            tmp.path().join("sys/state.json"),
            tmp.path().join("user/state.json"),
            schemes(),
        )
    }

    #[test]
    fn begin_commit_then_read_roundtrips_and_creates_dirs() {
        let tmp = tempfile::tempdir().unwrap();
        let store = store(&tmp);
        let txn = store.begin(Scope::User).unwrap();
        assert!(txn.state().packages.is_empty()); // nothing recorded yet
        txn.commit(sample()).unwrap();
        assert_eq!(store.read(Scope::User).unwrap(), sample());
    }

    #[test]
    fn checkpoint_persists_without_releasing_the_lock() {
        let tmp = tempfile::tempdir().unwrap();
        let store = store(&tmp);
        let mut txn = store.begin(Scope::User).unwrap();
        txn.checkpoint(&sample()).unwrap();
        // Persisted and readable mid-transaction (a crash now keeps this).
        assert_eq!(
            read_or_empty(&DirectFiles, store.path(Scope::User), &schemes()).unwrap(),
            sample()
        );
        // The lock is still held, so another run still backs off.
        assert!(
            store.try_begin(Scope::User).unwrap().is_none(),
            "lock held across checkpoint"
        );
        // Commit still writes and releases.
        txn.commit(sample()).unwrap();
        assert!(store.try_begin(Scope::User).unwrap().is_some());
    }

    #[test]
    fn read_missing_is_empty_document() {
        let tmp = tempfile::tempdir().unwrap();
        let doc = store(&tmp).read(Scope::User).unwrap();
        assert_eq!(doc.state_version, STATE_VERSION);
        assert!(doc.packages.is_empty());
    }

    #[test]
    fn a_second_transaction_cannot_begin_while_the_first_is_held() {
        let tmp = tempfile::tempdir().unwrap();
        let store = store(&tmp);
        let txn1 = store.begin(Scope::User).unwrap();
        // While txn1 holds the exclusive lock, a non-blocking begin backs off.
        assert!(store.try_begin(Scope::User).unwrap().is_none());
        // Committing releases the lock; the next transaction can begin and
        // sees txn1's write (no lost claim).
        txn1.commit(sample()).unwrap();
        let txn2 = store.try_begin(Scope::User).unwrap().expect("lock is free");
        assert_eq!(txn2.state(), &sample());
    }

    /// Whether file modes bind this process: not for root, which reads and
    /// writes regardless.
    #[cfg(unix)]
    fn modes_bind(dir: &Path) -> bool {
        use std::os::unix::fs::PermissionsExt;
        let probe = dir.join("probe");
        fs::write(&probe, "x").unwrap();
        fs::set_permissions(&probe, fs::Permissions::from_mode(0o000)).unwrap();
        let binds = fs::read(&probe).is_err();
        fs::remove_file(&probe).unwrap();
        binds
    }

    /// The user state committed, then its lock and state file set to `lock`
    /// and `state` modes, as an elevated run leaves the system state.
    #[cfg(unix)]
    fn committed_with_modes(tmp: &tempfile::TempDir, lock: u32, state: u32) -> FileStateStore {
        use std::os::unix::fs::PermissionsExt;
        let store = store(tmp);
        store.begin(Scope::User).unwrap().commit(sample()).unwrap();
        let path = store.path(Scope::User).to_path_buf();
        fs::set_permissions(
            path.with_extension("lock"),
            fs::Permissions::from_mode(lock),
        )
        .unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(state)).unwrap();
        store
    }

    #[cfg(unix)]
    #[test]
    fn read_takes_its_shared_lock_through_a_lock_file_it_may_not_write() {
        let tmp = tempfile::tempdir().unwrap();
        if !modes_bind(tmp.path()) {
            return;
        }
        let store = committed_with_modes(&tmp, 0o444, 0o444);
        assert_eq!(store.read(Scope::User).unwrap(), sample());
    }

    #[cfg(unix)]
    #[test]
    fn read_of_state_this_user_may_not_read_is_denied_naming_the_file() {
        let tmp = tempfile::tempdir().unwrap();
        if !modes_bind(tmp.path()) {
            return;
        }
        let store = committed_with_modes(&tmp, 0o444, 0o000);
        let path = store.path(Scope::User).display().to_string();
        assert_eq!(
            store.read(Scope::User).unwrap_err(),
            PortError::StateDenied { path }
        );
    }

    #[cfg(unix)]
    #[test]
    fn read_of_a_lock_this_user_may_not_open_is_denied_naming_it() {
        let tmp = tempfile::tempdir().unwrap();
        if !modes_bind(tmp.path()) {
            return;
        }
        let store = committed_with_modes(&tmp, 0o000, 0o444);
        let path = store
            .path(Scope::User)
            .with_extension("lock")
            .display()
            .to_string();
        assert_eq!(
            store.read(Scope::User).unwrap_err(),
            PortError::StateDenied { path }
        );
    }

    #[cfg(unix)]
    #[test]
    fn read_inside_a_directory_this_user_may_not_search_is_denied() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        if !modes_bind(tmp.path()) {
            return;
        }
        let store = committed_with_modes(&tmp, 0o644, 0o644);
        let dir = store.path(Scope::User).parent().unwrap().to_path_buf();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o000)).unwrap();
        let err = store.read(Scope::User).unwrap_err();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(matches!(err, PortError::StateDenied { .. }), "{err}");
    }

    #[test]
    fn read_creates_a_missing_lock_beside_existing_state() {
        let tmp = tempfile::tempdir().unwrap();
        let store = store(&tmp);
        store.begin(Scope::User).unwrap().commit(sample()).unwrap();
        let lock = store.path(Scope::User).with_extension("lock");
        fs::remove_file(&lock).unwrap();
        assert_eq!(store.read(Scope::User).unwrap(), sample());
        assert!(lock.exists());
    }

    /// File access that does this process's I/O and notes each operation.
    #[derive(Default)]
    struct Noted(std::cell::RefCell<Vec<String>>);

    impl Noted {
        fn note(&self, op: &str, path: &Path) {
            let name = path.file_name().unwrap().to_string_lossy();
            self.0.borrow_mut().push(format!("{op} {name}"));
        }
    }

    impl FileAccess for Noted {
        fn exists(&self, path: &Path) -> std::io::Result<bool> {
            self.note("exists", path);
            DirectFiles.exists(path)
        }
        fn read(&self, path: &Path) -> std::io::Result<Vec<u8>> {
            self.note("read", path);
            DirectFiles.read(path)
        }
        fn write(&self, path: &Path, bytes: &[u8]) -> std::io::Result<()> {
            self.note("write", path);
            DirectFiles.write(path, bytes)
        }
        fn open_lock(&self, path: &Path, how: LockOpen) -> std::io::Result<File> {
            self.note(&format!("{how:?}"), path);
            DirectFiles.open_lock(path, how)
        }
    }

    #[test]
    fn the_user_state_goes_through_its_files_and_the_system_state_does_not() {
        let tmp = tempfile::tempdir().unwrap();
        let noted = Rc::new(Noted::default());
        let store = store(&tmp).with_user_files(noted.clone());
        store
            .begin(Scope::System)
            .unwrap()
            .commit(sample())
            .unwrap();
        assert_eq!(store.read(Scope::System).unwrap(), sample());
        assert!(noted.0.borrow().is_empty(), "{:?}", noted.0.borrow());
        let mut txn = store.begin(Scope::User).unwrap();
        txn.checkpoint(&sample()).unwrap();
        txn.commit(sample()).unwrap();
        assert!(store.try_begin(Scope::User).unwrap().is_some());
        assert_eq!(store.read(Scope::User).unwrap(), sample());
        assert_eq!(
            *noted.0.borrow(),
            [
                "Exclusive state.lock",
                "read state.json",
                "write state.json",
                "write state.json",
                "Exclusive state.lock",
                "read state.json",
                "exists state.json",
                "Shared state.lock",
                "read state.json",
            ]
        );
    }

    #[test]
    fn a_state_file_that_is_not_text_is_a_parse_error_naming_it() {
        let tmp = tempfile::tempdir().unwrap();
        let store = store(&tmp);
        let path = store.path(Scope::User).to_path_buf();
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, [0xff, 0xfe]).unwrap();
        let err = store.read(Scope::User).unwrap_err().to_string();
        assert!(err.contains("state.json"), "{err}");
        assert!(err.contains(crate::errmsg::PARSE), "{err}");
    }
}
