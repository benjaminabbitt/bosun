//! bosun host: detect OS, architecture, distro family and libc (§5.4), report
//! privilege and the invoking user (§5.5), and probe each backend to build the
//! registry of backends present on this machine (§5.3). `LinuxMachine` is
//! real; `FakeMachine` drives tests. macOS/Windows are out of M1 but the
//! `Machine` trait keeps room for them.

#[cfg(unix)]
pub mod as_user;
pub mod errmsg;
pub mod error;
pub mod executor;
pub mod http;
pub mod invoker;
pub mod libc;
pub mod logmsg;
pub mod machine;
pub mod os_release;
pub mod process;
pub mod registry;

#[cfg(unix)]
pub use as_user::AsUser;
pub use error::HostError;
pub use executor::StepExecutor;
pub use http::UreqHttpFetcher;
#[cfg(unix)]
pub use invoker::passwd_account;
pub use invoker::{invoker, Account, Invoker, UnknownUser};
#[cfg(unix)]
pub use machine::LinuxMachine;
pub use machine::{
    detect_arch, privilege_from_whoami, windows_privilege, FakeMachine, WindowsMachine,
};
pub use process::ProcessCommandRunner;
pub use registry::{build_registry, RegisteredBackend, Registry, SkipReason, SkippedBackend};
