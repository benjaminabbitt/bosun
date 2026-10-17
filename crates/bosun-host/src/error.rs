//! bosun-host's error type (thiserror).

use crate::errmsg;

/// A failure detecting the host.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum HostError {
    /// The running architecture is not one bosun maps to a target (§5.5).
    #[error("{kind}: {0}", kind = errmsg::UNKNOWN_ARCH)]
    UnknownArch(String),
    /// The machine's architecture could not be read (`uname -m` failed, or
    /// Windows names no processor).
    #[error("{kind}: {0}", kind = errmsg::ARCH_UNREAD)]
    ArchUnread(String),
}
