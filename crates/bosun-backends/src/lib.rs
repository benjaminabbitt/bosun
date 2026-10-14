//! bosun backend adapters. M1 ships apt, cargo and github-release (§12);
//! the trait surface in `bosun-ports` keeps room for the rest (FUTURE.md)
//! without a core change.
//!
//! Mutations are described as step lists (§5.1); `verify_installed` and
//! `query_available` run directly through the `CommandRunner`/`HttpFetcher`.

pub mod apt;
pub mod cargo;
pub mod github_release;
pub mod logmsg;

pub use apt::AptFactory;
pub use cargo::CargoFactory;
pub use github_release::GithubReleaseFactory;
