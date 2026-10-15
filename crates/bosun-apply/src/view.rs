//! The plan's JSON wire format (`--format json`, §4.8). bosun-io owns the
//! lock/state wire format; this owns the plan's, so the plan's domain types
//! (which apply executes) stay free of serde. One-way: apply runs the in-memory
//! value, so there is no `Deserialize`.

use bosun_core::{Claim, Scope};
use bosun_ports::step::Step;
use serde::{Serialize, Serializer};

use crate::{Action, ActionOutcome, ApplyReport, ApplyResult, Diagnostic, Plan, Severity};

fn scope_label(scope: Scope) -> &'static str {
    match scope {
        Scope::System => "system",
        Scope::User => "user",
    }
}

#[derive(Serialize)]
struct PlanView<'a> {
    actions: Vec<ActionView<'a>>,
    diagnostics: Vec<DiagView<'a>>,
    exit_code: i32,
}

#[derive(Serialize)]
struct ActionView<'a> {
    action: &'static str,
    name: &'a str,
    scope: &'static str,
    backend: &'a str,
    purl: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    from: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    to: Option<String>,
    needs_elevation: bool,
    claims: Vec<ClaimView<'a>>,
    steps: Vec<StepView<'a>>,
}

#[derive(Serialize)]
struct ClaimView<'a> {
    manifest: String,
    spec: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    user: Option<&'a str>,
}

#[derive(Serialize)]
#[serde(tag = "step", rename_all = "lowercase")]
enum StepView<'a> {
    Run {
        argv: &'a [String],
        env: Vec<[&'a str; 2]>,
        scope: &'static str,
    },
    Fetch {
        url: &'a str,
        #[serde(skip_serializing_if = "Option::is_none")]
        sha256: Option<&'a str>,
        dest: String,
    },
    Extract {
        archive: String,
        member: &'a str,
        dest: String,
        mode: u32,
    },
}

#[derive(Serialize)]
struct DiagView<'a> {
    diagnostic: &'static str,
    severity: &'static str,
    name: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    detail: Option<String>,
}

fn claim_view(c: &Claim) -> ClaimView<'_> {
    ClaimView {
        manifest: c.manifest.display().to_string(),
        spec: &c.spec,
        user: c.user.as_deref(),
    }
}

fn step_view(step: &Step) -> StepView<'_> {
    match step {
        Step::Run { argv, env, scope } => StepView::Run {
            argv,
            env: env.iter().map(|(k, v)| [k.as_str(), v.as_str()]).collect(),
            scope: scope_label(*scope),
        },
        Step::Fetch { url, sha256, dest } => StepView::Fetch {
            url,
            sha256: sha256.as_deref(),
            dest: dest.display().to_string(),
        },
        Step::Extract {
            archive,
            member,
            dest,
            mode,
        } => StepView::Extract {
            archive: archive.display().to_string(),
            member,
            dest: dest.display().to_string(),
            mode: *mode,
        },
    }
}

fn action_view(a: &Action) -> ActionView<'_> {
    ActionView {
        action: a.kind.label(),
        name: &a.name,
        scope: scope_label(a.scope),
        backend: a.backend.as_str(),
        purl: a.purl.to_string(),
        from: a.from.as_ref().map(|v| v.as_str().to_string()),
        to: a.to.as_ref().map(|v| v.as_str().to_string()),
        needs_elevation: a.needs_elevation(),
        claims: a.claims.iter().map(claim_view).collect(),
        steps: a.steps.iter().map(step_view).collect(),
    }
}

fn diag_view(d: &Diagnostic) -> DiagView<'_> {
    let severity = match d.severity() {
        Severity::Error => "error",
        Severity::Warning => "warning",
    };
    let (diagnostic, name, detail) = match d {
        Diagnostic::NotApplicable { name } => ("not-applicable", name.as_str(), None),
        Diagnostic::MissingBackend { name, candidates } => (
            "missing-backend",
            name.as_str(),
            Some(
                candidates
                    .iter()
                    .map(|b| b.as_str())
                    .collect::<Vec<_>>()
                    .join(", "),
            ),
        ),
        Diagnostic::Unsatisfiable {
            name,
            spec,
            available,
        } => (
            "unsatisfiable",
            name.as_str(),
            Some(format!("{spec} (available: {available})")),
        ),
        Diagnostic::EmptyIntersection { name, claims } => {
            ("empty-intersection", name.as_str(), Some(claims.clone()))
        }
        Diagnostic::AcceptedUnsatisfied {
            name,
            installed,
            spec,
        } => (
            "accepted-unsatisfied",
            name.as_str(),
            Some(format!("{installed} does not satisfy {spec}")),
        ),
        Diagnostic::UnsupportedHold { name, backend } => (
            "unsupported-hold",
            name.as_str(),
            Some(backend.as_str().to_string()),
        ),
        Diagnostic::UnsupportedDowngrade { name, backend } => (
            "unsupported-downgrade",
            name.as_str(),
            Some(backend.as_str().to_string()),
        ),
        Diagnostic::FrozenAcceptedConflict {
            name,
            installed,
            locked,
        } => (
            "frozen-accepted-conflict",
            name.as_str(),
            Some(format!("installed {installed}, lock pins {locked}")),
        ),
    };
    DiagView {
        diagnostic,
        severity,
        name,
        detail,
    }
}

impl Serialize for Action {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        action_view(self).serialize(s)
    }
}

impl Serialize for Plan {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        PlanView {
            actions: self.actions.iter().map(action_view).collect(),
            diagnostics: self.diagnostics.iter().map(diag_view).collect(),
            exit_code: self.exit_code(),
        }
        .serialize(s)
    }
}

#[derive(Serialize)]
struct ReportView<'a> {
    outcomes: Vec<OutcomeView<'a>>,
}

#[derive(Serialize)]
struct OutcomeView<'a> {
    name: &'a str,
    action: &'static str,
    result: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<&'a str>,
}

fn outcome_view(o: &ActionOutcome) -> OutcomeView<'_> {
    let (result, error) = match &o.result {
        ApplyResult::Applied => ("applied", None),
        ApplyResult::Failed { error } => ("failed", Some(error.as_str())),
        ApplyResult::Skipped => ("skipped", None),
    };
    OutcomeView {
        name: &o.name,
        action: o.kind.label(),
        result,
        error,
    }
}

impl Serialize for ApplyReport {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        ReportView {
            outcomes: self.outcomes.iter().map(outcome_view).collect(),
        }
        .serialize(s)
    }
}
