//! bosun host: detect OS, architecture and distro family (§5.4), report
//! privilege and the invoking user (§5.5), and run each backend's probe to
//! build the registry of backends present on this machine (§5.3).
//! `LinuxMachine` is real; `FakeMachine` drives tests. macOS/Windows are
//! out of M1 but the `Machine` trait keeps room for them.
