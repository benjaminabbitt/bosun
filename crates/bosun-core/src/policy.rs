//! Resolution policy (§7): how a written spec picks a concrete version from
//! the set a backend reports as available.
//!
//! Three policies, orthogonal to the backend:
//!
//! - [`Policy::Exact`] — the version must be present verbatim, or resolution
//!   fails. Reproducible builds.
//! - [`Policy::Range`] — a constraint (`^1.2`, `>=1.2,<2`, `1.2.*`); resolve
//!   to the **highest** available version satisfying it.
//! - [`Policy::Tolerant`] — prefer the pin; if it is gone, round **up** to the
//!   next-higher available version, never below it, and record a
//!   [`Deviation`]. This is what makes a pin-ish intent usable on repo-bound
//!   backends that keep only the latest versions.
//!
//! All ordering and "highest"/"closest" selection ride on the total order
//! [`Version`] defines per scheme (§6); getting that right is what buys this
//! module its correctness.
//!
//! Range grammar is scheme-aware: under [`Scheme::Semver`] the full
//! `semver::VersionReq` grammar applies (`^`, `~`, `*`, comparators, and the
//! SemVer pre-release rule — a range admits pre-releases only when it names
//! one). Under every other scheme a range is a comma-separated list of
//! comparator clauses (`>=`, `>`, `<=`, `<`, `=`, or a bare version meaning
//! `=`), AND-ed together.

use std::cmp::Ordering;

use serde::{Deserialize, Serialize};

use crate::errmsg;
use crate::version::{Scheme, Version, VersionError};

/// How a spec string is interpreted when selecting a version (§7).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Policy {
    /// Must be exactly the written version, or fail.
    Exact,
    /// A constraint; resolve to the highest available version satisfying it.
    Range,
    /// Prefer the pin; else round up to the next-higher available version.
    Tolerant,
}

/// How a resolved version differs from what the spec literally asked for.
/// Absent when the selected version is exactly the written one.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Deviation {
    /// A tolerant pin was unavailable; bosun rounded up to the closest
    /// higher available version (§7, never below).
    NextHigher,
}

/// Why a spec string could not be understood.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum SpecError {
    /// A range the grammar could not parse.
    #[error("{kind}: {0}", kind = errmsg::SPEC_INVALID_RANGE)]
    InvalidRange(String),
    /// A comparator clause with no valid operator or version.
    #[error("{kind}: {0}", kind = errmsg::SPEC_INVALID_COMPARATOR)]
    InvalidComparator(String),
    /// The pin (exact/tolerant) was not a valid version under its scheme.
    #[error(transparent)]
    Version(#[from] VersionError),
}

/// Why resolution could not select a version.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ResolveError {
    /// The backend reported no available versions at all.
    #[error("{}", errmsg::RESOLVE_NO_AVAILABLE)]
    NoneAvailable,
    /// Versions exist, but none satisfy the spec under its policy.
    #[error("{kind}: {spec} ({policy:?})", kind = errmsg::RESOLVE_UNSATISFIABLE)]
    Unsatisfiable {
        /// The spec string that could not be satisfied.
        spec: String,
        /// The policy it was resolved under.
        policy: Policy,
    },
}

/// A comparator operator in a non-SemVer range clause.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Op {
    Gte,
    Gt,
    Lte,
    Lt,
    Eq,
}

/// One `op version` clause of a non-SemVer range.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Comparator {
    op: Op,
    bound: Version,
}

impl Comparator {
    /// Whether `v` satisfies this clause. A scheme mismatch never matches.
    fn satisfied(&self, v: &Version) -> bool {
        match v.try_cmp(&self.bound) {
            Some(ord) => match self.op {
                Op::Gte => ord != Ordering::Less,
                Op::Gt => ord == Ordering::Greater,
                Op::Lte => ord != Ordering::Greater,
                Op::Lt => ord == Ordering::Less,
                Op::Eq => ord == Ordering::Equal,
            },
            None => false,
        }
    }
}

/// A parsed range constraint, scheme-aware (see module docs).
#[derive(Clone, Debug, PartialEq, Eq)]
enum Constraint {
    /// SemVer grammar, via the `semver` crate.
    Semver(semver::VersionReq),
    /// AND-ed comparator clauses, for every other scheme.
    Comparators(Vec<Comparator>),
}

