//! vers ranges in an ecosystem's own range language (§5.6), so the range a
//! spec stores as vers is evaluated, and handed to the ecosystem's tool, the
//! way that ecosystem reads it.
//!
//! A vers range is a union of pieces: `*`, single versions (`=`), and
//! intervals the sorted comparators bound, less the `!=` exclusions (the
//! containment algorithm in [`crate::vers`]). Each piece maps onto the
//! native grammar:
//!
//! - npm (node-semver): a piece is a comparator set (`>=1.2.0 <2.0.0`, a
//!   bare `1.2.0`, `*`); pieces join with ` || `. node-semver has no `!=`,
//!   so an excluded version splits the interval holding it in two. A range
//!   with no piece is `<0.0.0-0`, which nothing satisfies. Evaluation is
//!   node-semver's (the `nodejs-semver` crate): a pre-release satisfies a set
//!   only when one of its bounds is a pre-release of the same
//!   `major.minor.patch`.
//! - PyPI (PEP 440): a piece is a specifier set (`>=1.2,<2`, `==1.2`; `*` is
//!   the empty set), carrying every exclusion as `!=`. PEP 440 has no union,
//!   so a range is a list of sets, satisfied when any is. Evaluation is
//!   `pep440_rs`'s: `<V` admits no pre-release of `V`, `>V` no post-release
//!   of `V`, `==V` ignores a local label.
//!
//! Several ranges together (every claim's, §4.6) are their intersection:
//! each combination of one piece per range, its comparators together.

use std::str::FromStr;

use crate::vers::{VersConstraint, VersOp, VersRange};
use crate::version::Version;

/// One piece of a vers range.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Piece {
    /// Every version.
    All,
    /// One version.
    Point(String),
    /// The versions between two bounds, each `(inclusive, version)`.
    Interval {
        lower: Option<(bool, String)>,
        upper: Option<(bool, String)>,
    },
}

/// A vers range as its pieces and exclusions, read as [`VersRange::contains`]
/// reads it.
struct Pieces {
    pieces: Vec<Piece>,
    excluded: Vec<String>,
}

fn pieces(range: &VersRange) -> Pieces {
    let mut pieces = Vec::new();
    let mut excluded = Vec::new();
    let mut lower: Option<(bool, String)> = None;
    for VersConstraint { op, version } in range.constraints() {
        let v = version.clone();
        match op {
            VersOp::Star => pieces.push(Piece::All),
            VersOp::Eq => pieces.push(Piece::Point(v)),
            VersOp::Ne => excluded.push(v),
            VersOp::Gt => lower = Some((false, v)),
            VersOp::Gte => lower = Some((true, v)),
            VersOp::Lt | VersOp::Lte => pieces.push(Piece::Interval {
                lower: lower.take(),
                upper: Some((*op == VersOp::Lte, v)),
            }),
        }
    }
    if lower.is_some() {
        pieces.push(Piece::Interval { lower, upper: None });
    }
    Pieces { pieces, excluded }
}

/// Whether `x` lies inside `piece`'s bounds (an exclusion there splits it),
/// compared under `range`'s scheme. A canonical range never repeats a
/// version, so `x` is never a bound itself and the bounds compare strictly.
fn inside(piece: &Piece, x: &str, range: &VersRange) -> bool {
    let scheme = range.scheme();
    let Ok(x) = Version::parse(x, scheme) else {
        return false;
    };
    let parse = |s: &str| Version::parse(s, scheme).ok();
    match piece {
        Piece::All => true,
        Piece::Point(_) => false,
        Piece::Interval { lower, upper } => {
            let above = lower
                .as_ref()
                .is_none_or(|(_, lo)| parse(lo).is_some_and(|lo| x > lo));
            let below = upper
                .as_ref()
                .is_none_or(|(_, up)| parse(up).is_some_and(|up| x < up));
            above && below
        }
    }
}

/// Split `piece` around `x`: below it, and above it.
fn split(piece: &Piece, x: &str) -> [Piece; 2] {
    let (lower, upper) = match piece {
        Piece::Interval { lower, upper } => (lower.clone(), upper.clone()),
        _ => (None, None),
    };
    [
        Piece::Interval {
            lower,
            upper: Some((false, x.to_string())),
        },
        Piece::Interval {
            lower: Some((false, x.to_string())),
            upper,
        },
    ]
}

