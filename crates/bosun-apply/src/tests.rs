//! Tests for the plan builders and the privilege check (§4.8, §5.5).

use std::collections::BTreeMap;
use std::path::PathBuf;

use bosun_core::record::{Package, Placement, StateDocument, StateLeaf};
use bosun_core::version::Scheme;
use bosun_core::{
    Arch, Capabilities, Checksum, Distro, HoldSupport, Installed, Origin, Os, Ref, Source, Target,
    Version,
};
use bosun_ports::step::{Step, Steps};
use bosun_ports::{Elevation, Installer, Machine, Privilege, User};

use super::*;

const AMD64: Target = Target::Platform {
    os: Os::Linux,
    arch: Arch::Amd64,
};

fn deb(raw: &str) -> Version {
    Version::parse(raw, Scheme::Debian).unwrap()
}

/// A configurable fake backend installer: deterministic steps, chosen scope,
/// capability flags, and what `verify_installed` reports after apply.
struct FakeInstaller {
    scope: Scope,
    hold: HoldSupport,
    downgradable: bool,
    verify: Installed,
}

impl FakeInstaller {
    fn run(&self, verb: &str, pkg: &Ref, version: Option<&Version>) -> Steps {
        let mut argv = vec![verb.to_string(), pkg.purl.native_id()];
        if let Some(v) = version {
            argv.push(v.as_str().to_string());
        }
        vec![Step::Run {
            argv,
            env: Vec::new(),
            scope: self.scope,
            ok_exits: Vec::new(),
        }]
    }
}

impl Installer for FakeInstaller {
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            supports_exact_version: true,
            hold: self.hold,
            supports_downgrade: self.downgradable,
        }
    }
    fn scope(&self) -> Scope {
        self.scope
    }
    fn ensure_source(&self, _source: &Source) -> Result<Steps, PortError> {
        Ok(Vec::new())
    }
    fn install_exact(&self, pkg: &Ref, version: &Version) -> Result<Steps, PortError> {
        Ok(self.run("install", pkg, Some(version)))
    }
    fn verify_installed(&self, _pkg: &Ref) -> Result<Installed, PortError> {
        Ok(self.verify.clone())
    }
    fn hold(&self, pkg: &Ref, version: &Version) -> Result<Steps, PortError> {
        Ok(self.run("hold", pkg, Some(version)))
    }
    fn unhold(&self, pkg: &Ref) -> Result<Steps, PortError> {
        Ok(self.run("unhold", pkg, None))
    }
    fn downgrade(&self, pkg: &Ref, version: &Version) -> Result<Steps, PortError> {
        if self.downgradable {
            Ok(self.run("downgrade", pkg, Some(version)))
        } else {
            Err(PortError::Unsupported {
                capability: "downgrade".to_string(),
            })
        }
    }
    fn uninstall(&self, pkg: &Ref) -> Result<Steps, PortError> {
        Ok(self.run("remove", pkg, None))
    }
}

fn apt() -> FakeInstaller {
    FakeInstaller {
        scope: Scope::System,
        hold: HoldSupport::Supported,
        downgradable: true,
        verify: Installed::Absent,
    }
}

fn cargo() -> FakeInstaller {
    FakeInstaller {
        scope: Scope::User,
        hold: HoldSupport::NotNeeded,
        downgradable: false,
        verify: Installed::Absent,
    }
}

fn claim(path: &str, spec: &str) -> Claim {
    Claim {
        manifest: PathBuf::from(path),
        spec: spec.to_string(),
        user: None,
    }
}

fn managed<'a>(
    name: &str,
    resolved: &str,
    current: Current,
    installer: &'a dyn Installer,
    claims: Vec<Claim>,
) -> Managed<'a> {
    Managed {
        name: name.to_string(),
        purl: format!("pkg:deb/debian/{name}").parse().unwrap(),
        backend: BackendId::new("apt"),
        resolved: deb(resolved),
        vers_type: "deb".to_string(),
        claims,
        current,
        installer,
        checksum: None,
    }
}

/// An installer whose install is one download carrying `sha256` (`None`: the
/// release published none).
struct FetchInstaller {
    sha256: Option<&'static str>,
}

impl Installer for FetchInstaller {
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            supports_exact_version: true,
            hold: HoldSupport::NotNeeded,
            supports_downgrade: true,
        }
    }
    fn scope(&self) -> Scope {
        Scope::User
    }
    fn ensure_source(&self, _source: &Source) -> Result<Steps, PortError> {
        Ok(Vec::new())
    }
    fn install_exact(&self, _pkg: &Ref, _version: &Version) -> Result<Steps, PortError> {
        Ok(vec![Step::Fetch {
            url: "https://dl/tool.tar.gz".to_string(),
            sha256: self.sha256.map(str::to_string),
            dest: PathBuf::from("/tmp/tool.tar.gz"),
        }])
    }
    fn verify_installed(&self, _pkg: &Ref) -> Result<Installed, PortError> {
        Ok(Installed::Absent)
    }
    fn hold(&self, _pkg: &Ref, _version: &Version) -> Result<Steps, PortError> {
        Ok(Vec::new())
    }
    fn unhold(&self, _pkg: &Ref) -> Result<Steps, PortError> {
        Ok(Vec::new())
    }
    fn downgrade(&self, pkg: &Ref, version: &Version) -> Result<Steps, PortError> {
        self.install_exact(pkg, version)
    }
    fn uninstall(&self, _pkg: &Ref) -> Result<Steps, PortError> {
        Ok(Vec::new())
    }
}

fn fetched_sha(plan: &Plan) -> Option<String> {
    match &plan.actions[0].steps[0] {
        Step::Fetch { sha256, .. } => sha256.clone(),
        other => panic!("not a fetch: {other:?}"),
    }
}

#[test]
fn a_download_is_verified_against_the_checksum_the_lock_recorded() {
    // Trust on first use: the release publishes none, the lock has one.
    let installer = FetchInstaller { sha256: None };
    let mut m = managed("tool", "1.0.0", Current::Absent, &installer, vec![]);
    m.checksum = Some(Checksum("locked".to_string()));
    let plan = build_install_plan(&[m], vec![]).unwrap();
    assert_eq!(fetched_sha(&plan), Some("locked".to_string()));
}

#[test]
fn the_locked_checksum_wins_over_one_published_since() {
    let installer = FetchInstaller {
        sha256: Some("published-now"),
    };
    let mut m = managed("tool", "1.0.0", Current::Absent, &installer, vec![]);
    m.checksum = Some(Checksum("locked".to_string()));
    let plan = build_install_plan(&[m], vec![]).unwrap();
    assert_eq!(fetched_sha(&plan), Some("locked".to_string()));
}

#[test]
fn without_a_locked_checksum_the_backends_own_stands() {
    let installer = FetchInstaller {
        sha256: Some("published"),
    };
    let m = managed("tool", "1.0.0", Current::Absent, &installer, vec![]);
    let plan = build_install_plan(&[m], vec![]).unwrap();
    assert_eq!(fetched_sha(&plan), Some("published".to_string()));
}

fn kinds(plan: &Plan) -> Vec<ActionKind> {
    plan.actions.iter().map(|a| a.kind).collect()
}

// --- install plan ----------------------------------------------------------

#[test]
fn install_plan_installs_an_absent_package_and_holds_it() {
    let apt = apt();
    let m = managed(
        "jq",
        "1.7.1-3",
        Current::Absent,
        &apt,
        vec![claim("/w/a", "1.7.1-3")],
    );
    let plan = build_install_plan(&[m], vec![]).unwrap();

    assert_eq!(kinds(&plan), vec![ActionKind::Install, ActionKind::Hold]);
    let install = &plan.actions[0];
    assert_eq!(install.from, None);
    assert_eq!(install.to.as_ref().unwrap().as_str(), "1.7.1-3");
    assert!(!install.steps.is_empty(), "install carries its steps");
    assert_eq!(plan.actions[1].to.as_ref().unwrap().as_str(), "1.7.1-3");
    assert_eq!(plan.exit_code(), 2);
}

#[test]
fn install_plan_upgrades_when_installed_is_lower() {
    let apt = apt();
    let current = Current::Present {
        version: Some(deb("1.7.0-1")),
        origin: Origin::Installed,
        claimed_by: vec![],
        held: false,
    };
    let m = managed(
        "jq",
        "1.7.1-3",
        current,
        &apt,
        vec![claim("/w/a", "1.7.1-3")],
    );
    let plan = build_install_plan(&[m], vec![]).unwrap();

    assert_eq!(kinds(&plan), vec![ActionKind::Upgrade, ActionKind::Hold]);
    assert_eq!(plan.actions[0].from.as_ref().unwrap().as_str(), "1.7.0-1");
    assert_eq!(plan.actions[0].to.as_ref().unwrap().as_str(), "1.7.1-3");
}

#[test]
fn install_plan_downgrades_when_installed_is_higher() {
    let apt = apt();
    let current = Current::Present {
        version: Some(deb("1.8.0-1")),
        origin: Origin::Installed,
        claimed_by: vec![],
        held: false,
    };
    let m = managed(
        "jq",
        "1.7.1-3",
        current,
        &apt,
        vec![claim("/w/a", "1.7.1-3")],
    );
    let plan = build_install_plan(&[m], vec![]).unwrap();

    assert_eq!(kinds(&plan), vec![ActionKind::Downgrade, ActionKind::Hold]);
    // The downgrade step came from the backend's downgrade path, not install.
    assert!(matches!(
        &plan.actions[0].steps[0],
        Step::Run { argv, .. } if argv[0] == "downgrade"
    ));
}

#[test]
fn install_plan_does_nothing_when_already_at_the_resolved_version() {
    let apt = apt();
    let current = Current::Present {
        version: Some(deb("1.7.1-3")),
        origin: Origin::Installed,
        claimed_by: vec![],
        held: false,
    };
    let m = managed("jq", "1.7.1-3", current, &apt, vec![]);
    let plan = build_install_plan(&[m], vec![]).unwrap();

    assert!(plan.actions.is_empty());
    assert_eq!(plan.exit_code(), 0);
}

