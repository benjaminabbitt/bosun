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
}

impl From<IoError> for bosun_ports::PortError {
    fn from(e: IoError) -> bosun_ports::PortError {
        bosun_ports::PortError::StateIo {
            detail: e.to_string(),
        }
    }
}