/// One npm comparator set for `piece`.
fn npm_set(piece: &Piece) -> String {
    match piece {
        Piece::All => "*".to_string(),
        Piece::Point(v) => v.clone(),
        Piece::Interval { lower, upper } => {
            let mut parts = Vec::new();
            if let Some((incl, v)) = lower {
                parts.push(format!("{}{v}", if *incl { ">=" } else { ">" }));
            }
            if let Some((incl, v)) = upper {
                parts.push(format!("{}{v}", if *incl { "<=" } else { "<" }));
            }
            if parts.is_empty() {
                "*".to_string()
            } else {
                parts.join(" ")
            }
        }
    }
}

/// The npm comparator sets for `range`, exclusions split out.
fn npm_sets(range: &VersRange) -> Vec<String> {
    let Pieces { pieces, excluded } = pieces(range);
    let mut out = pieces;
    for x in &excluded {
        out = out
            .into_iter()
            .flat_map(|p| {
                if inside(&p, x, range) {
                    split(&p, x).to_vec()
                } else {
                    vec![p]
                }
            })
            .collect();
    }
    out.iter().map(npm_set).collect()
}

/// What node-semver reads as no version at all.
const NPM_NOTHING: &str = "<0.0.0-0";

/// Join comparator sets into one node-semver range.
fn npm_join(sets: Vec<String>) -> String {
    if sets.is_empty() {
        NPM_NOTHING.to_string()
    } else {
        sets.join(" || ")
    }
}

/// `range` as a node-semver range (module docs).
pub fn npm_range(range: &VersRange) -> String {
    npm_join(npm_sets(range))
}

/// Every range in `ranges` at once, as one node-semver range.
pub fn npm_intersection(ranges: &[VersRange]) -> String {
    let combined = product(ranges.iter().map(npm_sets).collect(), " ");
    npm_join(
        combined
            .into_iter()
            .map(|set| {
                let kept: Vec<&str> = set.split(' ').filter(|c| *c != "*").collect::<Vec<_>>();
                if kept.is_empty() {
                    "*".to_string()
                } else {
                    kept.join(" ")
                }
            })
            .collect(),
    )
}

/// The PEP 440 specifier sets for `range`, any of which admits a version.
pub fn pep440_sets(range: &VersRange) -> Vec<String> {
    let Pieces { pieces, excluded } = pieces(range);
    let exclusions = excluded.iter().map(|x| format!("!={x}"));
    pieces
        .iter()
        .map(|piece| {
            let mut parts: Vec<String> = match piece {
                Piece::All => Vec::new(),
                Piece::Point(v) => return format!("=={v}"),
                Piece::Interval { lower, upper } => {
                    let mut parts = Vec::new();
                    if let Some((incl, v)) = lower {
                        parts.push(format!("{}{v}", if *incl { ">=" } else { ">" }));
                    }
                    if let Some((incl, v)) = upper {
                        parts.push(format!("{}{v}", if *incl { "<=" } else { "<" }));
                    }
                    parts
                }
            };
            parts.extend(exclusions.clone());
            parts.join(",")
        })
        .collect()
}

/// Every range in `ranges` at once, as PEP 440 specifier sets any of which
/// admits a version.
pub fn pep440_intersection(ranges: &[VersRange]) -> Vec<String> {
    product(ranges.iter().map(pep440_sets).collect(), ",")
        .into_iter()
        .map(|set| {
            set.split(',')
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>()
                .join(",")
        })
        .collect()
}

/// Each combination of one entry per list, joined with `sep`.
fn product(lists: Vec<Vec<String>>, sep: &str) -> Vec<String> {
    lists.into_iter().fold(vec![String::new()], |acc, list| {
        acc.iter()
            .flat_map(|head| {
                list.iter().map(move |item| {
                    if head.is_empty() {
                        item.clone()
                    } else {
                        format!("{head}{sep}{item}")
                    }
                })
            })
            .collect()
    })
}

/// Whether node-semver puts `version` in `range`; `None` when either does
/// not parse as node-semver.
pub(crate) fn npm_contains(range: &VersRange, version: &Version) -> Option<bool> {
    let native = nodejs_semver::Range::parse(npm_range(range)).ok()?;
    let v = nodejs_semver::Version::parse(version.as_str()).ok()?;
    Some(native.satisfies(&v))
}