fn claim_by(path: &str, spec: &str, user: &str) -> Claim {
    Claim {
        user: Some(user.to_string()),
        ..claim(path, spec)
    }
}

#[test]
fn install_plan_records_a_new_claim_on_a_package_already_at_the_resolved_version() {
    // Another project installed jq; this one wants it too, at the same version.
    let apt = apt();
    let current = Current::Present {
        version: Some(deb("1.7.1-3")),
        origin: Origin::Installed,
        claimed_by: vec![claim_by("/w/b", ">=1.7", "ann")],
        held: false,
    };
    let claims = vec![claim_by("/w/b", ">=1.7", "ann"), claim("/w/a", ">=1.6")];
    let m = managed("jq", "1.7.1-3", current, &apt, claims.clone());
    let plan = build_install_plan(&[m], vec![]).unwrap();

    assert_eq!(kinds(&plan), vec![ActionKind::Claim]);
    let action = &plan.actions[0];
    assert_eq!(action.claims, claims);
    assert!(action.steps.is_empty(), "a claim runs nothing");
    assert_eq!(plan.exit_code(), 0, "state-only: no drift");
}

#[test]
fn install_plan_records_a_changed_spec_on_a_package_already_at_the_resolved_version() {
    let apt = apt();
    let current = Current::Present {
        version: Some(deb("1.7.1-3")),
        origin: Origin::Installed,
        claimed_by: vec![claim("/w/a", ">=1.6")],
        held: false,
    };
    let m = managed("jq", "1.7.1-3", current, &apt, vec![claim("/w/a", ">=1.7")]);
    let plan = build_install_plan(&[m], vec![]).unwrap();

    assert_eq!(kinds(&plan), vec![ActionKind::Claim]);
}

#[test]
fn install_plan_records_a_new_claim_when_the_installed_version_is_unknown() {
    let apt = apt();
    let current = Current::Present {
        version: None,
        origin: Origin::Installed,
        claimed_by: vec![],
        held: false,
    };
    let m = managed("jq", "1.7.1-3", current, &apt, vec![claim("/w/a", ">=1.6")]);
    let plan = build_install_plan(&[m], vec![]).unwrap();

    assert_eq!(kinds(&plan), vec![ActionKind::Claim]);
}

#[test]
fn install_plan_does_nothing_when_its_claim_is_already_recorded() {
    // State stamps a system-scope claim with its user; the claim the project
    // brings has none yet. Same manifest and spec is the same claim.
    let apt = apt();
    let current = Current::Present {
        version: Some(deb("1.7.1-3")),
        origin: Origin::Installed,
        claimed_by: vec![claim_by("/w/a", ">=1.6", "ben"), claim("/w/b", ">=1.7")],
        held: false,
    };
    let claims = vec![claim("/w/b", ">=1.7"), claim("/w/a", ">=1.6")];
    let m = managed("jq", "1.7.1-3", current, &apt, claims);
    let plan = build_install_plan(&[m], vec![]).unwrap();

    assert!(plan.actions.is_empty(), "{:?}", kinds(&plan));
}

#[test]
fn install_plan_accepts_a_present_satisfying_accepted_package() {
    let apt = apt();
    let current = Current::Present {
        version: Some(deb("2.43.0")),
        origin: Origin::Accepted,
        claimed_by: vec![],
        held: false,
    };
    let m = managed(
        "git",
        "2.43.0",
        current,
        &apt,
        vec![claim("/w/a", ">=2.40")],
    );
    let plan = build_install_plan(&[m], vec![]).unwrap();

    assert_eq!(kinds(&plan), vec![ActionKind::Accept]);
    let accept = &plan.actions[0];
    assert_eq!(accept.from.as_ref().unwrap().as_str(), "2.43.0");
    assert_eq!(accept.to, None);
    assert!(accept.steps.is_empty(), "accept changes nothing");
    assert!(
        !accept.needs_elevation(),
        "accept needs no root even in system scope"
    );
    // An accept-only plan changes no package, so the drift check exits 0.
    assert_eq!(plan.exit_code(), 0);
}

#[test]
fn install_plan_warns_when_an_accepted_package_no_longer_satisfies() {
    let apt = apt();
    let current = Current::Present {
        version: Some(deb("2.39.0")),
        origin: Origin::Accepted,
        claimed_by: vec![],
        held: false,
    };
    let m = managed(
        "git",
        "2.45.0",
        current,
        &apt,
        vec![claim("/w/a", ">=2.40")],
    );
    let plan = build_install_plan(&[m], vec![]).unwrap();

    assert!(
        plan.actions.is_empty(),
        "bosun changes nothing it does not own"
    );
    assert_eq!(plan.diagnostics.len(), 1);
    assert_eq!(plan.diagnostics[0].severity(), Severity::Warning);
    match &plan.diagnostics[0] {
        Diagnostic::AcceptedUnsatisfied {
            name,
            installed,
            spec,
        } => {
            assert_eq!(name, "git");
            assert_eq!(installed, "2.39.0");
            // The spec field names the claim that is no longer met.
            assert_eq!(spec, "/w/a=>=2.40");
        }
        other => panic!("expected accepted-unsatisfied, got {other:?}"),
    }
    assert_eq!(plan.exit_code(), 0);
}

#[test]
fn diagnostics_render_human_text() {
    let d = Diagnostic::Unsatisfiable {
        name: "fd".to_string(),
        spec: "vers:deb/>=99".to_string(),
        available: "1.0-1".to_string(),
    };
    let text = d.to_string();
    assert!(text.contains("fd"), "names the entry: {text}");
    assert!(
        text.contains(crate::errmsg::UNSATISFIABLE),
        "carries the label: {text}"
    );
    assert!(text.contains(">=99"), "carries the spec: {text}");
}

#[test]
fn a_carried_error_diagnostic_blocks_apply() {
    let plan = build_install_plan(
        &[],
        vec![Diagnostic::Unsatisfiable {
            name: "fd".to_string(),
            spec: "vers:deb/>=99".to_string(),
            available: "1.0-1".to_string(),
        }],
    )
    .unwrap();

    assert!(plan.has_errors());
    assert_eq!(plan.exit_code(), 1);
}

#[test]
fn install_plan_orders_installs_then_upgrades_then_holds() {
    let apt = apt();
    let install = managed(
        "a",
        "1.0-1",
        Current::Absent,
        &apt,
        vec![claim("/w/a", "1.0-1")],
    );
    let upgrade = managed(
        "b",
        "2.0-1",
        Current::Present {
            version: Some(deb("1.0-1")),
            origin: Origin::Installed,
            claimed_by: vec![],
            held: false,
        },
        &apt,
        vec![claim("/w/a", "2.0-1")],
    );
    let plan = build_install_plan(&[install, upgrade], vec![]).unwrap();

    assert_eq!(
        kinds(&plan),
        vec![
            ActionKind::Install,
            ActionKind::Upgrade,
            ActionKind::Hold,
            ActionKind::Hold,
        ]
    );
}

#[test]
fn an_unsupported_hold_warns_and_skips_the_hold() {
    // A backend that would benefit from a hold but cannot (§5.2).
    let brew = FakeInstaller {
        scope: Scope::User,
        hold: HoldSupport::Unsupported,
        downgradable: true,
        verify: Installed::Absent,
    };
    let m = Managed {
        name: "ripgrep".to_string(),
        purl: "pkg:generic/ripgrep".parse().unwrap(),
        backend: BackendId::new("brew"),
        resolved: Version::parse("14.1.1", Scheme::Semver).unwrap(),
        vers_type: "semver".to_string(),
        claims: vec![claim("/w/a", "14.1.1")],
        current: Current::Absent,
        installer: &brew,
        checksum: None,
    };
    let plan = build_install_plan(&[m], vec![]).unwrap();
    // The install still stands; the hold degrades to a warning (§5.2).
    assert_eq!(kinds(&plan), vec![ActionKind::Install]);
    assert_eq!(plan.diagnostics.len(), 1);
    assert_eq!(plan.diagnostics[0].severity(), Severity::Warning);
    assert!(matches!(
        &plan.diagnostics[0],
        Diagnostic::UnsupportedHold { name, backend } if name == "ripgrep" && *backend == BackendId::new("brew")
    ));
    assert_eq!(plan.exit_code(), 2);
}

#[test]
fn a_not_needed_hold_is_silent() {
    // cargo/github re-assert or own the binary, so no hold and no warning (§5.2).
    let cargo = cargo();
    let m = Managed {
        name: "ripgrep".to_string(),
        purl: "pkg:cargo/ripgrep".parse().unwrap(),
        backend: BackendId::new("cargo"),
        resolved: Version::parse("14.1.1", Scheme::Semver).unwrap(),
        vers_type: "cargo".to_string(),
        claims: vec![],
        current: Current::Absent,
        installer: &cargo,
        checksum: None,
    };
    let plan = build_install_plan(&[m], vec![]).unwrap();
    assert_eq!(kinds(&plan), vec![ActionKind::Install]);
    assert!(plan.diagnostics.is_empty(), "not-needed hold is silent");
}

// --- holds: only an exact spec is held (§5.2) --------------------------------

fn installed_at(raw: &str, held: bool, claimed_by: Vec<Claim>) -> Current {
    Current::Present {
        version: Some(deb(raw)),
        origin: Origin::Installed,
        claimed_by,
        held,
    }
}

fn argv0(action: &Action) -> &str {
    match &action.steps[0] {
        Step::Run { argv, .. } => &argv[0],
        other => panic!("not a run: {other:?}"),
    }
}

#[test]
fn a_ranged_or_star_spec_installs_unheld() {
    let apt = apt();
    for spec in ["*", ">=1.6", ">=1.7.1|<1.7.2~"] {
        let m = managed(
            "jq",
            "1.7.1-3",
            Current::Absent,
            &apt,
            vec![claim("/w/a", spec)],
        );
        let plan = build_install_plan(&[m], vec![]).unwrap();
        assert_eq!(kinds(&plan), vec![ActionKind::Install], "{spec}");
        assert!(plan.diagnostics.is_empty(), "{spec}");
    }
}

