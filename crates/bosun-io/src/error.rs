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
        bosun_ports::PortError::StateIo {
            detail: e.to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
