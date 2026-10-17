//! vers — the Package-URL version range specifier (§5.6), bosun's only range
//! grammar. A spec is a bare version (equality) or vers constraints, written
//! in full (`vers:deb/>=1.7.1|<1.7.2~`) or without the `vers:<type>/` prefix
//! (`>=2.40|<3`, the type supplied by the backend).
//!
//! Version comparison per type maps onto bosun's §6 schemes: `deb`→Debian;
//! `cargo`/`npm`/`golang`/`semver`→SemVer; `apk`→Alpine; `pypi`→PEP 440;
//! everything else→generic chunk ordering. Containment follows the vers algorithm: a version is in the range
//! if it falls in any interval the sorted, alternating-bound constraints form,
//! is an `=` constraint, and is not an `!=` exclusion.
//!
//! Canonical parsing is strict (it rejects non-canonical input: whitespace,
//! empty or mis-piped constraints, an explicit `=` comparator, unsorted
//! constraints). Type-specific canonical rules for ecosystems bosun does not
//! model (datetime, lexicographic whitespace) are out of scope and are
//! reported as skipped conformance cases.

use std::fmt;

use crate::errmsg;
use crate::native;
use crate::version::{Scheme, Version};

/// A vers comparator.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VersOp {
    /// `*` — every version.
    Star,
    /// implicit equality (a bare version); never written as `=` in canonical.
    Eq,
    /// `!=`
    Ne,
    /// `<`
    Lt,
    /// `<=`
    Lte,
    /// `>`
    Gt,
    /// `>=`
    Gte,
}

impl VersOp {
    /// The canonical comparator text (`=` for [`VersOp::Eq`], per the test
    /// suite's component form).
    pub fn as_str(self) -> &'static str {
        match self {
            VersOp::Star => "*",
            VersOp::Eq => "=",
            VersOp::Ne => "!=",
            VersOp::Lt => "<",
            VersOp::Lte => "<=",
            VersOp::Gt => ">",
            VersOp::Gte => ">=",
        }
    }
}

/// One `op version` constraint. `version` is the percent-decoded version text;
/// it is parsed under the range's [`Scheme`] for comparison.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VersConstraint {
    /// The comparator.
    pub op: VersOp,
    /// The version text (empty for [`VersOp::Star`]).
    pub version: String,
}

/// A parsed vers range: its type and its constraints.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VersRange {
    ty: String,
    constraints: Vec<VersConstraint>,
}

/// Why a vers range could not be parsed.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum VersError {
    /// Missing `vers:` scheme or `<type>/` prefix.
    #[error("{kind}: {0}", kind = errmsg::VERS_NO_SCHEME)]
    NoScheme(String),
    /// Not in canonical form (with the specific reason).
    #[error("{kind}: {reason} in {input:?}", kind = errmsg::VERS_NONCANONICAL)]
    NonCanonical {
        /// The offending input.
        input: String,
        /// The canonical rule that was violated.
        reason: &'static str,
    },
    /// A `%HH` escape in a version was malformed.
    #[error("{kind}: {0:?}", kind = errmsg::VERS_BAD_PERCENT)]
    BadPercent(String),
}

/// The §6 version scheme used to compare versions of a given vers type.
pub fn scheme_for_type(ty: &str) -> Scheme {
    match ty {
        "deb" => Scheme::Debian,
        "rpm" => Scheme::Rpm,
        "cargo" | "npm" | "golang" | "semver" => Scheme::Semver,
        "apk" => Scheme::Alpine,
        "pypi" => Scheme::Pep440,
        _ => Scheme::Generic,
    }
}