#[test]
fn any_exact_claim_holds_the_package() {
    let apt = apt();
    let claims = vec![claim("/w/a", ">=1.6"), claim("/w/b", "1.7.1-3")];
    let m = managed("jq", "1.7.1-3", Current::Absent, &apt, claims);
    let plan = build_install_plan(&[m], vec![]).unwrap();
    assert_eq!(kinds(&plan), vec![ActionKind::Install, ActionKind::Hold]);
}

#[test]
fn a_ranged_spec_on_a_backend_that_cannot_hold_warns_nothing() {
    let brew = FakeInstaller {
        scope: Scope::System,
        hold: HoldSupport::Unsupported,
        downgradable: true,
        verify: Installed::Absent,
    };
    let m = managed(
        "jq",
        "1.7.1-3",
        Current::Absent,
        &brew,
        vec![claim("/w/a", "*")],
    );
    let plan = build_install_plan(&[m], vec![]).unwrap();
    assert_eq!(kinds(&plan), vec![ActionKind::Install]);
    assert!(plan.diagnostics.is_empty());
}

#[test]
fn a_held_package_whose_spec_is_no_longer_exact_is_unheld() {
    // bosun held it under an exact spec, or under a range before it held only
    // exact specs; now the spec is a range, so the distro's updates may flow.
    let apt = apt();
    let claims = vec![claim("/w/a", ">=1.6")];
    let current = installed_at("1.7.1-3", true, claims.clone());
    let plan =
        build_install_plan(&[managed("jq", "1.7.1-3", current, &apt, claims)], vec![]).unwrap();

    assert_eq!(kinds(&plan), vec![ActionKind::Unhold]);
    let unhold = &plan.actions[0];
    assert_eq!(argv0(unhold), "unhold");
    assert_eq!(unhold.from.as_ref().unwrap().as_str(), "1.7.1-3");
    assert_eq!(unhold.to, None);
    assert!(unhold.needs_elevation());
    assert_eq!(plan.exit_code(), 2);
}

#[test]
fn a_held_package_moving_under_a_ranged_spec_is_unheld_before_it_moves() {
    let apt = apt();
    let claims = vec![claim("/w/a", "*")];
    let current = installed_at("1.7.0-1", true, claims.clone());
    let plan =
        build_install_plan(&[managed("jq", "1.7.1-3", current, &apt, claims)], vec![]).unwrap();
    assert_eq!(kinds(&plan), vec![ActionKind::Unhold, ActionKind::Upgrade]);
}

#[test]
fn unholds_come_before_every_install() {
    let apt = apt();
    let fresh = managed(
        "a",
        "1.0-1",
        Current::Absent,
        &apt,
        vec![claim("/w/a", "*")],
    );
    let claims = vec![claim("/w/a", "*")];
    let held = managed(
        "b",
        "1.0-1",
        installed_at("1.0-1", true, claims.clone()),
        &apt,
        claims,
    );
    let plan = build_install_plan(&[fresh, held], vec![]).unwrap();
    assert_eq!(kinds(&plan), vec![ActionKind::Unhold, ActionKind::Install]);
}

#[test]
fn an_unheld_package_whose_spec_is_now_exact_is_held() {
    let apt = apt();
    let current = installed_at("1.7.1-3", false, vec![claim("/w/a", ">=1.6")]);
    let claims = vec![claim("/w/a", "1.7.1-3")];
    let plan =
        build_install_plan(&[managed("jq", "1.7.1-3", current, &apt, claims)], vec![]).unwrap();
    assert_eq!(kinds(&plan), vec![ActionKind::Claim, ActionKind::Hold]);
    assert_eq!(argv0(&plan.actions[1]), "hold");
}

#[test]
fn a_package_held_as_its_spec_asks_needs_nothing() {
    let apt = apt();
    let exact = vec![claim("/w/a", "1.7.1-3")];
    let held = installed_at("1.7.1-3", true, exact.clone());
    let ranged = vec![claim("/w/a", "*")];
    let free = installed_at("1.7.1-3", false, ranged.clone());
    let plan = build_install_plan(
        &[
            managed("a", "1.7.1-3", held, &apt, exact),
            managed("b", "1.7.1-3", free, &apt, ranged),
        ],
        vec![],
    )
    .unwrap();
    assert!(plan.actions.is_empty(), "{:?}", kinds(&plan));
}

#[test]
fn a_backend_without_holds_never_unholds() {
    for hold in [HoldSupport::Unsupported, HoldSupport::NotNeeded] {
        let backend = FakeInstaller {
            scope: Scope::System,
            hold,
            downgradable: true,
            verify: Installed::Absent,
        };
        let claims = vec![claim("/w/a", "*")];
        let current = installed_at("1.7.1-3", true, claims.clone());
        let plan = build_install_plan(
            &[managed("jq", "1.7.1-3", current, &backend, claims)],
            vec![],
        )
        .unwrap();
        assert!(plan.actions.is_empty(), "{hold:?}");
    }
}

#[test]
fn an_accepted_package_is_never_held_or_unheld() {
    let apt = apt();
    let current = Current::Present {
        version: Some(deb("1.7.1-3")),
        origin: Origin::Accepted,
        claimed_by: vec![],
        held: true,
    };
    let plan = build_install_plan(
        &[managed(
            "jq",
            "1.7.1-3",
            current,
            &apt,
            vec![claim("/w/a", "1.7.1-3")],
        )],
        vec![],
    )
    .unwrap();
    assert_eq!(kinds(&plan), vec![ActionKind::Accept]);
}

#[test]
fn an_unsupported_downgrade_is_an_error_that_skips_the_action() {
    // A backend that can't downgrade, asked to move a package down.
    let frozen = FakeInstaller {
        scope: Scope::User,
        hold: HoldSupport::NotNeeded,
        downgradable: false,
        verify: Installed::Absent,
    };
    let m = Managed {
        name: "ripgrep".to_string(),
        purl: "pkg:cargo/ripgrep".parse().unwrap(),
        backend: BackendId::new("cargo"),
        resolved: Version::parse("1.0.0", Scheme::Semver).unwrap(),
        vers_type: "cargo".to_string(),
        claims: vec![],
        current: Current::Present {
            version: Some(Version::parse("2.0.0", Scheme::Semver).unwrap()),
            origin: Origin::Installed,
            claimed_by: vec![],
            held: false,
        },
        installer: &frozen,
        checksum: None,
    };
    let plan = build_install_plan(&[m], vec![]).unwrap();

    // No action emitted; an error diagnostic naming the package and backend.
    assert!(plan.actions.is_empty());
    assert!(plan.has_errors());
    assert!(matches!(
        &plan.diagnostics[0],
        Diagnostic::UnsupportedDowngrade { name, backend } if name == "ripgrep" && *backend == BackendId::new("cargo")
    ));
    assert_eq!(plan.exit_code(), 1);
}

// --- privilege check -------------------------------------------------------

struct FakeMachine {
    privilege: Privilege,
}

impl Machine for FakeMachine {
    fn os(&self) -> Os {
        Os::Linux
    }
    fn arch(&self) -> Arch {
        Arch::Amd64
    }
    fn distro(&self) -> Option<&Distro> {
        None
    }
    fn libc(&self) -> Option<&bosun_core::Libc> {
        None
    }
    fn privilege(&self) -> Privilege {
        self.privilege
    }
    fn invoking_user(&self) -> User {
        User {
            name: "babbitt".to_string(),
            uid: Some(1000),
        }
    }
    fn state_path(&self, _scope: Scope) -> PathBuf {
        PathBuf::from("/var/lib/bosun/state.json")
    }
    fn elevation(&self) -> Elevation {
        Elevation::Sudo
    }
}

fn system_install_plan() -> Plan {
    let apt = apt();
    // Leaked so the plan (which holds no installer) outlives this helper.
    let apt: &'static FakeInstaller = Box::leak(Box::new(apt));
    build_install_plan(
        &[managed("jq", "1.0-1", Current::Absent, apt, vec![])],
        vec![],
    )
    .unwrap()
}

fn user_install_plan() -> Plan {
    let cargo: &'static FakeInstaller = Box::leak(Box::new(cargo()));
    let m = Managed {
        name: "ripgrep".to_string(),
        purl: "pkg:cargo/ripgrep".parse().unwrap(),
        backend: BackendId::new("cargo"),
        resolved: Version::parse("1.0.0", Scheme::Semver).unwrap(),
        vers_type: "cargo".to_string(),
        claims: vec![],
        current: Current::Absent,
        installer: cargo,
        checksum: None,
    };
    build_install_plan(&[m], vec![]).unwrap()
}

#[test]
fn unelevated_system_changes_require_elevation() {
    let machine = FakeMachine {
        privilege: Privilege::Unelevated,
    };
    // The plan installs AND holds jq (apt holds); it must be named once.
    let err = check_privilege(&system_install_plan(), &machine, None).unwrap_err();
    match err {
        PrivilegeError::ElevationRequired { packages } => {
            assert_eq!(packages.iter().filter(|p| p.name == "jq").count(), 1);
        }
        other => panic!("expected elevation required, got {other:?}"),
    }
}

#[test]
fn elevated_user_changes_must_not_run_elevated() {
    let machine = FakeMachine {
        privilege: Privilege::Elevated,
    };
    let err = check_privilege(&user_install_plan(), &machine, None).unwrap_err();
    assert!(matches!(err, PrivilegeError::MustNotBeElevated { .. }));
}

#[test]
fn system_changes_pass_when_elevated_and_user_changes_pass_unelevated() {
    let elevated = FakeMachine {
        privilege: Privilege::Elevated,
    };
    let unelevated = FakeMachine {
        privilege: Privilege::Unelevated,
    };
    assert!(check_privilege(&system_install_plan(), &elevated, None).is_ok());
    assert!(check_privilege(&user_install_plan(), &unelevated, None).is_ok());
}

