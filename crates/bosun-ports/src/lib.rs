//! bosun ports (hexagonal boundaries): the traits adapters implement, and the
//! small value types those traits speak in. No concrete backends, no I/O, no
//! OS branching — those live in bosun-backends, bosun-io, and bosun-host.
//!
//! - [`Index`] / [`Installer`] — the backend contract, query/mutation split
//!   (§5.1); [`BackendFactory`] registers a present backend (§5.3).
//! - [`Machine`] — the OS interface (§5.5).
//! - [`StateStore`] — machine-state persistence (§4.6).
//! - [`CommandRunner`] / [`HttpFetcher`] — the side-effect ports a backend
//!   runs through, with fakes in [`testing`].
//!
//! `Driver` (§4.1, §8) is intentionally not here yet: its shape depends on the
//! plan type built in bosun-apply, so it is defined alongside the host applier.
//!
//! Every fallible port returns [`PortError`] (never `anyhow`).

pub mod backend;
pub mod errmsg;
pub mod error;
pub mod exec;
pub mod machine;
pub mod state;
pub mod step;
pub mod testing;

pub use backend::{Backend, BackendFactory, BackendMeta, HostInfo, Index, Installer, Probe};
pub use error::PortError;
pub use exec::{Command, CommandRunner, HttpFetcher, HttpRequest, HttpResponse, Output};
pub use machine::{Elevation, Machine, Privilege, User};
pub use state::{StateStore, StateTransaction};
pub use step::{ArchiveFormat, Step, StepRunner, Steps};
