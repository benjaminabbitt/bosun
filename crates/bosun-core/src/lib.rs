//! bosun domain model: versions and their ordering (§6), resolution policies
//! (§7), platform identity, and the manifest/lock/state value types (§4.5,
//! §4.6). Pure — no I/O, no process calls.

pub mod errmsg;
pub mod model;
pub mod platform;
pub mod policy;
pub mod record;
pub mod version;

pub use model::{
    BackendId, Capabilities, Checksum, Claim, Installed, Origin, PackageId, Ref, Scope, Source,
    SourceId,
};
pub use platform::{Arch, Os, PlatformError, Selector, Target};
pub use policy::{Deviation, Entry, Policy, Resolution, ResolveError, Spec, SpecError};
pub use record::{
    LockDocument, LockLeaf, LockPackage, Package, Placement, StateDocument, StateLeaf,
    StatePackage, TargetLock,
};
pub use version::{Scheme, Segment, Version, VersionError};