impl VersRange {
    /// Parse a full canonical vers string, `vers:<type>/<constraints>`.
    ///
    /// Strict: rejects whitespace, an empty or mis-piped constraint list, an
    /// explicit `=`/`==` comparator, unsorted or duplicate versions, and
    /// malformed percent-escapes. (Type-specific canonical rules for schemes
    /// bosun does not model are out of scope — see the module docs.)
    pub fn parse(s: &str) -> Result<VersRange, VersError> {
        let nc = |reason| VersError::NonCanonical {
            input: s.to_string(),
            reason,
        };
        let rest = s
            .strip_prefix("vers:")
            .ok_or_else(|| VersError::NoScheme(s.to_string()))?;
        let (ty, cstr) = rest
            .split_once('/')
            .filter(|(ty, _)| !ty.is_empty())
            .ok_or_else(|| VersError::NoScheme(s.to_string()))?;

        if cstr.is_empty() {
            return Err(nc("constraints must not be empty"));
        }
        if cstr.chars().any(|c| c.is_ascii_whitespace()) {
            return Err(nc("whitespace is not permitted"));
        }
        if cstr.starts_with('|') {
            return Err(nc("leading pipe is not permitted"));
        }
        if cstr.ends_with('|') {
            return Err(nc("trailing pipe is not permitted"));
        }
        if cstr.contains("||") {
            return Err(nc("consecutive pipes are not permitted"));
        }

        let mut constraints = Vec::new();
        for seg in cstr.split('|') {
            constraints.push(parse_constraint(seg, s)?);
        }

        // Canonical order: strictly ascending, unique, by the type's scheme.
        // (`*` is only valid alone; versions parse lazily, so a single
        // constraint with a type-foreign version — e.g. `1.0%2F0` — is kept
        // verbatim without ordering.)
        if constraints.len() > 1 {
            let scheme = scheme_for_type(ty);
            let parsed: Result<Vec<Version>, _> = constraints
                .iter()
                .map(|c| Version::parse(&c.version, scheme))
                .collect();
            let parsed = parsed.map_err(|_| nc("version is not valid for its type"))?;
            for w in parsed.windows(2) {
                if w[0] >= w[1] {
                    return Err(nc("constraints are not sorted and unique by version"));
                }
            }
        }

        Ok(VersRange {
            ty: ty.to_string(),
            constraints,
        })
    }

    /// Whether `version` is contained in this range (the vers algorithm):
    /// `*` matches all; an `=` constraint matches that version; an `!=`
    /// constraint excludes it; the remaining comparator constraints form
    /// sorted, alternating-bound intervals, and a version in any interval is
    /// contained.
    ///
    /// An npm or PyPI range is evaluated as its ecosystem evaluates it
    /// ([`crate::native`]): node-semver's pre-release rule, PEP 440's
    /// exclusive bounds. A version that ecosystem's grammar rejects is
    /// walked as above.
    pub fn contains(&self, version: &Version) -> bool {
        match self.ty.as_str() {
            "npm" => native::npm_contains(self, version),
            "pypi" => native::pep440_contains(self, version),
            _ => None,
        }
        .unwrap_or_else(|| self.interval_contains(version))
    }

    /// [`VersRange::contains`] by the vers algorithm alone.
    fn interval_contains(&self, version: &Version) -> bool {
        let scheme = self.scheme();
        let cver = |c: &VersConstraint| Version::parse(&c.version, scheme).ok();

        // `*` and exact/exclusion first.
        for c in &self.constraints {
            match c.op {
                VersOp::Star => return true,
                VersOp::Eq if cver(c).as_ref() == Some(version) => return true,
                VersOp::Ne if cver(c).as_ref() == Some(version) => return false,
                _ => {}
            }
        }

        // Interval walk over the comparator constraints (already sorted).
        let mut lower: Option<(VersOp, Version)> = None;
        for c in &self.constraints {
            let Some(v) = cver(c) else { continue };
            match c.op {
                VersOp::Gt | VersOp::Gte => lower = Some((c.op, v)),
                VersOp::Lt | VersOp::Lte => {
                    let above_lower = match &lower {
                        None => true,
                        Some((VersOp::Gte, lo)) => version >= lo,
                        Some((_, lo)) => version > lo,
                    };
                    let below_upper = match c.op {
                        VersOp::Lte => version <= &v,
                        _ => version < &v,
                    };
                    if above_lower && below_upper {
                        return true;
                    }
                    lower = None;
                }
                _ => {}
            }
        }
        // A trailing open lower bound: [lower, +inf).
        match lower {
            Some((VersOp::Gte, lo)) => version >= &lo,
            Some((_, lo)) => version > &lo,
            None => false,
        }
    }

    /// The vers type (e.g. `deb`, `npm`).
    pub fn ty(&self) -> &str {
        &self.ty
    }

    /// The §6 scheme versions in this range compare under.
    pub fn scheme(&self) -> Scheme {
        scheme_for_type(&self.ty)
    }

    /// The constraints, in canonical (sorted) order.
    pub fn constraints(&self) -> &[VersConstraint] {
        &self.constraints
    }