/// Whether PEP 440 puts `version` in `range`; `None` when either does not
/// parse as PEP 440.
pub(crate) fn pep440_contains(range: &VersRange, version: &Version) -> Option<bool> {
    let v = pep440_rs::Version::from_str(version.as_str()).ok()?;
    let mut any = false;
    for set in pep440_sets(range) {
        let specifiers = pep440_rs::VersionSpecifiers::from_str(&set).ok()?;
        any |= specifiers.contains(&v);
    }
    Some(any)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::version::Scheme;

    fn r(s: &str) -> VersRange {
        VersRange::parse(s).unwrap()
    }

    #[test]
    fn npm_ranges_are_comparator_sets_joined_by_or() {
        assert_eq!(npm_range(&r("vers:npm/*")), "*");
        assert_eq!(npm_range(&r("vers:npm/1.2.3")), "1.2.3");
        assert_eq!(npm_range(&r("vers:npm/>=1.2.0|<2.0.0")), ">=1.2.0 <2.0.0");
        assert_eq!(npm_range(&r("vers:npm/>1.0.0|<=1.5.0")), ">1.0.0 <=1.5.0");
        assert_eq!(npm_range(&r("vers:npm/>=1.0.0")), ">=1.0.0");
        assert_eq!(npm_range(&r("vers:npm/<2.0.0")), "<2.0.0");
        assert_eq!(
            npm_range(&r("vers:npm/>=1.0.0|<1.1.0|>=1.5.0|<1.6.0")),
            ">=1.0.0 <1.1.0 || >=1.5.0 <1.6.0"
        );
        assert_eq!(
            npm_range(&r("vers:npm/1.0.0|>=2.0.0|<3.0.0|4.0.0")),
            "1.0.0 || >=2.0.0 <3.0.0 || 4.0.0"
        );
    }

    #[test]
    fn an_npm_exclusion_splits_the_interval_holding_it() {
        assert_eq!(
            npm_range(&r("vers:npm/>=1.0.0|!=1.5.0|<2.0.0")),
            ">=1.0.0 <1.5.0 || >1.5.0 <2.0.0"
        );
        assert_eq!(
            npm_range(&r("vers:npm/>=1.0.0|<2.0.0|!=3.0.0")),
            ">=1.0.0 <2.0.0"
        );
        assert_eq!(
            npm_range(&r("vers:npm/>=1.0.0|!=1.2.0|!=1.4.0")),
            ">=1.0.0 <1.2.0 || >1.2.0 <1.4.0 || >1.4.0"
        );
    }

    #[test]
    fn an_npm_range_of_only_exclusions_admits_nothing_as_vers_does() {
        assert_eq!(npm_range(&r("vers:npm/!=1.0.0")), "<0.0.0-0");
        let v = Version::parse("2.0.0", Scheme::Semver).unwrap();
        assert!(!r("vers:npm/!=1.0.0").contains(&v));
    }

    #[test]
    fn several_npm_ranges_intersect() {
        assert_eq!(
            npm_intersection(&[r("vers:npm/>=1.0.0|<2.0.0"), r("vers:npm/>=1.5.0")]),
            ">=1.0.0 <2.0.0 >=1.5.0"
        );
        assert_eq!(
            npm_intersection(&[r("vers:npm/*"), r("vers:npm/>=1.5.0")]),
            ">=1.5.0"
        );
        assert_eq!(npm_intersection(&[r("vers:npm/*"), r("vers:npm/*")]), "*");
        assert_eq!(
            npm_intersection(&[r("vers:npm/1.0.0|2.0.0"), r("vers:npm/<3.0.0|4.0.0")]),
            "1.0.0 <3.0.0 || 1.0.0 4.0.0 || 2.0.0 <3.0.0 || 2.0.0 4.0.0"
        );
        assert_eq!(
            npm_intersection(&[r("vers:npm/!=1.0.0"), r("vers:npm/*")]),
            "<0.0.0-0"
        );
    }

    #[test]
    fn pep440_ranges_are_specifier_sets() {
        assert_eq!(pep440_sets(&r("vers:pypi/*")), vec![""]);
        assert_eq!(pep440_sets(&r("vers:pypi/1.2")), vec!["==1.2"]);
        assert_eq!(pep440_sets(&r("vers:pypi/>=1.2|<2")), vec![">=1.2,<2"]);
        assert_eq!(pep440_sets(&r("vers:pypi/>1.0|<=1.5")), vec![">1.0,<=1.5"]);
        assert_eq!(
            pep440_sets(&r("vers:pypi/>=1.0|!=1.5|<2.0")),
            vec![">=1.0,<2.0,!=1.5"]
        );
        assert_eq!(
            pep440_sets(&r("vers:pypi/>=1.0|<1.1|>=1.5|<1.6")),
            vec![">=1.0,<1.1", ">=1.5,<1.6"]
        );
        assert_eq!(
            pep440_sets(&r("vers:pypi/1.0|>=2.0")),
            vec!["==1.0", ">=2.0"]
        );
        assert_eq!(pep440_sets(&r("vers:pypi/!=1.0")), Vec::<String>::new());
    }

    #[test]
    fn several_pep440_ranges_intersect() {
        assert_eq!(
            pep440_intersection(&[r("vers:pypi/>=1.0|<2.0"), r("vers:pypi/>=1.5")]),
            vec![">=1.0,<2.0,>=1.5"]
        );
        assert_eq!(
            pep440_intersection(&[r("vers:pypi/*"), r("vers:pypi/>=1.5")]),
            vec![">=1.5"]
        );
        assert_eq!(
            pep440_intersection(&[r("vers:pypi/*"), r("vers:pypi/*")]),
            vec![""]
        );
        assert_eq!(
            pep440_intersection(&[r("vers:pypi/1.0|>=3"), r("vers:pypi/<4")]),
            vec!["==1.0,<4", ">=3,<4"]
        );
    }

    fn npm(v: &str) -> Version {
        Version::parse(v, Scheme::Semver).unwrap()
    }

    fn pep(v: &str) -> Version {
        Version::parse(v, Scheme::Pep440).unwrap()
    }

    #[test]
    fn npm_containment_is_node_semvers() {
        let range = r("vers:npm/>=1.0.0|<2.0.0");
        assert!(range.contains(&npm("1.5.0")));
        assert!(!range.contains(&npm("2.0.0")));
        // node-semver: a pre-release only within its own major.minor.patch
        // bound; the vers interval alone would admit both.
        assert!(!range.contains(&npm("1.5.0-rc.1")));
        assert!(!range.contains(&npm("2.0.0-rc.1")));
        let pre = r("vers:npm/>=1.5.0-rc.1|<2.0.0");
        assert!(pre.contains(&npm("1.5.0-rc.2")));
        assert!(!pre.contains(&npm("1.6.0-rc.1")));
        assert!(r("vers:npm/1.2.3").contains(&npm("1.2.3")));
        assert!(!r("vers:npm/>=1.0.0|!=1.5.0|<2.0.0").contains(&npm("1.5.0")));
        assert!(r("vers:npm/>=1.0.0|!=1.5.0|<2.0.0").contains(&npm("1.5.1")));
        assert!(r("vers:npm/*").contains(&npm("3.0.0")));
        assert!(!r("vers:npm/*").contains(&npm("3.0.0-beta.1")));
    }

    #[test]
    fn pep440_containment_is_pep440s() {
        let range = r("vers:pypi/>=2.0b1|<3.0");
        assert!(range.contains(&pep("2.5")));
        assert!(range.contains(&pep("2.0b2")));
        // `<3.0` admits no pre-release of 3.0, which the interval would.
        assert!(!range.contains(&pep("3.0rc1")));
        // `>1.0` admits no post-release of 1.0.
        assert!(!r("vers:pypi/>1.0").contains(&pep("1.0.post1")));
        assert!(r("vers:pypi/>1.0").contains(&pep("1.1")));
        // `==1.0` ignores a local label.
        assert!(r("vers:pypi/1.0").contains(&pep("1.0+cpu")));
        assert!(!r("vers:pypi/>=1.0|!=1.5|<2.0").contains(&pep("1.5")));
        assert!(r("vers:pypi/1.0|>=3").contains(&pep("3.1")));
        assert!(!r("vers:pypi/1.0|>=3").contains(&pep("2.0")));
        assert!(r("vers:pypi/*").contains(&pep("9.0")));
    }

    #[test]
    fn a_version_the_native_grammar_rejects_is_left_to_the_vers_walk() {
        let bad = Version::parse("x.y", Scheme::Generic).unwrap();
        assert_eq!(
            npm_contains(&r("vers:npm/>=1.0.0"), &npm("1.2.0")),
            Some(true)
        );
        assert_eq!(npm_contains(&r("vers:npm/>=1.0.0"), &bad), None);
        assert_eq!(
            pep440_contains(&r("vers:pypi/>=1.0"), &pep("1.2")),
            Some(true)
        );
        assert_eq!(pep440_contains(&r("vers:pypi/>=1.0"), &bad), None);
    }
}
