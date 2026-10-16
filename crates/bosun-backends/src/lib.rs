//! bosun backend adapters: apt, dnf, apk, cargo, github-release (§12), github-deb /
//! github-rpm for OS packages attached to a release, and winget, scoop and
//! choco on Windows;
//! the trait surface in `bosun-ports` keeps room for the rest (FUTURE.md)
//! without a core change.
//!
//! Mutations are described as step lists (§5.1); `verify_installed` and
//! `query_available` run directly through the `CommandRunner`/`HttpFetcher`.

pub mod apk;
pub mod apt;
pub mod cargo;
pub mod choco;
pub mod dnf;
pub mod errmsg;
pub mod github_installer;
pub mod github_package;
pub mod github_release;
pub mod logmsg;
pub mod scoop;
pub mod winget;

pub use apk::ApkFactory;
pub use apt::AptFactory;
pub use cargo::CargoFactory;
pub use choco::ChocoFactory;
pub use dnf::DnfFactory;
pub use github_installer::GithubInstallerFactory;
pub use github_package::{GithubPackageFactory, PackageKind};
pub use github_release::GithubReleaseFactory;
pub use scoop::ScoopFactory;
pub use winget::WingetFactory;
