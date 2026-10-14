//! The backend probe registry (§5.3). Every compiled-in backend factory is
//! probed **in parallel** (`std::thread::scope`, no async runtime); only those
//! present are registered, and each absent one records why it was skipped (for
//! `bosun backends`). A probe that errors or panics skips just that backend —
//! it never takes down the others (Appendix B).

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::thread;

use bosun_core::{BackendId, Version};
use bosun_ports::{Backend, BackendFactory, BackendMeta, HostInfo, Probe};
use tracing::debug;

use crate::logmsg;

/// A backend that probed present: its id, probed version, and both trait
/// objects.
pub struct RegisteredBackend {
    /// The backend id.
    pub id: BackendId,
    /// The package manager's own version (§5.3).
    pub version: Version,
    /// The resolver metadata (§5.6), from the factory.
    pub meta: BackendMeta,
    /// The query and mutation sides.
    pub backend: Backend,
}

/// Why an absent backend was skipped (surfaced by `bosun backends`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SkipReason {
    /// The probe reported the backend is not present.
    NotPresent,
    /// The probe returned an error.
    ProbeFailed(String),
    /// The probe panicked.
    Panicked,
}

/// A backend that was not registered, with the reason.
pub struct SkippedBackend {
    /// The backend id.
    pub id: BackendId,
    /// Why it was skipped.
    pub reason: SkipReason,
}

/// The outcome of this run's probes: the present backends and the skipped ones.
pub struct Registry {
    backends: Vec<RegisteredBackend>,
    skipped: Vec<SkippedBackend>,
}

impl Registry {
    /// The registered (present) backends.
    pub fn backends(&self) -> &[RegisteredBackend] {
        &self.backends
    }

    /// The skipped backends and why.
    pub fn skipped(&self) -> &[SkippedBackend] {
        &self.skipped
    }

    /// The registered backend with this id, if present.
    pub fn get(&self, id: &str) -> Option<&RegisteredBackend> {
        self.backends.iter().find(|b| b.id.as_str() == id)
    }

    /// Consume the registry into its registered and skipped backends, so a
    /// caller can move each backend's `Index`/`Installer` out of it.
    pub fn into_parts(self) -> (Vec<RegisteredBackend>, Vec<SkippedBackend>) {
        (self.backends, self.skipped)
    }
}

enum Outcome {
    Present(Probe),
    Absent,
    Failed(String),
    Panicked,
}