#[test]
fn a_mixed_plan_without_a_scope_is_refused() {
    // One system action and one user action in the same plan.
    let apt: &'static FakeInstaller = Box::leak(Box::new(apt()));
    let cargo: &'static FakeInstaller = Box::leak(Box::new(cargo()));
    let sys = managed("jq", "1.0-1", Current::Absent, apt, vec![]);
    let usr = Managed {
        name: "ripgrep".to_string(),
        purl: "pkg:cargo/ripgrep".parse().unwrap(),
        backend: BackendId::new("cargo"),
        resolved: Version::parse("1.0.0", Scheme::Semver).unwrap(),
        vers_type: "cargo".to_string(),
        claims: vec![],
        current: Current::Absent,
        installer: cargo,
        checksum: None,
    };
    let plan = build_install_plan(&[sys, usr], vec![]).unwrap();
    let machine = FakeMachine {
        privilege: Privilege::Unelevated,
    };

    let err = check_privilege(&plan, &machine, None).unwrap_err();
    match err {
        PrivilegeError::MixedScope { system, user } => {
            assert!(system.iter().any(|p| p.name == "jq"));
            assert!(user.iter().any(|p| p.name == "ripgrep"));
        }
        other => panic!("expected mixed scope, got {other:?}"),
    }
    // Restricting to one scope resolves it: --scope system under sudo.
    let elevated = FakeMachine {
        privilege: Privilege::Elevated,
    };
    assert!(check_privilege(&plan, &elevated, Some(Scope::System)).is_ok());
}

#[test]
fn an_accept_only_plan_needs_no_elevation() {
    let apt = apt();
    let current = Current::Present {
        version: Some(deb("2.43.0")),
        origin: Origin::Accepted,
        claimed_by: vec![],
        held: false,
    };
    let m = managed(
        "git",
        "2.43.0",
        current,
        &apt,
        vec![claim("/w/a", ">=2.40")],
    );
    let plan = build_install_plan(&[m], vec![]).unwrap();
    let machine = FakeMachine {
        privilege: Privilege::Unelevated,
    };
    assert!(check_privilege(&plan, &machine, None).is_ok());
}

#[test]
fn without_elevation_system_records_wait_and_everything_else_stays() {
    let apt: &'static FakeInstaller = Box::leak(Box::new(apt()));
    let cargo: &'static FakeInstaller = Box::leak(Box::new(cargo()));
    let semver = |v: &str| Version::parse(v, Scheme::Semver).unwrap();
    let accepted = |v: Version| Current::Present {
        version: Some(v),
        origin: Origin::Accepted,
        claimed_by: vec![],
        held: false,
    };
    let jq = managed(
        "jq",
        "1.7.1-3",
        accepted(deb("1.7.1-3")),
        apt,
        vec![claim("/w/a", ">=1.6")],
    );
    let fd = Managed {
        name: "fd".to_string(),
        purl: "pkg:cargo/fd-find".parse().unwrap(),
        backend: BackendId::new("cargo"),
        resolved: semver("10.0.0"),
        vers_type: "cargo".to_string(),
        claims: vec![claim("/w/a", "*")],
        current: accepted(semver("10.0.0")),
        installer: cargo,
        checksum: None,
    };
    let rg = Managed {
        name: "ripgrep".to_string(),
        purl: "pkg:cargo/ripgrep".parse().unwrap(),
        backend: BackendId::new("cargo"),
        resolved: semver("1.0.0"),
        vers_type: "cargo".to_string(),
        claims: vec![],
        current: Current::Absent,
        installer: cargo,
        checksum: None,
    };
    let plan = build_install_plan(&[jq, fd, rg], vec![]).unwrap();
    assert_eq!(
        defer_system_records(plan.clone(), Privilege::Elevated),
        plan
    );

    let deferred = defer_system_records(plan, Privilege::Unelevated);
    assert_eq!(
        kinds(&deferred),
        vec![ActionKind::Install, ActionKind::Accept]
    );
    assert_eq!(deferred.actions[1].name, "fd");
    assert_eq!(
        deferred.diagnostics,
        vec![Diagnostic::Deferred {
            name: "jq".to_string(),
            action: ActionKind::Accept,
        }]
    );
    assert_eq!(deferred.diagnostics[0].severity(), Severity::Warning);
    assert_eq!(
        deferred.diagnostics[0].to_string(),
        format!("jq: accept {}", errmsg::DEFERRED)
    );
    assert_eq!(
        deferred.deferred(),
        vec![ActionOutcome {
            name: "jq".to_string(),
            kind: ActionKind::Accept,
            result: ApplyResult::Deferred,
        }]
    );
    assert_eq!(deferred.exit_code(), 2);

    // A system change stays, for the privilege check to refuse.
    let install = system_install_plan();
    assert_eq!(
        defer_system_records(install.clone(), Privilege::Unelevated),
        install
    );
    assert!(install.deferred().is_empty());
}

// --- uninstall and prune plans ---------------------------------------------

fn leaf(name: &str, installed: &str, origin: Origin, claims: Vec<Claim>) -> StateLeaf {
    StateLeaf {
        placement: Placement {
            backend: BackendId::new("apt"),
            purl: format!("pkg:deb/debian/{name}").parse().unwrap(),
        },
        installed: deb(installed),
        origin,
        claimed_by: claims,
        held: false,
    }
}

fn state(packages: Vec<(&str, StateLeaf)>) -> StateDocument {
    StateDocument {
        state_version: 1,
        packages: packages
            .into_iter()
            .map(|(name, l)| Package {
                name: name.to_string(),
                meta: (),
                targets: BTreeMap::from([(AMD64, l)]),
            })
            .collect(),
    }
}

fn installers(apt: &dyn Installer) -> BTreeMap<BackendId, &dyn Installer> {
    BTreeMap::from([(BackendId::new("apt"), apt)])
}

#[test]
fn uninstall_removes_an_installed_package_on_its_last_claim() {
    let apt = apt();
    let st = state(vec![(
        "jq",
        leaf(
            "jq",
            "1.7.1-3",
            Origin::Installed,
            vec![claim("/w/a", ">=1.7")],
        ),
    )]);
    let plan = build_uninstall_plan(Path::new("/w/a"), &st, &installers(&apt)).unwrap();

    assert_eq!(kinds(&plan), vec![ActionKind::Remove]);
    let remove = &plan.actions[0];
    assert_eq!(remove.from.as_ref().unwrap().as_str(), "1.7.1-3");
    assert!(matches!(&remove.steps[0], Step::Run { argv, .. } if argv[0] == "remove"));
}

#[test]
fn uninstall_only_drops_the_claim_when_another_manifest_still_wants_it() {
    let apt = apt();
    let st = state(vec![(
        "jq",
        leaf(
            "jq",
            "1.7.1-3",
            Origin::Installed,
            vec![claim("/w/a", ">=1.7"), claim("/w/b", ">=1.7")],
        ),
    )]);
    let plan = build_uninstall_plan(Path::new("/w/a"), &st, &installers(&apt)).unwrap();

    assert_eq!(kinds(&plan), vec![ActionKind::DropClaim]);
    assert_eq!(plan.actions[0].claims, vec![claim("/w/a", ">=1.7")]);
    // A bare claim drop changes no package: exit 0.
    assert_eq!(plan.exit_code(), 0);
}

#[test]
fn uninstall_never_removes_an_accepted_package() {
    let apt = apt();
    let st = state(vec![(
        "git",
        leaf(
            "git",
            "2.43.0",
            Origin::Accepted,
            vec![claim("/w/a", ">=2.40")],
        ),
    )]);
    let plan = build_uninstall_plan(Path::new("/w/a"), &st, &installers(&apt)).unwrap();

    assert_eq!(kinds(&plan), vec![ActionKind::DropClaim]);
}

#[test]
fn uninstall_ignores_packages_this_manifest_does_not_claim() {
    let apt = apt();
    let st = state(vec![(
        "jq",
        leaf(
            "jq",
            "1.7.1-3",
            Origin::Installed,
            vec![claim("/w/other", ">=1.7")],
        ),
    )]);
    let plan = build_uninstall_plan(Path::new("/w/a"), &st, &installers(&apt)).unwrap();
    assert!(plan.actions.is_empty());
}

#[test]
fn release_removes_only_the_named_package_of_this_manifest() {
    let apt = apt();
    let st = state(vec![
        (
            "jq",
            leaf(
                "jq",
                "1.7.1-3",
                Origin::Installed,
                vec![claim("/w/a", ">=1.7")],
            ),
        ),
        (
            "git",
            leaf(
                "git",
                "2.43.0",
                Origin::Installed,
                vec![claim("/w/a", ">=2.40")],
            ),
        ),
    ]);
    let plan = build_release_plan(Path::new("/w/a"), "jq", &st, &installers(&apt)).unwrap();

    assert_eq!(kinds(&plan), vec![ActionKind::Remove]);
    assert_eq!(plan.actions[0].name, "jq");
}

#[test]
fn release_keeps_a_package_another_manifest_claims() {
    let apt = apt();
    let st = state(vec![(
        "jq",
        leaf(
            "jq",
            "1.7.1-3",
            Origin::Installed,
            vec![claim("/w/a", ">=1.7"), claim("/w/b", ">=1.7")],
        ),
    )]);
    let plan = build_release_plan(Path::new("/w/a"), "jq", &st, &installers(&apt)).unwrap();

    assert_eq!(kinds(&plan), vec![ActionKind::DropClaim]);
    assert_eq!(plan.actions[0].claims, vec![claim("/w/a", ">=1.7")]);
}

#[test]
fn release_of_a_package_this_manifest_never_claimed_is_empty() {
    let apt = apt();
    let st = state(vec![(
        "jq",
        leaf(
            "jq",
            "1.7.1-3",
            Origin::Installed,
            vec![claim("/w/b", ">=1.7")],
        ),
    )]);
    let plan = build_release_plan(Path::new("/w/a"), "jq", &st, &installers(&apt)).unwrap();
    assert!(plan.actions.is_empty());
}

#[test]
fn prune_drops_claims_of_vanished_manifests_and_removes_orphans() {
    let apt = apt();
    let st = state(vec![
        (
            "bat",
            leaf(
                "bat",
                "0.24.0",
                Origin::Installed,
                vec![claim("/gone", ">=0")],
            ),
        ),
        (
            "git",
            leaf(
                "git",
                "2.43.0",
                Origin::Installed,
                vec![claim("/here", ">=2")],
            ),
        ),
    ]);
    let present = |p: &Path| p == Path::new("/here");
    let plan = build_prune_plan(&st, &installers(&apt), present).unwrap();

    // bat's manifest is gone and it was the last claim → remove; git stays.
    assert_eq!(kinds(&plan), vec![ActionKind::Remove]);
    assert_eq!(plan.actions[0].name, "bat");
}

