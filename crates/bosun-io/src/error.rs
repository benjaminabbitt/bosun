//! bosun-io's error type (thiserror, no anyhow). The `StateStore` port maps
//! these onto `bosun_ports::PortError`.

use std::path::PathBuf;

use crate::errmsg;

/// A failure reading, parsing, or writing one of bosun's files.
#[derive(Debug, thiserror::Error)]
pub enum IoError {
    /// A file could not be read.
    #[error("{kind}: {path} ({source})", kind = errmsg::READ)]
    Read {
        /// The file.
        path: PathBuf,
        /// The underlying I/O error.
        source: std::io::Error,
    },
    /// A file could not be written.
    #[error("{kind}: {path} ({source})", kind = errmsg::WRITE)]
    Write {
        /// The file.
        path: PathBuf,
        /// The underlying I/O error.
        source: std::io::Error,
    },
    /// A file's contents could not be parsed.
    #[error("{kind}: {path} ({detail})", kind = errmsg::PARSE)]
    Parse {
        /// The file.
        path: PathBuf,
        /// What went wrong.
        detail: String,
    },
    /// The machine-state lock could not be acquired.
    #[error("{kind}: {path} ({source})", kind = errmsg::LOCK)]
    Lock {
        /// The lock file.
        path: PathBuf,
        /// The underlying I/O error.
        source: std::io::Error,
    },
    /// This user may not read a scope's machine state (§4.6).
    #[error("{kind}: {path}", kind = bosun_ports::errmsg::STATE_DENIED)]
    Denied {
        /// The state or lock file that could not be opened.
        path: PathBuf,
    },
    /// A lockfile's major version is newer than this build can read.
    #[error("{kind}: found {found}, this build reads {max}", kind = errmsg::LOCKFILE_VERSION)]
    LockfileVersion {
        /// The version found.
        found: u32,
        /// The newest version this build reads.
        max: u32,
    },
    /// A recorded string could not be converted to the domain model.
    #[error("{kind}: {detail}", kind = errmsg::CONVERT)]
    Convert {
        /// What could not be converted.
        detail: String,
    },
    /// `bosun add` named a package the manifest already has.
    #[error("{kind} {name:?}: {path}", kind = errmsg::DUPLICATE_ENTRY)]
    DuplicateEntry {
        /// The manifest.
        path: PathBuf,
        /// The package name.
        name: String,
    },
    /// `bosun remove` named a package the manifest does not have.
    #[error("{kind} {name:?}: {path}", kind = errmsg::NO_SUCH_ENTRY)]
    NoSuchEntry {
        /// The manifest.
        path: PathBuf,
        /// The package name.
        name: String,
    },
    /// `bosun add` / `remove` on a manifest older than this build writes.
    #[error(
        "{path}: {kind} {found}, but this build writes version {current}; run `bosun migrate` \
         first",
        kind = errmsg::MANIFEST_OUTDATED
    )]
    ManifestOutdated {
        /// The manifest.
        path: PathBuf,
        /// Its `version`.
        found: u32,
        /// The version this build writes.
        current: u32,
    },
    /// The manifest could not be edited in place.
    #[error("{kind}: {path} ({detail})", kind = errmsg::EDIT)]
    Edit {
        /// The manifest.
        path: PathBuf,
        /// What went wrong.
        detail: String,
    },
    /// A lock/state leaf names a backend this build does not know, so its
    /// version scheme cannot be determined (§6).
    #[error("{kind}: {backend:?} in {path}", kind = errmsg::UNKNOWN_BACKEND)]
    UnknownBackend {
        /// The file the leaf came from.
        path: PathBuf,
        /// The unrecognised backend id.
        backend: String,
    },
}

impl From<IoError> for bosun_ports::PortError {
    fn from(e: IoError) -> bosun_ports::PortError {
        match e {
            IoError::Denied { path } => bosun_ports::PortError::StateDenied {
                path: path.display().to_string(),
            },
            e => bosun_ports::PortError::StateIo {
                detail: e.to_string(),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn denied_maps_to_its_own_port_error_naming_the_file() {
        let port: bosun_ports::PortError = IoError::Denied {
            path: PathBuf::from("/var/lib/bosun/state.json"),
        }
        .into();
        assert_eq!(
            port,
            bosun_ports::PortError::StateDenied {
                path: "/var/lib/bosun/state.json".to_string()
            }
        );
    }

    #[test]
    fn maps_to_a_port_error_carrying_its_message() {
        let e = IoError::Convert {
            detail: "origin \"bogus\"".to_string(),
        };
        let message = e.to_string();
        let port: bosun_ports::PortError = e.into();
        assert!(matches!(port, bosun_ports::PortError::StateIo { detail } if detail == message));
    }
}
