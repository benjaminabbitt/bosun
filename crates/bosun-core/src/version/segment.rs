//! The generic version-ordering engine: decompose a version string into
//! typed `Segment` chunks and compare the sequences (§6).
//!
//! This is bosun's own comparator, used for the `Generic` scheme — the long
//! tail of ecosystems with no dedicated reference crate. Debian and SemVer
//! versions delegate to `deb-version` and `semver` respectively (see
//! `super`), so this engine is deliberately a clean, documented ordering
//! rather than a byte-for-byte clone of any one tool. It still encodes the
//! three non-obvious rules §6 requires:
//!
//! 1. numeric chunks compare numerically (`1.10 > 1.9`),
//! 2. a tilde chunk lowers precedence below even the end of the string
//!    (`1.0~rc1 < 1.0`), and
//! 3. when one sequence runs out, a remaining tilde makes that side smaller
//!    and any other remaining chunk makes it larger (so `1.0 < 1.0.1`).
//!
//! The tie-break between differently typed chunks at the same position is
//! fixed and total: `Tilde < Alpha < Numeric`.

use std::cmp::Ordering;

/// One typed chunk of a version string.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Segment {
    /// A `~`: sorts before anything, even the end of the string.
    Tilde,
    /// A maximal run of characters that are neither ASCII digits nor `~`.
    /// Compared lexically, byte by byte.
    Alpha(String),
    /// A maximal run of ASCII digits, compared numerically. A run too long
    /// for `u64` saturates at `u64::MAX` (documented; real versions fit).
    Numeric(u64),
}

impl Segment {
    /// The rank used to order chunks of different types at the same
    /// position: `Tilde < Alpha < Numeric`.
    fn rank(&self) -> u8 {
        match self {
            Segment::Tilde => 0,
            Segment::Alpha(_) => 1,
            Segment::Numeric(_) => 2,
        }
    }
}

impl Ord for Segment {
    fn cmp(&self, other: &Self) -> Ordering {
        match (self, other) {
            (Segment::Numeric(a), Segment::Numeric(b)) => a.cmp(b),
            (Segment::Alpha(a), Segment::Alpha(b)) => a.cmp(b),
            // Two tildes carry no value to compare; the equal-rank fallback
            // below already yields `Equal`, so no explicit arm is needed.
            _ => self.rank().cmp(&other.rank()),
        }
    }
}

impl PartialOrd for Segment {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// Decompose a version string into typed chunks: each `~` is its own
/// `Tilde`, each maximal digit run a `Numeric`, each maximal run of other
/// characters an `Alpha`.
pub fn tokenize(s: &str) -> Vec<Segment> {
    let mut out = Vec::new();
    let mut chars = s.chars().peekable();
    while let Some(&c) = chars.peek() {
        if c == '~' {
            chars.next();
            out.push(Segment::Tilde);
        } else if c.is_ascii_digit() {
            let mut n: u64 = 0;
            while let Some(&d) = chars.peek() {
                if let Some(digit) = d.to_digit(10) {
                    n = n.saturating_mul(10).saturating_add(u64::from(digit));
                    chars.next();
                } else {
                    break;
                }
            }
            out.push(Segment::Numeric(n));
        } else {
            let mut buf = String::new();
            while let Some(&a) = chars.peek() {
                if a == '~' || a.is_ascii_digit() {
                    break;
                }
                buf.push(a);
                chars.next();
            }
            out.push(Segment::Alpha(buf));
        }
    }
    out
}

/// Compare two tokenized version sequences (§6 rule 3 for the tails).
pub fn compare(a: &[Segment], b: &[Segment]) -> Ordering {
    let mut a = a.iter();
    let mut b = b.iter();
    loop {
        match (a.next(), b.next()) {
            (Some(x), Some(y)) => {
                let o = x.cmp(y);
                if o != Ordering::Equal {
                    return o;
                }
            }
            (Some(Segment::Tilde), None) => return Ordering::Less,
            (None, Some(Segment::Tilde)) => return Ordering::Greater,
            (Some(_), None) => return Ordering::Greater,
            (None, Some(_)) => return Ordering::Less,
            (None, None) => return Ordering::Equal,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cmp(a: &str, b: &str) -> Ordering {
        compare(&tokenize(a), &tokenize(b))
    }

    #[test]
    fn tokenize_splits_tilde_numeric_alpha() {
        assert_eq!(
            tokenize("1.0~rc1"),
            vec![
                Segment::Numeric(1),
                Segment::Alpha(".".into()),
                Segment::Numeric(0),
                Segment::Tilde,
                Segment::Alpha("rc".into()),
                Segment::Numeric(1),
            ]
        );
    }

    #[test]
    fn tokenize_empty_is_empty() {
        assert_eq!(tokenize(""), Vec::<Segment>::new());
    }

    #[test]
    fn numeric_compares_numerically_not_lexically() {
        // Rule 1: 1.10 > 1.9 even though "10" < "9" as text.
        assert_eq!(cmp("1.10", "1.9"), Ordering::Greater);
        assert_eq!(cmp("1.9", "1.10"), Ordering::Less);
    }

    #[test]
    fn numeric_run_saturates_without_panicking() {
        let huge = "9".repeat(40);
        // Does not panic; saturates. Equal to itself.
        assert_eq!(cmp(&huge, &huge), Ordering::Equal);
    }

    #[test]
    fn tilde_sorts_before_release() {
        // Rule 2: a tilde chunk lowers precedence below the end of string.
        assert_eq!(cmp("1.0~rc1", "1.0"), Ordering::Less);
        assert_eq!(cmp("1.0", "1.0~rc1"), Ordering::Greater);
    }

    #[test]
    fn tilde_sorts_before_anything() {
        assert_eq!(cmp("1.0~~", "1.0~"), Ordering::Less);
        assert_eq!(cmp("1.0~", "1.0"), Ordering::Less);
    }

    #[test]
    fn longer_wins_when_extra_is_not_tilde() {
        // Rule 3: a remaining non-tilde chunk makes that side larger.
        assert_eq!(cmp("1.0.1", "1.0"), Ordering::Greater);
        assert_eq!(cmp("1.0", "1.0.1"), Ordering::Less);
    }

    #[test]
    fn equal_sequences_are_equal() {
        assert_eq!(cmp("1.2.3", "1.2.3"), Ordering::Equal);
    }

    #[test]
    fn tilde_beats_empty_even_mid_compare() {
        assert_eq!(cmp("1.0~alpha", "1.0.0"), Ordering::Less);
    }

    #[test]
    fn segment_type_tiebreak_is_tilde_alpha_numeric() {
        assert!(Segment::Tilde < Segment::Alpha("a".into()));
        assert!(Segment::Alpha("z".into()) < Segment::Numeric(0));
        assert!(Segment::Numeric(0) > Segment::Tilde);
    }

    #[test]
    fn alpha_compares_lexically() {
        assert_eq!(cmp("1.0a", "1.0b"), Ordering::Less);
    }
}