#[test]
fn removal_plan_orders_removes_before_dropped_claims() {
    let apt = apt();
    // State lists the drop-claim package (git, still wanted by /w/b) before the
    // removed one (jq): a correct plan reorders removes ahead of drop-claims.
    let st = state(vec![
        (
            "git",
            leaf(
                "git",
                "2.43.0",
                Origin::Installed,
                vec![claim("/w/a", ">=2"), claim("/w/b", ">=2")],
            ),
        ),
        (
            "jq",
            leaf(
                "jq",
                "1.7.1-3",
                Origin::Installed,
                vec![claim("/w/a", ">=1.7")],
            ),
        ),
    ]);
    let plan = build_uninstall_plan(Path::new("/w/a"), &st, &installers(&apt)).unwrap();

    assert_eq!(
        kinds(&plan),
        vec![ActionKind::Remove, ActionKind::DropClaim]
    );
    assert_eq!(plan.actions[0].name, "jq");
    assert_eq!(plan.actions[1].name, "git");
}

#[test]
fn privilege_errors_render_their_packages() {
    let err = PrivilegeError::ElevationRequired {
        packages: vec![PackageRef {
            name: "jq".to_string(),
            backend: BackendId::new("apt"),
        }],
    };
    let text = err.to_string();
    assert!(text.contains("jq"), "names the package: {text}");
    assert!(text.contains("apt"), "names the backend: {text}");
    assert!(
        text.contains(crate::errmsg::ELEVATION_REQUIRED),
        "carries the label: {text}"
    );
}

/// A release of jq (apt, system scope) by /w/a: a drop-claim when /w/b also
/// claims it, else a remove.
fn jq_release(shared: bool) -> Plan {
    let apt = apt();
    let mut claims = vec![claim("/w/a", ">=1.7")];
    if shared {
        claims.push(claim("/w/b", ">=1.7"));
    }
    let st = state(vec![(
        "jq",
        leaf("jq", "1.7.1-3", Origin::Installed, claims),
    )]);
    build_release_plan(Path::new("/w/a"), "jq", &st, &installers(&apt)).unwrap()
}

#[test]
fn without_elevation_a_release_dropping_a_system_claim_is_refused() {
    let plan = jq_release(true);
    assert_eq!(kinds(&plan), vec![ActionKind::DropClaim]);
    let unelevated = FakeMachine {
        privilege: Privilege::Unelevated,
    };
    let err = check_release_privilege(&plan, &unelevated).unwrap_err();
    assert_eq!(
        err,
        PrivilegeError::ReleaseNeedsElevation {
            packages: vec![PackageRef {
                name: "jq".to_string(),
                backend: BackendId::new("apt"),
            }],
        }
    );
    assert_eq!(
        err.to_string(),
        format!("{}: jq (apt)", crate::errmsg::RELEASE_NEEDS_ELEVATION)
    );
}

#[test]
fn a_release_passes_elevated_or_with_no_system_claim_to_drop() {
    let elevated = FakeMachine {
        privilege: Privilege::Elevated,
    };
    let unelevated = FakeMachine {
        privilege: Privilege::Unelevated,
    };
    assert_eq!(
        check_release_privilege(&jq_release(true), &elevated),
        Ok(())
    );
    // A remove changes the machine: that is check_privilege's to judge.
    let remove = jq_release(false);
    assert_eq!(kinds(&remove), vec![ActionKind::Remove]);
    assert_eq!(check_release_privilege(&remove, &unelevated), Ok(()));
    let mut user = jq_release(true);
    user.actions[0].scope = Scope::User;
    assert_eq!(check_release_privilege(&user, &unelevated), Ok(()));
}

// --- serialization ---------------------------------------------------------

#[test]
fn plan_serializes_to_the_json_wire_format() {
    let apt = apt();
    let m = managed(
        "jq",
        "1.7.1-3",
        Current::Absent,
        &apt,
        vec![claim("/w/a", "1.7.1-3")],
    );
    let plan = build_install_plan(&[m], vec![]).unwrap();

    let json = serde_json::to_value(&plan).unwrap();
    assert_eq!(json["exit_code"], 2);
    let actions = json["actions"].as_array().unwrap();
    assert_eq!(actions.len(), 2);
    assert_eq!(actions[0]["action"], "install");
    assert_eq!(actions[0]["scope"], "system");
    assert_eq!(actions[0]["backend"], "apt");
    assert_eq!(actions[0]["purl"], "pkg:deb/debian/jq");
    assert_eq!(actions[0]["to"], "1.7.1-3");
    assert_eq!(actions[0]["needs_elevation"], true);
    assert_eq!(actions[0]["steps"][0]["step"], "run");
    assert_eq!(actions[1]["action"], "hold");
}

// --- the host applier (9-2, §4.8/§4.6) -------------------------------------

use std::cell::RefCell;
use std::rc::Rc;

use bosun_ports::{StateStore, StateTransaction, StepRunner};

/// A step runner that records what it ran and optionally fails on a marker arg.
struct FakeStepRunner {
    fail_on: Option<String>,
    executed: RefCell<Vec<Vec<String>>>,
}

impl FakeStepRunner {
    fn ok() -> FakeStepRunner {
        FakeStepRunner {
            fail_on: None,
            executed: RefCell::new(Vec::new()),
        }
    }
    fn failing(marker: &str) -> FakeStepRunner {
        FakeStepRunner {
            fail_on: Some(marker.to_string()),
            executed: RefCell::new(Vec::new()),
        }
    }
    fn ran(&self) -> Vec<Vec<String>> {
        self.executed.borrow().clone()
    }
}

impl StepRunner for FakeStepRunner {
    fn execute(&self, steps: &[Step]) -> Result<(), PortError> {
        for step in steps {
            if let Step::Run { argv, .. } = step {
                self.executed.borrow_mut().push(argv.clone());
                if let Some(marker) = &self.fail_on {
                    if argv.iter().any(|a| a == marker) {
                        return Err(PortError::CommandFailed {
                            program: argv.first().cloned().unwrap_or_default(),
                            code: Some(1),
                            stderr: "boom".to_string(),
                        });
                    }
                }
            }
        }
        Ok(())
    }
}

/// An in-memory state store. `checkpoints` logs each mid-transaction write so a
/// test can prove per-action durability; `doc` is the latest persisted state.
struct FakeStateStore {
    doc: Rc<RefCell<StateDocument>>,
    checkpoints: Rc<RefCell<Vec<StateDocument>>>,
}

struct FakeTxn {
    doc: Rc<RefCell<StateDocument>>,
    checkpoints: Rc<RefCell<Vec<StateDocument>>>,
    snapshot: StateDocument,
}

impl FakeStateStore {
    fn empty() -> FakeStateStore {
        FakeStateStore::with(StateDocument {
            state_version: 1,
            packages: Vec::new(),
        })
    }
    fn with(doc: StateDocument) -> FakeStateStore {
        FakeStateStore {
            doc: Rc::new(RefCell::new(doc)),
            checkpoints: Rc::new(RefCell::new(Vec::new())),
        }
    }
    fn snapshot(&self) -> StateDocument {
        self.doc.borrow().clone()
    }
    fn checkpoints(&self) -> Vec<StateDocument> {
        self.checkpoints.borrow().clone()
    }
}

impl StateStore for FakeStateStore {
    type Transaction = FakeTxn;
    fn begin(&self, _scope: Scope) -> Result<FakeTxn, PortError> {
        Ok(FakeTxn {
            doc: self.doc.clone(),
            checkpoints: self.checkpoints.clone(),
            snapshot: self.doc.borrow().clone(),
        })
    }
    fn try_begin(&self, scope: Scope) -> Result<Option<FakeTxn>, PortError> {
        Ok(Some(self.begin(scope)?))
    }
    fn read(&self, _scope: Scope) -> Result<StateDocument, PortError> {
        Ok(self.doc.borrow().clone())
    }
}

impl StateTransaction for FakeTxn {
    fn state(&self) -> &StateDocument {
        &self.snapshot
    }
    fn checkpoint(&mut self, state: &StateDocument) -> Result<(), PortError> {
        // A durable write that keeps the lock: record it and update the store.
        self.checkpoints.borrow_mut().push(state.clone());
        *self.doc.borrow_mut() = state.clone();
        Ok(())
    }
    fn commit(self, state: StateDocument) -> Result<(), PortError> {
        *self.doc.borrow_mut() = state;
        Ok(())
    }
}

fn present(raw: &str) -> Installed {
    Installed::Present {
        version: Some(deb(raw)),
    }
}

fn verifying(scope: Scope, hold: bool, down: bool, verify: Installed) -> FakeInstaller {
    FakeInstaller {
        scope,
        // true → a real durable hold (apt); false → not needed (cargo/github).
        hold: if hold {
            HoldSupport::Supported
        } else {
            HoldSupport::NotNeeded
        },
        downgradable: down,
        verify,
    }
}

fn results(report: &ApplyReport) -> Vec<(ActionKind, ApplyResult)> {
    report
        .outcomes
        .iter()
        .map(|o| (o.kind, o.result.clone()))
        .collect()
}