impl Constraint {
    fn parse(raw: &str, scheme: Scheme) -> Result<Constraint, SpecError> {
        if scheme == Scheme::Semver {
            return semver::VersionReq::parse(raw)
                .map(Constraint::Semver)
                .map_err(|e| SpecError::InvalidRange(e.to_string()));
        }
        let clauses = raw
            .split(',')
            .map(|clause| parse_comparator(clause, scheme))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Constraint::Comparators(clauses))
    }

    fn matches(&self, v: &Version) -> bool {
        match self {
            // A SemVer range only applies to a SemVer version; `as_semver`
            // yields `None` for any other scheme, which never matches.
            Constraint::Semver(req) => v.as_semver().is_some_and(|sv| req.matches(sv)),
            Constraint::Comparators(cs) => cs.iter().all(|c| c.satisfied(v)),
        }
    }
}

/// Parse one `op version` clause (`>=2.40`, `<3.0`, or a bare `2.43.0`).
fn parse_comparator(clause: &str, scheme: Scheme) -> Result<Comparator, SpecError> {
    let clause = clause.trim();
    let (op, rest) = if let Some(r) = clause.strip_prefix(">=") {
        (Op::Gte, r)
    } else if let Some(r) = clause.strip_prefix("<=") {
        (Op::Lte, r)
    } else if let Some(r) = clause.strip_prefix('>') {
        (Op::Gt, r)
    } else if let Some(r) = clause.strip_prefix('<') {
        (Op::Lt, r)
    } else if let Some(r) = clause.strip_prefix('=') {
        (Op::Eq, r)
    } else {
        (Op::Eq, clause)
    };
    let bound = Version::parse(rest.trim(), scheme)
        .map_err(|_| SpecError::InvalidComparator(clause.to_string()))?;
    Ok(Comparator { op, bound })
}

/// The interpreted spec: the exact kernel each policy resolves with. Keeping
/// the policy and its parsed form in one enum makes an invalid pairing
/// (e.g. `Range` with a lone pin) unrepresentable.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Plan {
    Exact(Version),
    Range(Constraint),
    Tolerant(Version),
}

/// A version spec: the written string, the scheme it is read under, and the
/// policy that governs how it selects from available versions.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Spec {
    raw: String,
    scheme: Scheme,
    plan: Plan,
}

impl Spec {
    /// Parse `raw` under `scheme` and `policy`. Validates eagerly: a bad range
    /// or an unparseable exact/tolerant pin is an error here, so
    /// [`Spec::resolve`] fails only on unsatisfiability, never on parsing.
    pub fn new(raw: impl Into<String>, scheme: Scheme, policy: Policy) -> Result<Spec, SpecError> {
        let raw = raw.into();
        let plan = match policy {
            Policy::Exact => Plan::Exact(Version::parse(&raw, scheme)?),
            Policy::Tolerant => Plan::Tolerant(Version::parse(&raw, scheme)?),
            Policy::Range => Plan::Range(Constraint::parse(&raw, scheme)?),
        };
        Ok(Spec { raw, scheme, plan })
    }

    /// The spec string exactly as written.
    pub fn as_str(&self) -> &str {
        &self.raw
    }

    /// The scheme this spec is read under.
    pub fn scheme(&self) -> Scheme {
        self.scheme
    }

    /// The policy governing this spec.
    pub fn policy(&self) -> Policy {
        match self.plan {
            Plan::Exact(_) => Policy::Exact,
            Plan::Range(_) => Policy::Range,
            Plan::Tolerant(_) => Policy::Tolerant,
        }
    }

    /// Select a version from `available` under this spec's policy (§7).
    ///
    /// `available` is the set a backend reports for one package on one target;
    /// every version shares this spec's scheme. Returns the selected version
    /// and any [`Deviation`] from the literal spec.
    pub fn resolve(&self, available: &[Version]) -> Result<Resolution, ResolveError> {
        if available.is_empty() {
            return Err(ResolveError::NoneAvailable);
        }
        match &self.plan {
            Plan::Exact(pin) => available
                .iter()
                .find(|v| *v == pin)
                .map(|v| Resolution::exact(v.clone()))
                .ok_or_else(|| self.unsatisfiable()),
            Plan::Range(c) => available
                .iter()
                .filter(|v| c.matches(v))
                .max()
                .map(|v| Resolution::exact(v.clone()))
                .ok_or_else(|| self.unsatisfiable()),
            Plan::Tolerant(pin) => {
                // The smallest available version at or above the pin, never
                // below it (§7). When the pin itself is present it is that
                // minimum (nothing smaller is `>= pin`), so it wins with no
                // deviation; otherwise bosun rounds up to the closest higher.
                match available.iter().filter(|v| *v >= pin).min() {
                    None => Err(self.unsatisfiable()),
                    Some(v) if v == pin => Ok(Resolution::exact(v.clone())),
                    Some(v) => Ok(Resolution {
                        selected: v.clone(),
                        deviation: Some(Deviation::NextHigher),
                    }),
                }
            }
        }
    }

