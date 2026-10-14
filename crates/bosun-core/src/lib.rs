//! bosun domain model: versions and their ordering (§6), resolution policies
//! (§7), and the manifest/lock value types. Pure — no I/O, no process calls.

pub mod version;

pub use version::{Scheme, Segment, Version, VersionError};