    /// Whether any constraint names a pre-release version. Selection admits a
    /// pre-release result only when this holds (§5.6, §12).
    pub fn mentions_prerelease(&self) -> bool {
        let scheme = self.scheme();
        self.constraints.iter().any(|c| {
            Version::parse(&c.version, scheme)
                .map(|v| v.is_prerelease())
                .unwrap_or(false)
        })
    }
}

impl fmt::Display for VersRange {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "vers:{}/", self.ty)?;
        for (i, c) in self.constraints.iter().enumerate() {
            if i > 0 {
                f.write_str("|")?;
            }
            match c.op {
                VersOp::Star => f.write_str("*")?,
                VersOp::Eq => f.write_str(&c.version)?,
                op => write!(f, "{}{}", op.as_str(), c.version)?,
            }
        }
        Ok(())
    }
}

/// Parse one constraint segment into an op and its (percent-decoded) version.
fn parse_constraint(seg: &str, whole: &str) -> Result<VersConstraint, VersError> {
    if seg == "*" {
        return Ok(VersConstraint {
            op: VersOp::Star,
            version: String::new(),
        });
    }
    let (op, raw) = if let Some(r) = seg.strip_prefix("!=") {
        (VersOp::Ne, r)
    } else if seg.starts_with('=') {
        // Covers both `=` and `==`.
        return Err(VersError::NonCanonical {
            input: whole.to_string(),
            reason: "explicit equality comparator is not permitted",
        });
    } else if let Some(r) = seg.strip_prefix(">=") {
        (VersOp::Gte, r)
    } else if let Some(r) = seg.strip_prefix("<=") {
        (VersOp::Lte, r)
    } else if let Some(r) = seg.strip_prefix('>') {
        (VersOp::Gt, r)
    } else if let Some(r) = seg.strip_prefix('<') {
        (VersOp::Lt, r)
    } else {
        (VersOp::Eq, seg)
    };
    Ok(VersConstraint {
        op,
        version: decode_percent(raw, whole)?,
    })
}