    fn unsatisfiable(&self) -> ResolveError {
        ResolveError::Unsatisfiable {
            spec: self.raw.clone(),
            policy: self.policy(),
        }
    }
}

/// The outcome of resolving a [`Spec`] against available versions.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Resolution {
    /// The version selected.
    pub selected: Version,
    /// How it differs from the literal spec, if at all.
    pub deviation: Option<Deviation>,
}

impl Resolution {
    /// A resolution that matches the spec exactly (no deviation).
    fn exact(selected: Version) -> Resolution {
        Resolution {
            selected,
            deviation: None,
        }
    }
}

/// A manifest entry's resolvable intent: a logical name (§5.4) and its spec.
/// Per-target backend/package selection is layered on in later slices.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    /// bosun's own logical name for the tool.
    pub name: String,
    /// The version spec and policy.
    pub spec: Spec,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vers(scheme: Scheme, raw: &[&str]) -> Vec<Version> {
        raw.iter()
            .map(|s| Version::parse(*s, scheme).unwrap())
            .collect()
    }

    // --- exact -------------------------------------------------------------

    #[test]
    fn exact_resolves_when_present() {
        let spec = Spec::new("1.7.1-3", Scheme::Debian, Policy::Exact).unwrap();
        let available = vers(Scheme::Debian, &["1.7.0", "1.7.1-3", "1.8.0"]);
        let r = spec.resolve(&available).unwrap();
        assert_eq!(r.selected.as_str(), "1.7.1-3");
        assert_eq!(r.deviation, None);
    }

    #[test]
    fn exact_fails_when_absent() {
        let spec = Spec::new("1.7.1-3", Scheme::Debian, Policy::Exact).unwrap();
        let available = vers(Scheme::Debian, &["1.7.0", "1.8.0"]);
        assert!(matches!(
            spec.resolve(&available),
            Err(ResolveError::Unsatisfiable { .. })
        ));
    }

    #[test]
    fn exact_reports_none_available_on_empty() {
        let spec = Spec::new("1.0.0", Scheme::Semver, Policy::Exact).unwrap();
        assert_eq!(spec.resolve(&[]), Err(ResolveError::NoneAvailable));
    }

    // --- range -------------------------------------------------------------

    #[test]
    fn range_picks_highest_satisfying() {
        let spec = Spec::new(">=1.2.0, <2.0.0", Scheme::Semver, Policy::Range).unwrap();
        let available = vers(Scheme::Semver, &["1.2.0", "1.5.0", "1.9.3", "2.0.0"]);
        let r = spec.resolve(&available).unwrap();
        assert_eq!(r.selected.as_str(), "1.9.3");
        assert_eq!(r.deviation, None);
    }

    #[test]
    fn range_semver_caret_excludes_next_major() {
        let spec = Spec::new("^1.2", Scheme::Semver, Policy::Range).unwrap();
        let available = vers(Scheme::Semver, &["1.2.0", "1.9.0", "2.0.0"]);
        assert_eq!(spec.resolve(&available).unwrap().selected.as_str(), "1.9.0");
    }

    #[test]
    fn range_semver_excludes_prerelease_unless_named() {
        // SemVer rule: a plain range does not admit pre-releases.
        let spec = Spec::new("^1.2", Scheme::Semver, Policy::Range).unwrap();
        let available = vers(Scheme::Semver, &["1.2.0", "1.9.0-rc1"]);
        assert_eq!(spec.resolve(&available).unwrap().selected.as_str(), "1.2.0");
    }

    #[test]
    fn range_fails_when_none_satisfy() {
        let spec = Spec::new(">=3.0.0", Scheme::Semver, Policy::Range).unwrap();
        let available = vers(Scheme::Semver, &["1.0.0", "2.5.0"]);
        assert!(matches!(
            spec.resolve(&available),
            Err(ResolveError::Unsatisfiable { .. })
        ));
    }

    #[test]
    fn range_debian_comparator_conjunction() {
        let spec = Spec::new(">=2.40, <3.0", Scheme::Debian, Policy::Range).unwrap();
        let available = vers(Scheme::Debian, &["2.39", "2.43.0", "3.0", "3.1"]);
        assert_eq!(
            spec.resolve(&available).unwrap().selected.as_str(),
            "2.43.0"
        );
    }

    #[test]
    fn range_gt_comparator_excludes_equal() {
        let spec = Spec::new(">2.40", Scheme::Debian, Policy::Range).unwrap();
        let available = vers(Scheme::Debian, &["2.39", "2.40", "2.41"]);
        assert_eq!(spec.resolve(&available).unwrap().selected.as_str(), "2.41");
    }

    #[test]
    fn range_lte_comparator_includes_equal() {
        let spec = Spec::new("<=2.40", Scheme::Debian, Policy::Range).unwrap();
        let available = vers(Scheme::Debian, &["2.39", "2.40", "2.41"]);
        assert_eq!(spec.resolve(&available).unwrap().selected.as_str(), "2.40");
    }

    #[test]
    fn range_bare_comparator_means_equal() {
        let spec = Spec::new("2.43.0", Scheme::Debian, Policy::Range).unwrap();
        let available = vers(Scheme::Debian, &["2.42.0", "2.43.0", "2.44.0"]);
        assert_eq!(
            spec.resolve(&available).unwrap().selected.as_str(),
            "2.43.0"
        );
    }

    // --- tolerant ----------------------------------------------------------

    #[test]
    fn tolerant_resolves_pin_without_deviation() {
        let spec = Spec::new("10.2.0", Scheme::Semver, Policy::Tolerant).unwrap();
        let available = vers(Scheme::Semver, &["10.1.0", "10.2.0", "10.5.0"]);
        let r = spec.resolve(&available).unwrap();
        assert_eq!(r.selected.as_str(), "10.2.0");
        assert_eq!(r.deviation, None);
    }

    #[test]
    fn tolerant_rounds_up_to_closest_higher() {
        let spec = Spec::new("10.2.0", Scheme::Semver, Policy::Tolerant).unwrap();
        // Pin absent; 10.3.0 is the closest higher, not the highest.
        let available = vers(Scheme::Semver, &["10.1.0", "10.3.0", "10.5.0", "11.0.0"]);
        let r = spec.resolve(&available).unwrap();
        assert_eq!(r.selected.as_str(), "10.3.0");
        assert_eq!(r.deviation, Some(Deviation::NextHigher));
    }

    #[test]
    fn tolerant_never_rounds_below_pin() {
        let spec = Spec::new("10.2.0", Scheme::Semver, Policy::Tolerant).unwrap();
        // Only lower versions exist: refuse rather than downgrade.
        let available = vers(Scheme::Semver, &["9.0.0", "10.1.0"]);
        assert!(matches!(
            spec.resolve(&available),
            Err(ResolveError::Unsatisfiable { .. })
        ));
    }

    // --- spec parsing ------------------------------------------------------

    #[test]
    fn new_rejects_invalid_semver_range() {
        assert!(matches!(
            Spec::new("not a range", Scheme::Semver, Policy::Range),
            Err(SpecError::InvalidRange(_))
        ));
    }

    #[test]
    fn new_rejects_empty_exact_pin() {
        assert!(matches!(
            Spec::new("", Scheme::Debian, Policy::Exact),
            Err(SpecError::Version(VersionError::Empty))
        ));
    }

    #[test]
    fn new_rejects_garbage_comparator() {
        assert!(matches!(
            Spec::new(">=", Scheme::Debian, Policy::Range),
            Err(SpecError::InvalidComparator(_))
        ));
    }

    #[test]
    fn as_str_returns_the_written_spec() {
        assert_eq!(
            Spec::new("^1.2", Scheme::Semver, Policy::Range)
                .unwrap()
                .as_str(),
            "^1.2"
        );
    }

    #[test]
    fn policy_accessor_reflects_construction() {
        assert_eq!(
            Spec::new("1.0.0", Scheme::Semver, Policy::Exact)
                .unwrap()
                .policy(),
            Policy::Exact
        );
        assert_eq!(
            Spec::new("^1.0", Scheme::Semver, Policy::Range)
                .unwrap()
                .policy(),
            Policy::Range
        );
        assert_eq!(
            Spec::new("1.0.0", Scheme::Semver, Policy::Tolerant)
                .unwrap()
                .policy(),
            Policy::Tolerant
        );
    }

    // --- serde wire formats (§4.5) -----------------------------------------

    #[test]
    fn policy_serializes_lowercase() {
        assert_eq!(serde_json::to_string(&Policy::Exact).unwrap(), "\"exact\"");
        assert_eq!(serde_json::to_string(&Policy::Range).unwrap(), "\"range\"");
        assert_eq!(
            serde_json::to_string(&Policy::Tolerant).unwrap(),
            "\"tolerant\""
        );
    }

    #[test]
    fn deviation_serializes_kebab_case() {
        assert_eq!(
            serde_json::to_string(&Deviation::NextHigher).unwrap(),
            "\"next-higher\""
        );
    }

    #[test]
    fn unsatisfiable_message_names_spec_and_policy() {
        let spec = Spec::new("1.7.1-3", Scheme::Debian, Policy::Exact).unwrap();
        let err = spec.resolve(&vers(Scheme::Debian, &["1.8.0"])).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("1.7.1-3"), "message was: {msg}");
        assert!(msg.contains("Exact"), "message was: {msg}");
    }
}
