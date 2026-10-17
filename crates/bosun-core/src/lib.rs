//! bosun domain model: versions and their ordering (§6), vers ranges and
//! selection (§5.6, §7), platform identity, and the manifest/lock/state value
//! types (§4.5, §4.6). Pure — no I/O, no process calls.

pub mod errmsg;
pub mod manifest;
pub mod model;
pub mod native;
pub mod platform;
pub mod purl;
pub mod record;
pub mod spec;
pub mod vers;
pub mod version;
pub mod want;

pub use manifest::{Backends, InstallBlock, Manifest, ManifestEntry, Options, PACKAGE_OPTION};
pub use model::{
    BackendId, Capabilities, Checksum, Claim, HoldSupport, Installed, Origin, Ref, Scope, Source,
    SourceId,
};
pub use platform::{
    Arch, Distro, Libc, LibcFamily, LibcRange, Os, PlatformError, Selector, Target,
};
pub use purl::{Purl, PurlError};
pub use record::{
    LockDocument, LockLeaf, LockPackage, Package, Placement, StateDocument, StateLeaf, StatePackage,
};
pub use spec::{default_spec, Entry, SelectError, Spec, SpecError};
pub use vers::{VersConstraint, VersError, VersOp, VersRange};
pub use version::{Scheme, Segment, Version, VersionError};
pub use want::Want;
