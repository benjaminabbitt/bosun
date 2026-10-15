//! bosun domain model: versions and their ordering (§6), vers ranges and
//! selection (§5.6, §7), platform identity, and the manifest/lock/state value
//! types (§4.5, §4.6). Pure — no I/O, no process calls.

pub mod errmsg;
pub mod manifest;
pub mod model;
pub mod platform;
pub mod purl;
pub mod record;
pub mod spec;
pub mod vers;
pub mod version;

pub use manifest::{Ext, InstallBlock, Manifest, ManifestEntry};
pub use model::{
    BackendId, Capabilities, Checksum, Claim, HoldSupport, Installed, Origin, Ref, Scope, Source,
    SourceId,
};
pub use platform::{Arch, Distro, Os, PlatformError, Selector, Target};
pub use purl::{Purl, PurlError};
pub use record::{
    LockDocument, LockLeaf, LockPackage, Package, Placement, StateDocument, StateLeaf,
    StatePackage, TargetLock,
};
pub use spec::{Entry, SelectError, Spec, SpecError};
pub use vers::{VersConstraint, VersError, VersOp, VersRange};
pub use version::{Scheme, Segment, Version, VersionError};