#[test]
fn apply_runs_steps_verifies_and_records_state() {
    let apt = verifying(Scope::System, true, true, present("1.7.1-3"));
    let plan = build_install_plan(
        &[managed(
            "jq",
            "1.7.1-3",
            Current::Absent,
            &apt,
            vec![claim("/w/a", "1.7.1-3")],
        )],
        vec![],
    )
    .unwrap();

    let exec = FakeStepRunner::ok();
    let insts: BTreeMap<BackendId, &dyn Installer> =
        BTreeMap::from([(BackendId::new("apt"), &apt as &dyn Installer)]);
    let store = FakeStateStore::empty();
    let applier = HostApplier::new(
        Scope::System,
        AMD64,
        Some("babbitt".to_string()),
        &exec,
        &insts,
        &store,
    );

    let report = applier.apply(&plan).unwrap();

    assert_eq!(
        results(&report),
        vec![
            (ActionKind::Install, ApplyResult::Applied),
            (ActionKind::Hold, ApplyResult::Applied),
        ]
    );
    // State written: one package, verified version, origin installed, claim
    // stamped with the invoking user (system scope).
    let doc = store.snapshot();
    assert_eq!(doc.packages.len(), 1);
    let leaf = &doc.packages[0].targets[&AMD64];
    assert_eq!(leaf.installed.as_str(), "1.7.1-3");
    assert_eq!(leaf.origin, Origin::Installed);
    assert_eq!(leaf.claimed_by[0].user.as_deref(), Some("babbitt"));
    // The install and the hold both ran.
    assert_eq!(exec.ran().len(), 2);
    assert_eq!(exec.ran()[0][0], "install");
    assert_eq!(exec.ran()[1][0], "hold");
}

#[test]
fn apply_records_the_resolved_version_when_verify_has_none() {
    // github-release: present but no readable version → record the resolved one.
    let gh = verifying(
        Scope::User,
        false,
        false,
        Installed::Present { version: None },
    );
    let m = Managed {
        name: "ctxloom".to_string(),
        purl: "pkg:github/ctxloom/ctxloom?binary=ctxloom".parse().unwrap(),
        backend: BackendId::new("github-release"),
        resolved: Version::parse("0.6.4", Scheme::Semver).unwrap(),
        vers_type: "semver".to_string(),
        claims: vec![claim("/w/a", ">=0.6")],
        current: Current::Absent,
        installer: &gh,
        checksum: None,
    };
    let plan = build_install_plan(&[m], vec![]).unwrap();

    let exec = FakeStepRunner::ok();
    let insts: BTreeMap<BackendId, &dyn Installer> =
        BTreeMap::from([(BackendId::new("github-release"), &gh as &dyn Installer)]);
    let store = FakeStateStore::empty();
    let applier = HostApplier::new(Scope::User, AMD64, None, &exec, &insts, &store);

    let report = applier.apply(&plan).unwrap();
    assert!(report
        .outcomes
        .iter()
        .all(|o| o.result == ApplyResult::Applied));
    assert_eq!(
        store.snapshot().packages[0].targets[&AMD64]
            .installed
            .as_str(),
        "0.6.4"
    );
}

#[test]
fn apply_fails_an_install_that_does_not_verify() {
    // Steps "succeed" but the package is not present afterwards.
    let apt = verifying(Scope::System, true, true, Installed::Absent);
    let plan = build_install_plan(
        &[managed(
            "jq",
            "1.7.1-3",
            Current::Absent,
            &apt,
            vec![claim("/w/a", "1.7.1-3")],
        )],
        vec![],
    )
    .unwrap();

    let exec = FakeStepRunner::ok();
    let insts: BTreeMap<BackendId, &dyn Installer> =
        BTreeMap::from([(BackendId::new("apt"), &apt as &dyn Installer)]);
    let store = FakeStateStore::empty();
    let applier = HostApplier::new(Scope::System, AMD64, None, &exec, &insts, &store);

    let report = applier.apply(&plan).unwrap();
    assert!(matches!(
        report.outcomes[0].result,
        ApplyResult::Failed { .. }
    ));
    // Nothing recorded: the install did not verify (Appendix B).
    assert!(store.snapshot().packages.is_empty());
}

#[test]
fn apply_isolates_a_failed_backend_and_continues_others() {
    let apt = verifying(Scope::User, true, true, present("1.0.0"));
    let cargo = verifying(Scope::User, false, false, present("1.0.0"));
    // "boom" installs via apt (the runner fails on it); ripgrep via cargo is fine.
    let boom = Managed {
        name: "boom".to_string(),
        purl: "pkg:deb/debian/boom".parse().unwrap(),
        backend: BackendId::new("apt"),
        resolved: deb("1.0.0"),
        vers_type: "deb".to_string(),
        claims: vec![claim("/w/a", "1.0.0")],
        current: Current::Absent,
        installer: &apt,
        checksum: None,
    };
    let ripgrep = Managed {
        name: "ripgrep".to_string(),
        purl: "pkg:deb/debian/ripgrep".parse().unwrap(),
        backend: BackendId::new("cargo"),
        resolved: deb("1.0.0"),
        vers_type: "deb".to_string(),
        claims: vec![],
        current: Current::Absent,
        installer: &cargo,
        checksum: None,
    };
    let plan = build_install_plan(&[boom, ripgrep], vec![]).unwrap();

    let exec = FakeStepRunner::failing("boom");
    let insts: BTreeMap<BackendId, &dyn Installer> = BTreeMap::from([
        (BackendId::new("apt"), &apt as &dyn Installer),
        (BackendId::new("cargo"), &cargo as &dyn Installer),
    ]);
    let store = FakeStateStore::empty();
    let applier = HostApplier::new(Scope::User, AMD64, None, &exec, &insts, &store);

    let report = applier.apply(&plan).unwrap();

    // boom's install fails; its later hold is skipped; cargo's install still runs.
    let r = results(&report);
    assert_eq!(r.len(), 3);
    assert!(matches!(
        r[0],
        (ActionKind::Install, ApplyResult::Failed { .. })
    ));
    assert_eq!(r[1], (ActionKind::Install, ApplyResult::Applied));
    assert_eq!(r[2], (ActionKind::Hold, ApplyResult::Skipped));
    // Only ripgrep landed in state.
    let doc = store.snapshot();
    assert_eq!(doc.packages.len(), 1);
    assert_eq!(doc.packages[0].name, "ripgrep");
}

#[test]
fn apply_accept_records_a_claim_without_running_steps() {
    let apt = verifying(Scope::System, true, true, Installed::Absent);
    let current = Current::Present {
        version: Some(deb("2.43.0")),
        origin: Origin::Accepted,
        claimed_by: vec![],
        held: false,
    };
    let plan = build_install_plan(
        &[managed(
            "git",
            "2.43.0",
            current,
            &apt,
            vec![claim("/w/a", ">=2.40")],
        )],
        vec![],
    )
    .unwrap();

    let exec = FakeStepRunner::ok();
    let insts: BTreeMap<BackendId, &dyn Installer> =
        BTreeMap::from([(BackendId::new("apt"), &apt as &dyn Installer)]);
    let store = FakeStateStore::empty();
    let applier = HostApplier::new(
        Scope::System,
        AMD64,
        Some("babbitt".to_string()),
        &exec,
        &insts,
        &store,
    );

    let report = applier.apply(&plan).unwrap();
    assert_eq!(
        results(&report),
        vec![(ActionKind::Accept, ApplyResult::Applied)]
    );
    assert!(exec.ran().is_empty(), "accept runs no steps");
    let leaf = &store.snapshot().packages[0].targets[&AMD64];
    assert_eq!(leaf.origin, Origin::Accepted);
    assert_eq!(leaf.installed.as_str(), "2.43.0");
}

fn prestate(name: &str, installed: &str, origin: Origin, claims: Vec<Claim>) -> StateDocument {
    StateDocument {
        state_version: 1,
        packages: vec![Package {
            name: name.to_string(),
            meta: (),
            targets: BTreeMap::from([(
                AMD64,
                StateLeaf {
                    placement: Placement {
                        backend: BackendId::new("apt"),
                        purl: format!("pkg:deb/debian/{name}").parse().unwrap(),
                    },
                    installed: deb(installed),
                    origin,
                    claimed_by: claims,
                    held: false,
                },
            )]),
        }],
    }
}

fn held_prestate(name: &str, installed: &str, held: bool, claims: Vec<Claim>) -> StateDocument {
    let mut doc = prestate(name, installed, Origin::Installed, claims);
    doc.packages[0].targets.get_mut(&AMD64).unwrap().held = held;
    doc
}

fn apply_system(
    plan: &Plan,
    apt: &FakeInstaller,
    store: &FakeStateStore,
) -> (ApplyReport, Vec<Vec<String>>) {
    let insts: BTreeMap<BackendId, &dyn Installer> =
        BTreeMap::from([(BackendId::new("apt"), apt as &dyn Installer)]);
    let exec = FakeStepRunner::ok();
    let applier = HostApplier::new(Scope::System, AMD64, None, &exec, &insts, store);
    let report = applier.apply(plan).unwrap();
    (report, exec.ran())
}

#[test]
fn apply_records_a_hold_and_an_install_left_unheld() {
    let apt = verifying(Scope::System, true, true, present("1.7.1-3"));
    for (spec, held) in [("1.7.1-3", true), (">=1.6", false)] {
        let m = managed(
            "jq",
            "1.7.1-3",
            Current::Absent,
            &apt,
            vec![claim("/w/a", spec)],
        );
        let plan = build_install_plan(&[m], vec![]).unwrap();
        let store = FakeStateStore::empty();
        apply_system(&plan, &apt, &store);
        assert_eq!(
            store.snapshot().packages[0].targets[&AMD64].held,
            held,
            "{spec}"
        );
    }
}

#[test]
fn apply_unhold_runs_its_steps_and_records_the_package_unheld() {
    let apt = verifying(Scope::System, true, true, present("1.7.1-3"));
    let claims = vec![claim("/w/a", "*")];
    let store = FakeStateStore::with(held_prestate("jq", "1.7.1-3", true, claims.clone()));
    let current = installed_at("1.7.1-3", true, claims.clone());
    let plan =
        build_install_plan(&[managed("jq", "1.7.1-3", current, &apt, claims)], vec![]).unwrap();

    let (report, ran) = apply_system(&plan, &apt, &store);

    assert_eq!(
        results(&report),
        vec![(ActionKind::Unhold, ApplyResult::Applied)]
    );
    assert_eq!(ran, vec![vec!["unhold".to_string(), "jq".to_string()]]);
    let leaf = &store.snapshot().packages[0].targets[&AMD64];
    assert!(!leaf.held);
    assert_eq!(leaf.installed.as_str(), "1.7.1-3");
}