/// Percent-decode a version segment once, validating each `%HH` escape. A `%`
/// must be followed by exactly two hex digits.
fn decode_percent(s: &str, whole: &str) -> Result<String, VersError> {
    let bad = || VersError::BadPercent(whole.to_string());
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '%' {
            let (h1, h2) = (chars.next().ok_or_else(bad)?, chars.next().ok_or_else(bad)?);
            let hex: String = [h1, h2].iter().collect();
            let byte = u8::from_str_radix(&hex, 16).map_err(|_| bad())?;
            out.push(byte as char);
        } else {
            out.push(c);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ver(s: &str, scheme: Scheme) -> Version {
        Version::parse(s, scheme).unwrap()
    }

    #[test]
    fn an_apk_range_compares_as_apk_does() {
        let r = VersRange::parse("vers:apk/>=1.7.1|<1.7.2_alpha").unwrap();
        assert_eq!(r.scheme(), Scheme::Alpine);
        let a = |s| ver(s, Scheme::Alpine);
        assert!(r.contains(&a("1.7.1-r0")));
        assert!(r.contains(&a("1.7.1_p2-r3")));
        assert!(!r.contains(&a("1.7.1_rc1-r0")));
        assert!(!r.contains(&a("1.7.2_rc1-r0")));
        // apk has no pre-release concept: `_alpha` is only an ordering.
        assert!(!r.mentions_prerelease());
    }

    #[test]
    fn a_pypi_range_compares_under_pep_440() {
        let r = VersRange::parse("vers:pypi/>=1.0|<2.0").unwrap();
        assert_eq!(r.scheme(), Scheme::Pep440);
        let p = |s| ver(s, Scheme::Pep440);
        assert!(r.contains(&p("1.0.post1")));
        assert!(r.contains(&p("1.10")));
        assert!(!r.contains(&p("1.0rc1")));
        assert!(!r.contains(&p("2.0.0")));
        assert!(!r.mentions_prerelease());
        assert!(VersRange::parse("vers:pypi/>=2.0b1")
            .unwrap()
            .mentions_prerelease());
    }

    #[test]
    fn parses_debian_interval() {
        let r = VersRange::parse("vers:deb/>=1.7.1|<1.7.2~").unwrap();
        assert_eq!(r.ty(), "deb");
        assert_eq!(r.scheme(), Scheme::Debian);
        assert_eq!(r.constraints().len(), 2);
        assert_eq!(r.constraints()[0].op, VersOp::Gte);
        assert_eq!(r.constraints()[1].op, VersOp::Lt);
    }

    #[test]
    fn debian_interval_containment() {
        // "1.7.1, else the next Debian revision of 1.7.1" (§5.6 dogfood).
        let r = VersRange::parse("vers:deb/>=1.7.1|<1.7.2~").unwrap();
        let d = Scheme::Debian;
        assert!(r.contains(&ver("1.7.1", d)));
        assert!(r.contains(&ver("1.7.1-3ubuntu0.1", d)));
        assert!(!r.contains(&ver("1.7.0-9", d))); // below lower
        assert!(!r.contains(&ver("1.7.2", d))); // at/above the ~ upper
    }

    #[test]
    fn rpm_interval_containment() {
        let r = VersRange::parse("vers:rpm/>=1.7.1|<1.7.2~").unwrap();
        assert_eq!(r.scheme(), Scheme::Rpm);
        let v = |s| ver(s, Scheme::Rpm);
        assert!(r.contains(&v("1.7.1-8.fc41")));
        assert!(r.contains(&v("1.7.1-10.fc41")));
        assert!(!r.contains(&v("1.7.1~rc1-1.fc41")));
        assert!(!r.contains(&v("1.7.2-1.fc41")));
        assert!(!r.contains(&v("1:1.7.1-1.fc41"))); // a higher epoch is above
    }

    /// The vers walk (cargo) and node-semver (npm) agree on release
    /// versions, so each containment case runs under both.
    const SEMVER_TYPES: [&str; 2] = ["cargo", "npm"];

    fn semver_range(ty: &str, constraints: &str) -> VersRange {
        VersRange::parse(&format!("vers:{ty}/{constraints}")).unwrap()
    }

    #[test]
    fn bare_version_is_equality() {
        for ty in SEMVER_TYPES {
            let r = semver_range(ty, "1.0.0");
            assert_eq!(
                r.constraints(),
                &[VersConstraint {
                    op: VersOp::Eq,
                    version: "1.0.0".into()
                }]
            );
            assert!(r.contains(&ver("1.0.0", Scheme::Semver)), "{ty}");
            assert!(!r.contains(&ver("1.0.1", Scheme::Semver)), "{ty}");
            let two = semver_range(ty, "1.0.0|2.0.0");
            assert!(two.contains(&ver("2.0.0", Scheme::Semver)), "{ty}");
            assert!(!two.contains(&ver("1.5.0", Scheme::Semver)), "{ty}");
        }
    }

    #[test]
    fn open_bounds() {
        for ty in SEMVER_TYPES {
            let lo = semver_range(ty, ">=1.0.0");
            assert!(lo.contains(&ver("2.0.0", Scheme::Semver)), "{ty}");
            assert!(lo.contains(&ver("1.0.0", Scheme::Semver)), "{ty}");
            assert!(!lo.contains(&ver("0.9.0", Scheme::Semver)), "{ty}");
            let hi = semver_range(ty, "<2.0.0");
            assert!(hi.contains(&ver("1.0.0", Scheme::Semver)), "{ty}");
            assert!(!hi.contains(&ver("2.0.0", Scheme::Semver)), "{ty}");
            let hi_incl = semver_range(ty, "<=2.0.0");
            assert!(hi_incl.contains(&ver("2.0.0", Scheme::Semver)), "{ty}");
            assert!(!hi_incl.contains(&ver("2.0.1", Scheme::Semver)), "{ty}");
        }
    }

    #[test]
    fn not_equal_excludes() {
        for ty in SEMVER_TYPES {
            let r = semver_range(ty, ">=1.0.0|!=1.5.0|<2.0.0");
            assert!(r.contains(&ver("1.4.0", Scheme::Semver)), "{ty}");
            assert!(!r.contains(&ver("1.5.0", Scheme::Semver)), "{ty}");
            assert!(r.contains(&ver("1.5.1", Scheme::Semver)), "{ty}");
        }
    }

    #[test]
    fn star_contains_everything() {
        for ty in SEMVER_TYPES {
            let r = semver_range(ty, "*");
            assert!(r.contains(&ver("0.0.1", Scheme::Semver)), "{ty}");
        }
    }

    #[test]
    fn several_intervals_each_contain_their_own() {
        for ty in SEMVER_TYPES {
            let r = semver_range(ty, ">1.0.0|<=1.2.0|>2.0.0|<3.0.0");
            assert!(!r.contains(&ver("1.0.0", Scheme::Semver)), "{ty}");
            assert!(r.contains(&ver("1.2.0", Scheme::Semver)), "{ty}");
            assert!(!r.contains(&ver("1.5.0", Scheme::Semver)), "{ty}");
            assert!(!r.contains(&ver("2.0.0", Scheme::Semver)), "{ty}");
            assert!(r.contains(&ver("2.5.0", Scheme::Semver)), "{ty}");
            assert!(!r.contains(&ver("3.0.0", Scheme::Semver)), "{ty}");
        }
    }

    #[test]
    fn rejects_explicit_equality() {
        assert!(VersRange::parse("vers:npm/=1.0.0").is_err());
        assert!(VersRange::parse("vers:npm/==1.0.0").is_err());
    }

    #[test]
    fn rejects_unsorted() {
        assert!(matches!(
            VersRange::parse("vers:npm/>=2.0.0|<1.0.0"),
            Err(VersError::NonCanonical { .. })
        ));
    }

    #[test]
    fn rejects_structural_violations() {
        for bad in [
            "vers:npm/",
            "vers:npm/|>=1.0.0",
            "vers:npm/>=1.0.0|",
            "vers:npm/a||b",
        ] {
            assert!(VersRange::parse(bad).is_err(), "should reject {bad:?}");
        }
        assert!(matches!(
            VersRange::parse("no-scheme"),
            Err(VersError::NoScheme(_))
        ));
    }

    #[test]
    fn decodes_percent_once() {
        let r = VersRange::parse("vers:npm/1.0%252F0").unwrap();
        assert_eq!(r.constraints()[0].version, "1.0%2F0");
        assert!(matches!(
            VersRange::parse("vers:npm/1.0%2G0"),
            Err(VersError::BadPercent(_))
        ));
    }

    #[test]
    fn decode_percent_edges() {
        // A `%HH` not at the start decodes; a truncated `%` is rejected.
        assert_eq!(
            VersRange::parse("vers:npm/12%2F").unwrap().constraints()[0].version,
            "12/"
        );
        for bad in ["vers:npm/1.0%", "vers:npm/1.0%2"] {
            assert!(
                matches!(VersRange::parse(bad), Err(VersError::BadPercent(_))),
                "{bad}"
            );
        }
    }

    #[test]
    fn strict_lower_bound_excludes_its_boundary() {
        for ty in SEMVER_TYPES {
            let r = semver_range(ty, ">1.0.0|<2.0.0");
            assert!(!r.contains(&ver("1.0.0", Scheme::Semver)), "{ty}"); // strict `>`
            assert!(r.contains(&ver("1.0.1", Scheme::Semver)), "{ty}");
        }
    }

    #[test]
    fn upper_bound_inclusive_vs_exclusive() {
        for ty in SEMVER_TYPES {
            let incl = semver_range(ty, ">=1.0.0|<=2.0.0");
            assert!(incl.contains(&ver("2.0.0", Scheme::Semver)), "{ty}"); // `<=` includes
            let excl = semver_range(ty, ">=1.0.0|<2.0.0");
            assert!(!excl.contains(&ver("2.0.0", Scheme::Semver)), "{ty}"); // `<` excludes
            assert!(excl.contains(&ver("1.0.0", Scheme::Semver)), "{ty}");
        }
    }

    #[test]
    fn strict_open_lower_bound() {
        for ty in SEMVER_TYPES {
            let r = semver_range(ty, ">1.0.0");
            assert!(r.contains(&ver("2.0.0", Scheme::Semver)), "{ty}");
            assert!(!r.contains(&ver("1.0.0", Scheme::Semver)), "{ty}"); // strict, no upper
        }
    }

    #[test]
    fn mentions_prerelease_detects_constraint_prereleases() {
        assert!(VersRange::parse("vers:npm/>=1.0.0-rc1")
            .unwrap()
            .mentions_prerelease());
        assert!(!VersRange::parse("vers:npm/>=1.0.0")
            .unwrap()
            .mentions_prerelease());
    }

    #[test]
    fn display_roundtrips_canonical() {
        for s in ["vers:deb/>=1.7.1|<1.7.2~", "vers:npm/1.0.0", "vers:npm/*"] {
            assert_eq!(VersRange::parse(s).unwrap().to_string(), s);
        }
    }
}