/// Probe every factory in parallel and build the registry (§5.3).
pub fn build_registry(factories: Vec<Box<dyn BackendFactory>>, host: &HostInfo) -> Registry {
    // Probe in parallel. `catch_unwind` isolates a panicking probe so the
    // others still run; asserting unwind-safety is sound because a panicked
    // probe is simply discarded (no shared state is left inconsistent).
    let outcomes: Vec<Outcome> = thread::scope(|scope| {
        let handles: Vec<_> = factories
            .iter()
            .map(|factory| {
                scope.spawn(
                    || match catch_unwind(AssertUnwindSafe(|| factory.probe(host))) {
                        Ok(Ok(Some(probe))) => Outcome::Present(probe),
                        Ok(Ok(None)) => Outcome::Absent,
                        Ok(Err(e)) => Outcome::Failed(e.to_string()),
                        Err(_) => Outcome::Panicked,
                    },
                )
            })
            .collect();
        handles
            .into_iter()
            .map(|h| h.join().expect("probe wrapper does not panic"))
            .collect()
    });

    let mut backends = Vec::new();
    let mut skipped = Vec::new();
    for (factory, outcome) in factories.into_iter().zip(outcomes) {
        let id = factory.id();
        match outcome {
            Outcome::Present(probe) => {
                let version = probe.version.clone();
                debug!(message = logmsg::BACKEND_REGISTERED, backend = %id.as_str(), version = %version);
                let meta = factory.meta();
                let backend = factory.build(probe);
                backends.push(RegisteredBackend {
                    id,
                    version,
                    meta,
                    backend,
                });
            }
            Outcome::Absent => {
                debug!(message = logmsg::BACKEND_ABSENT, backend = %id.as_str());
                skipped.push(SkippedBackend {
                    id,
                    reason: SkipReason::NotPresent,
                });
            }
            Outcome::Failed(e) => {
                debug!(message = logmsg::BACKEND_PROBE_ERROR, backend = %id.as_str(), error = %e);
                skipped.push(SkippedBackend {
                    id,
                    reason: SkipReason::ProbeFailed(e),
                });
            }
            Outcome::Panicked => {
                debug!(message = logmsg::BACKEND_PROBE_PANIC, backend = %id.as_str());
                skipped.push(SkippedBackend {
                    id,
                    reason: SkipReason::Panicked,
                });
            }
        }
    }
    Registry { backends, skipped }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bosun_core::{Arch, Capabilities, Installed, Os, Ref, Scheme, Scope, Source, Target};
    use bosun_ports::testing::FakeCommandRunner;
    use bosun_ports::{Index, Installer, PortError, Steps};

    /// A no-op backend; the registry test registers it but never drives it.
    struct Noop;
    impl Index for Noop {
        fn refresh(&self) -> Result<(), PortError> {
            Ok(())
        }
        fn query_available(&self, _: &Ref, _: &Target) -> Result<Vec<Version>, PortError> {
            Ok(vec![])
        }
    }
    impl Installer for Noop {
        fn capabilities(&self) -> Capabilities {
            Capabilities {
                supports_exact_version: false,
                hold: bosun_core::HoldSupport::NotNeeded,
                supports_downgrade: false,
            }
        }
        fn scope(&self) -> Scope {
            Scope::User
        }
        fn ensure_source(&self, _: &Source) -> Result<Steps, PortError> {
            Ok(vec![])
        }
        fn install_exact(&self, _: &Ref, _: &Version) -> Result<Steps, PortError> {
            Ok(vec![])
        }
        fn verify_installed(&self, _: &Ref) -> Result<Installed, PortError> {
            Ok(Installed::Absent)
        }
        fn hold(&self, _: &Ref, _: &Version) -> Result<Steps, PortError> {
            Ok(vec![])
        }
        fn downgrade(&self, _: &Ref, _: &Version) -> Result<Steps, PortError> {
            Ok(vec![])
        }
        fn uninstall(&self, _: &Ref) -> Result<Steps, PortError> {
            Ok(vec![])
        }
    }

    enum Behaviour {
        Present,
        Absent,
        Error,
        Panic,
    }

    struct TestFactory {
        id: &'static str,
        behaviour: Behaviour,
    }

    impl BackendFactory for TestFactory {
        fn id(&self) -> BackendId {
            BackendId::new(self.id)
        }
        fn meta(&self) -> BackendMeta {
            BackendMeta {
                purl_type: "generic",
                vers_type: "generic",
                platform_independent: true,
            }
        }
        fn probe(&self, _: &HostInfo) -> Result<Option<Probe>, PortError> {
            match self.behaviour {
                Behaviour::Present => Ok(Some(Probe {
                    backend: BackendId::new(self.id),
                    version: Version::parse("1.0.0", Scheme::Semver).unwrap(),
                })),
                Behaviour::Absent => Ok(None),
                Behaviour::Error => Err(PortError::Backend {
                    detail: "boom".into(),
                }),
                Behaviour::Panic => panic!("probe blew up"),
            }
        }
        fn build(&self, _: Probe) -> Backend {
            Backend {
                index: Box::new(Noop),
                installer: Box::new(Noop),
            }
        }
    }

    fn factory(id: &'static str, behaviour: Behaviour) -> Box<dyn BackendFactory> {
        Box::new(TestFactory { id, behaviour })
    }

    #[test]
    fn registers_present_backends_and_records_skips() {
        let runner = FakeCommandRunner::new();
        let host = HostInfo {
            os: Os::Linux,
            arch: Arch::Amd64,
            distro: None,
            runner: &runner,
        };
        let reg = build_registry(
            vec![
                factory("apt", Behaviour::Present),
                factory("dnf", Behaviour::Absent),
                factory("brew", Behaviour::Error),
                factory("snap", Behaviour::Panic), // must not take down the rest
                factory("cargo", Behaviour::Present),
            ],
            &host,
        );

        let mut present: Vec<&str> = reg.backends().iter().map(|b| b.id.as_str()).collect();
        present.sort();
        assert_eq!(present, vec!["apt", "cargo"]);
        let got = reg.get("apt").unwrap();
        // Assert on the id, not the version: both present backends carry
        // "1.0.0", so a version check cannot tell them apart.
        assert_eq!(got.id.as_str(), "apt");
        assert_eq!(got.version.as_str(), "1.0.0");

        let skip = |id: &str| {
            reg.skipped()
                .iter()
                .find(|s| s.id.as_str() == id)
                .map(|s| &s.reason)
        };
        assert_eq!(skip("dnf"), Some(&SkipReason::NotPresent));
        assert!(matches!(skip("brew"), Some(SkipReason::ProbeFailed(_))));
        assert_eq!(skip("snap"), Some(&SkipReason::Panicked));

        let (backends, skipped) = reg.into_parts();
        let mut ids: Vec<&str> = backends.iter().map(|b| b.id.as_str()).collect();
        ids.sort();
        assert_eq!(ids, vec!["apt", "cargo"]);
        assert_eq!(skipped.len(), 3);
    }
}