#[test]
fn apply_hold_records_an_installed_package_held() {
    let apt = verifying(Scope::System, true, true, present("1.7.1-3"));
    let claims = vec![claim("/w/a", "1.7.1-3")];
    let store = FakeStateStore::with(held_prestate("jq", "1.7.1-3", false, claims.clone()));
    let current = installed_at("1.7.1-3", false, claims.clone());
    let plan =
        build_install_plan(&[managed("jq", "1.7.1-3", current, &apt, claims)], vec![]).unwrap();

    let (report, _) = apply_system(&plan, &apt, &store);

    assert_eq!(
        results(&report),
        vec![(ActionKind::Hold, ApplyResult::Applied)]
    );
    assert!(store.snapshot().packages[0].targets[&AMD64].held);
}

#[test]
fn a_hold_on_a_package_state_does_not_record_fails() {
    let apt = verifying(Scope::System, true, true, present("1.7.1-3"));
    let claims = vec![claim("/w/a", "1.7.1-3")];
    let current = installed_at("1.7.1-3", false, claims.clone());
    let plan =
        build_install_plan(&[managed("jq", "1.7.1-3", current, &apt, claims)], vec![]).unwrap();
    let store = FakeStateStore::empty();

    let (report, _) = apply_system(&plan, &apt, &store);

    match &report.outcomes[0].result {
        ApplyResult::Failed { error } => {
            assert!(
                error.contains("jq: hold on a package state does not record"),
                "{error}"
            )
        }
        other => panic!("expected a failure, got {other:?}"),
    }
}

#[test]
fn apply_remove_runs_uninstall_and_drops_the_package() {
    let apt = verifying(Scope::System, true, true, Installed::Absent);
    let insts: BTreeMap<BackendId, &dyn Installer> =
        BTreeMap::from([(BackendId::new("apt"), &apt as &dyn Installer)]);
    let store = FakeStateStore::with(prestate(
        "jq",
        "1.7.1-3",
        Origin::Installed,
        vec![claim("/w/a", ">=1.7")],
    ));
    let plan = build_uninstall_plan(Path::new("/w/a"), &store.snapshot(), &insts).unwrap();

    let exec = FakeStepRunner::ok();
    let applier = HostApplier::new(Scope::System, AMD64, None, &exec, &insts, &store);
    let report = applier.apply(&plan).unwrap();

    assert_eq!(
        results(&report),
        vec![(ActionKind::Remove, ApplyResult::Applied)]
    );
    assert_eq!(exec.ran()[0][0], "remove");
    assert!(store.snapshot().packages.is_empty());
}

#[test]
fn apply_claim_records_the_claims_and_keeps_version_and_origin() {
    let apt = verifying(Scope::System, true, true, Installed::Absent);
    let insts: BTreeMap<BackendId, &dyn Installer> =
        BTreeMap::from([(BackendId::new("apt"), &apt as &dyn Installer)]);
    let store = FakeStateStore::with(prestate(
        "jq",
        "1.7.1-3",
        Origin::Installed,
        vec![claim_by("/w/b", ">=1.7", "ann")],
    ));
    let current = Current::Present {
        version: Some(deb("1.7.1-3")),
        origin: Origin::Installed,
        claimed_by: vec![claim_by("/w/b", ">=1.7", "ann")],
        held: false,
    };
    let claims = vec![claim_by("/w/b", ">=1.7", "ann"), claim("/w/a", ">=1.6")];
    let plan =
        build_install_plan(&[managed("jq", "1.7.1-3", current, &apt, claims)], vec![]).unwrap();

    let exec = FakeStepRunner::ok();
    let applier = HostApplier::new(
        Scope::System,
        AMD64,
        Some("babbitt".to_string()),
        &exec,
        &insts,
        &store,
    );
    let report = applier.apply(&plan).unwrap();

    assert_eq!(
        results(&report),
        vec![(ActionKind::Claim, ApplyResult::Applied)]
    );
    assert!(exec.ran().is_empty(), "a claim runs no steps");
    let leaf = &store.snapshot().packages[0].targets[&AMD64];
    assert_eq!(leaf.origin, Origin::Installed, "still bosun's to prune");
    assert_eq!(leaf.installed.as_str(), "1.7.1-3");
    assert_eq!(
        leaf.claimed_by,
        vec![
            claim_by("/w/b", ">=1.7", "ann"),
            claim_by("/w/a", ">=1.6", "babbitt")
        ]
    );
}

#[test]
fn apply_drop_claim_keeps_the_package_and_removes_only_that_claim() {
    let apt = verifying(Scope::System, true, true, Installed::Absent);
    let insts: BTreeMap<BackendId, &dyn Installer> =
        BTreeMap::from([(BackendId::new("apt"), &apt as &dyn Installer)]);
    let store = FakeStateStore::with(prestate(
        "jq",
        "1.7.1-3",
        Origin::Installed,
        vec![claim("/w/a", ">=1.7"), claim("/w/b", ">=1.7")],
    ));
    let plan = build_uninstall_plan(Path::new("/w/a"), &store.snapshot(), &insts).unwrap();

    let exec = FakeStepRunner::ok();
    let applier = HostApplier::new(Scope::System, AMD64, None, &exec, &insts, &store);
    let report = applier.apply(&plan).unwrap();

    assert_eq!(
        results(&report),
        vec![(ActionKind::DropClaim, ApplyResult::Applied)]
    );
    assert!(exec.ran().is_empty(), "dropping a claim runs no steps");
    let leaf = &store.snapshot().packages[0].targets[&AMD64];
    assert_eq!(leaf.claimed_by, vec![claim("/w/b", ">=1.7")]);
}

#[test]
fn apply_records_a_new_package_without_touching_an_existing_one() {
    // A different package already in state must not be overwritten by a new
    // install (guards the name match in `record`).
    let apt = verifying(Scope::System, true, true, present("1.7.1-3"));
    // git through a backend this run has no installer for, so nothing reads
    // its version again.
    let mut git = prestate(
        "git",
        "2.43.0",
        Origin::Installed,
        vec![claim("/w/x", ">=2")],
    );
    git.packages[0]
        .targets
        .get_mut(&AMD64)
        .unwrap()
        .placement
        .backend = BackendId::new("dnf");
    let store = FakeStateStore::with(git);
    let plan = build_install_plan(
        &[managed(
            "jq",
            "1.7.1-3",
            Current::Absent,
            &apt,
            vec![claim("/w/a", "1.7.1-3")],
        )],
        vec![],
    )
    .unwrap();

    let exec = FakeStepRunner::ok();
    let insts: BTreeMap<BackendId, &dyn Installer> =
        BTreeMap::from([(BackendId::new("apt"), &apt as &dyn Installer)]);
    let applier = HostApplier::new(Scope::System, AMD64, None, &exec, &insts, &store);
    applier.apply(&plan).unwrap();

    let doc = store.snapshot();
    assert_eq!(doc.packages.len(), 2);
    let git = doc.packages.iter().find(|p| p.name == "git").unwrap();
    assert_eq!(git.targets[&AMD64].installed.as_str(), "2.43.0");
    let jq = doc.packages.iter().find(|p| p.name == "jq").unwrap();
    assert_eq!(jq.targets[&AMD64].installed.as_str(), "1.7.1-3");
}

#[test]
fn user_scope_apply_leaves_claims_unstamped() {
    // The invoking user is stamped only in system scope (§4.6); a user-scope
    // apply must leave a claim's user as it was, even with a user provided.
    let inst = verifying(Scope::User, false, true, present("1.0.0"));
    let plan = build_install_plan(
        &[managed(
            "ripgrep",
            "1.0.0",
            Current::Absent,
            &inst,
            vec![claim("/w/a", ">=1.0")],
        )],
        vec![],
    )
    .unwrap();

    let exec = FakeStepRunner::ok();
    let insts: BTreeMap<BackendId, &dyn Installer> =
        BTreeMap::from([(BackendId::new("apt"), &inst as &dyn Installer)]);
    let store = FakeStateStore::empty();
    let applier = HostApplier::new(
        Scope::User,
        AMD64,
        Some("root".to_string()),
        &exec,
        &insts,
        &store,
    );
    applier.apply(&plan).unwrap();

    let leaf = &store.snapshot().packages[0].targets[&AMD64];
    assert_eq!(leaf.claimed_by[0].user, None);
}

#[test]
fn apply_checkpoints_each_confirmed_action_so_progress_survives_a_crash() {
    // A installs (ok); B's backend then fails. Because state is written after
    // each verified action, A is already on disk when B fails — a crash right
    // after B would keep A (no accepted/unheld divergence, Appendix B).
    let inst_a = verifying(Scope::User, false, false, present("1.0.0"));
    let inst_b = verifying(Scope::User, false, false, present("1.0.0"));
    let a = Managed {
        name: "A".to_string(),
        purl: "pkg:deb/debian/A".parse().unwrap(),
        backend: BackendId::new("back-a"),
        resolved: deb("1.0.0"),
        vers_type: "deb".to_string(),
        claims: vec![],
        current: Current::Absent,
        installer: &inst_a,
        checksum: None,
    };
    let b = Managed {
        name: "B".to_string(),
        purl: "pkg:deb/debian/B".parse().unwrap(),
        backend: BackendId::new("back-b"),
        resolved: deb("1.0.0"),
        vers_type: "deb".to_string(),
        claims: vec![],
        current: Current::Absent,
        installer: &inst_b,
        checksum: None,
    };
    let plan = build_install_plan(&[a, b], vec![]).unwrap();

    let exec = FakeStepRunner::failing("b"); // deb folds the purl name: B's step carries "b"
    let insts: BTreeMap<BackendId, &dyn Installer> = BTreeMap::from([
        (BackendId::new("back-a"), &inst_a as &dyn Installer),
        (BackendId::new("back-b"), &inst_b as &dyn Installer),
    ]);
    let store = FakeStateStore::empty();
    let applier = HostApplier::new(Scope::User, AMD64, None, &exec, &insts, &store);

    let report = applier.apply(&plan).unwrap();
    let r = results(&report);
    assert_eq!(r[0], (ActionKind::Install, ApplyResult::Applied));
    assert!(matches!(
        r[1],
        (ActionKind::Install, ApplyResult::Failed { .. })
    ));

    // One checkpoint — for A, written before B's action ran and failed.
    let ckpts = store.checkpoints();
    assert_eq!(ckpts.len(), 1);
    assert!(ckpts[0].packages.iter().any(|p| p.name == "A"));
    assert!(!ckpts[0].packages.iter().any(|p| p.name == "B"));
}

#[test]
fn apply_report_serializes_to_json() {
    let apt = verifying(Scope::System, true, true, present("1.7.1-3"));
    let plan = build_install_plan(
        &[managed(
            "jq",
            "1.7.1-3",
            Current::Absent,
            &apt,
            vec![claim("/w/a", "1.7.1-3")],
        )],
        vec![],
    )
    .unwrap();
    let exec = FakeStepRunner::ok();
    let insts: BTreeMap<BackendId, &dyn Installer> =
        BTreeMap::from([(BackendId::new("apt"), &apt as &dyn Installer)]);
    let store = FakeStateStore::empty();
    let applier = HostApplier::new(Scope::System, AMD64, None, &exec, &insts, &store);

    let report = applier.apply(&plan).unwrap();
    let json = serde_json::to_value(&report).unwrap();
    let outcomes = json["outcomes"].as_array().unwrap();
    assert_eq!(outcomes.len(), 2);
    assert_eq!(outcomes[0]["action"], "install");
    assert_eq!(outcomes[0]["result"], "applied");
    assert_eq!(outcomes[0]["name"], "jq");
    assert_eq!(outcomes[1]["action"], "hold");
}

#[test]
fn plan_json_renders_every_diagnostic_and_step_kind() {
    use std::path::PathBuf;
    // An action whose steps include Fetch and Extract (github-release shape).
    let action = Action {
        kind: ActionKind::Install,
        name: "ctxloom".to_string(),
        scope: Scope::User,
        backend: BackendId::new("github-release"),
        purl: "pkg:github/ctxloom/ctxloom".parse().unwrap(),
        from: None,
        to: Some(Version::parse("0.6.4", Scheme::Semver).unwrap()),
        claims: vec![Claim {
            manifest: PathBuf::from("/w/bosun.yaml"),
            spec: "vers:semver/>=0.6.0".to_string(),
            user: Some("ben".to_string()),
        }],
        steps: vec![
            Step::Fetch {
                url: "https://example/a.tar.gz".to_string(),
                sha256: Some("abc".to_string()),
                dest: PathBuf::from("/tmp/a.tar.gz"),
            },
            Step::Extract {
                format: bosun_ports::ArchiveFormat::TarGz,
                archive: PathBuf::from("/tmp/a.tar.gz"),
                member: "ctxloom".to_string(),
                dest: PathBuf::from("/home/ben/.local/bin/ctxloom"),
                mode: 0o755,
            },
        ],
    };
    let plan = Plan {
        actions: vec![action],
        diagnostics: vec![
            Diagnostic::NotApplicable { name: "a".into() },
            Diagnostic::MissingBackend {
                name: "b".into(),
                candidates: vec![BackendId::new("brew")],
            },
            Diagnostic::Unsatisfiable {
                name: "c".into(),
                spec: "vers:semver/>=9".into(),
                available: "1.0.0".into(),
            },
            Diagnostic::EmptyIntersection {
                name: "d".into(),
                claims: "x=>=1, y=<1".into(),
            },
            Diagnostic::AcceptedUnsatisfied {
                name: "e".into(),
                installed: "1.0".into(),
                spec: ">=2".into(),
            },
            Diagnostic::UnsupportedHold {
                name: "f".into(),
                backend: BackendId::new("brew"),
            },
            Diagnostic::UnsupportedDowngrade {
                name: "g".into(),
                backend: BackendId::new("brew"),
            },
        ],
    };
    let json = serde_json::to_value(&plan).unwrap();
    let steps = &json["actions"][0]["steps"];
    assert_eq!(steps[0]["step"], "fetch");
    assert_eq!(steps[1]["step"], "extract");
    assert_eq!(steps[1]["format"], "tar.gz");
    assert_eq!(json["actions"][0]["claims"][0]["user"], "ben");
    let kinds: Vec<&str> = json["diagnostics"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["diagnostic"].as_str().unwrap())
        .collect();
    assert!(kinds.contains(&"not-applicable"));
    assert!(kinds.contains(&"missing-backend"));
    assert!(kinds.contains(&"unsatisfiable"));
    assert!(kinds.contains(&"empty-intersection"));
    assert!(kinds.contains(&"accepted-unsatisfied"));
    assert!(kinds.contains(&"unsupported-hold"));
    assert!(kinds.contains(&"unsupported-downgrade"));
    // Error diagnostics make it an error plan.
    assert_eq!(json["exit_code"], 1);
}

#[test]
fn apply_report_json_renders_applied_failed_and_skipped() {
    let report = ApplyReport {
        outcomes: vec![
            ActionOutcome {
                name: "a".into(),
                kind: ActionKind::Install,
                result: ApplyResult::Applied,
            },
            ActionOutcome {
                name: "b".into(),
                kind: ActionKind::Hold,
                result: ApplyResult::Failed {
                    error: "boom".into(),
                },
            },
            ActionOutcome {
                name: "c".into(),
                kind: ActionKind::Hold,
                result: ApplyResult::Skipped,
            },
        ],
    };
    let json = serde_json::to_value(&report).unwrap();
    let os = json["outcomes"].as_array().unwrap();
    assert_eq!(os[0]["result"], "applied");
    assert_eq!(os[1]["result"], "failed");
    assert_eq!(os[1]["error"], "boom");
    assert_eq!(os[2]["result"], "skipped");
}

#[test]
fn diagnostics_and_privilege_errors_have_text() {
    // Exercise the Display arms (human/text rendering).
    assert!(Diagnostic::NotApplicable { name: "a".into() }
        .to_string()
        .contains("a"));
    assert!(Diagnostic::MissingBackend {
        name: "b".into(),
        candidates: vec![BackendId::new("brew")]
    }
    .to_string()
    .contains("brew"));
    assert!(Diagnostic::EmptyIntersection {
        name: "d".into(),
        claims: "x".into()
    }
    .to_string()
    .contains("x"));
    assert!(Diagnostic::UnsupportedHold {
        name: "f".into(),
        backend: BackendId::new("brew")
    }
    .to_string()
    .contains("brew"));
    assert!(Diagnostic::UnsupportedDowngrade {
        name: "g".into(),
        backend: BackendId::new("cargo")
    }
    .to_string()
    .contains("cargo"));
    let err = PrivilegeError::MustNotBeElevated {
        packages: vec![PackageRef {
            name: "rg".into(),
            backend: BackendId::new("cargo"),
        }],
    };
    assert!(err.to_string().contains("rg"));
    let mixed = PrivilegeError::MixedScope {
        system: vec![PackageRef {
            name: "jq".into(),
            backend: BackendId::new("apt"),
        }],
        user: vec![PackageRef {
            name: "rg".into(),
            backend: BackendId::new("cargo"),
        }],
    };
    assert!(mixed.to_string().contains("jq") && mixed.to_string().contains("rg"));
}

/// A plan with nothing in it for the system scope, applied there.
fn apply_nothing(apt: &FakeInstaller, store: &FakeStateStore) {
    let plan = Plan {
        actions: Vec::new(),
        diagnostics: Vec::new(),
    };
    apply_system(&plan, apt, store);
}

#[test]
fn apply_records_the_version_the_package_manager_moved_a_package_to() {
    let claims = vec![claim("/w/a", ">=1.6")];
    for origin in [Origin::Installed, Origin::Accepted] {
        let apt = verifying(Scope::System, true, true, present("1.7.1-4"));
        let store = FakeStateStore::with(prestate("jq", "1.7.1-3", origin, claims.clone()));
        apply_nothing(&apt, &store);
        let leaf = &store.snapshot().packages[0].targets[&AMD64];
        assert_eq!(leaf.installed.as_str(), "1.7.1-4", "{origin:?}");
        assert_eq!(leaf.origin, origin);
    }
}

#[test]
fn apply_keeps_the_recorded_version_where_the_package_manager_reports_none() {
    let claims = vec![claim("/w/a", ">=1.6")];
    for verify in [Installed::Absent, Installed::Present { version: None }] {
        let apt = verifying(Scope::System, true, true, verify);
        let store =
            FakeStateStore::with(prestate("jq", "1.7.1-3", Origin::Installed, claims.clone()));
        apply_nothing(&apt, &store);
        assert_eq!(
            store.snapshot().packages[0].targets[&AMD64]
                .installed
                .as_str(),
            "1.7.1-3"
        );
    }
}

#[test]
fn apply_keeps_the_recorded_version_of_another_target_or_an_absent_backend() {
    let claims = vec![claim("/w/a", ">=1.6")];
    let apt = verifying(Scope::System, true, true, present("1.7.1-4"));
    let mut doc = prestate("jq", "1.7.1-3", Origin::Installed, claims.clone());
    let leaf = doc.packages[0].targets.remove(&AMD64).unwrap();
    doc.packages[0].targets.insert(Target::Any, leaf);
    let mut other = prestate("rg", "14.0", Origin::Installed, claims);
    other.packages[0]
        .targets
        .get_mut(&AMD64)
        .unwrap()
        .placement
        .backend = BackendId::new("cargo");
    doc.packages.extend(other.packages);
    let store = FakeStateStore::with(doc);
    apply_nothing(&apt, &store);
    let after = store.snapshot();
    assert_eq!(
        after.packages[0].targets[&Target::Any].installed.as_str(),
        "1.7.1-3"
    );
    assert_eq!(after.packages[1].targets[&AMD64].installed.as_str(), "14.0");
}

#[test]
fn reported_installed_is_the_version_the_package_manager_reads_or_none() {
    let purl: Purl = "pkg:deb/debian/jq".parse().unwrap();
    let cases = [
        (present("1.7.1-4"), Some(deb("1.7.1-4"))),
        (Installed::Present { version: None }, None),
        (Installed::Absent, None),
    ];
    for (verify, want) in cases {
        let apt = verifying(Scope::System, true, true, verify);
        assert_eq!(reported_installed(&apt, &purl).unwrap(), want);
    }
}
