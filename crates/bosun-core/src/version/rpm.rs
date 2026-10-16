//! RPM version ordering (§6): `rpmvercmp` over each part of
//! `[epoch:]version[-release]`, as rpm and dnf order packages.
//!
//! `rpmvercmp` walks both strings a segment at a time: a maximal run of ASCII
//! digits or of ASCII letters, with every other character a separator that only
//! splits segments. Digits compare numerically (leading zeros dropped) and beat
//! letters; letters compare byte by byte. `~` sorts before anything, even the
//! end of the string (`1.0~rc1 < 1.0`); `^` sorts after the end of the string
//! but before any further segment (`1.0 < 1.0^git1 < 1.0.1`). When every
//! segment matched, the side with characters left over is newer.
//!
//! An [`Evr`] compares its epoch numerically (absent is 0), then the version,
//! then the release. rpm skips the release when either side has none, which
//! is not a total order (`1.0` would equal both `1.0-1` and `1.0-2`); here an
//! absent release sorts below any release instead, so sorting stays sound.

use std::cmp::Ordering;

/// A parsed `[epoch:]version[-release]`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Evr {
    epoch: u64,
    version: String,
    release: Option<String>,
}

impl Evr {
    /// Split `raw` into epoch, version and release. The epoch is the text
    /// before the first `:` when that is all digits (an empty one is 0); the
    /// release is the text after the last `-`.
    pub fn parse(raw: &str) -> Evr {
        let (epoch, rest) = match raw.split_once(':') {
            Some((e, rest)) if e.bytes().all(|b| b.is_ascii_digit()) => {
                (e.parse().unwrap_or(0), rest)
            }
            _ => (0, raw),
        };
        let (version, release) = match rest.rsplit_once('-') {
            Some((v, r)) => (v.to_string(), Some(r.to_string())),
            None => (rest.to_string(), None),
        };
        Evr {
            epoch,
            version,
            release,
        }
    }
}

impl Ord for Evr {
    fn cmp(&self, other: &Self) -> Ordering {
        self.epoch
            .cmp(&other.epoch)
            .then_with(|| rpmvercmp(&self.version, &other.version))
            .then_with(|| match (&self.release, &other.release) {
                (Some(a), Some(b)) => rpmvercmp(a, b),
                (a, b) => a.is_some().cmp(&b.is_some()),
            })
    }
}

impl PartialOrd for Evr {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// rpm's `rpmvercmp`: the ordering of two version (or release) strings.
pub fn rpmvercmp(a: &str, b: &str) -> Ordering {
    let (mut one, mut two) = (a.as_bytes(), b.as_bytes());
    loop {
        one = skip_separators(one);
        two = skip_separators(two);
        match (one.first(), two.first()) {
            (Some(b'~'), Some(b'~')) | (Some(b'^'), Some(b'^')) => {
                one = &one[1..];
                two = &two[1..];
                continue;
            }
            (Some(b'~'), _) => return Ordering::Less,
            (_, Some(b'~')) => return Ordering::Greater,
            (None, Some(b'^')) => return Ordering::Less,
            (Some(b'^'), None) => return Ordering::Greater,
            (Some(b'^'), _) => return Ordering::Less,
            (_, Some(b'^')) => return Ordering::Greater,
            (None, _) | (_, None) => break,
            (Some(_), Some(_)) => {}
        }
        let numeric = one[0].is_ascii_digit();
        let class = |c: &u8| {
            if numeric {
                c.is_ascii_digit()
            } else {
                c.is_ascii_alphabetic()
            }
        };
        let (seg1, rest1) = one.split_at(one.iter().take_while(|c| class(c)).count());
        let (seg2, rest2) = two.split_at(two.iter().take_while(|c| class(c)).count());
        if seg2.is_empty() {
            // Different kinds: a number is newer than letters.
            return if numeric {
                Ordering::Greater
            } else {
                Ordering::Less
            };
        }
        let order = if numeric {
            let (n1, n2) = (strip_zeros(seg1), strip_zeros(seg2));
            n1.len().cmp(&n2.len()).then_with(|| n1.cmp(n2))
        } else {
            seg1.cmp(seg2)
        };
        if order != Ordering::Equal {
            return order;
        }
        one = rest1;
        two = rest2;
    }
    // Every segment matched: whichever has characters left is newer.
    one.is_empty().cmp(&two.is_empty()).reverse()
}

/// Drop leading characters that are neither ASCII alphanumerics nor `~`/`^`.
fn skip_separators(s: &[u8]) -> &[u8] {
    let n = s
        .iter()
        .take_while(|c| !(c.is_ascii_alphanumeric() || **c == b'~' || **c == b'^'))
        .count();
    &s[n..]
}

fn strip_zeros(s: &[u8]) -> &[u8] {
    &s[s.iter().take_while(|c| **c == b'0').count()..]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The cases from rpm's own test suite (`tests/rpmvercmp.at`), as
    /// `(a, b, rpmvercmp(a, b))`.
    const RPM_CASES: &[(&str, &str, i8)] = &[
        ("1.0", "1.0", 0),
        ("1.0", "2.0", -1),
        ("2.0", "1.0", 1),
        ("2.0.1", "2.0.1", 0),
        ("2.0", "2.0.1", -1),
        ("2.0.1", "2.0", 1),
        ("2.0.1a", "2.0.1a", 0),
        ("2.0.1a", "2.0.1", 1),
        ("2.0.1", "2.0.1a", -1),
        ("5.5p1", "5.5p1", 0),
        ("5.5p1", "5.5p2", -1),
        ("5.5p2", "5.5p1", 1),
        ("5.5p10", "5.5p10", 0),
        ("5.5p1", "5.5p10", -1),
        ("5.5p10", "5.5p1", 1),
        ("10xyz", "10.1xyz", -1),
        ("10.1xyz", "10xyz", 1),
        ("xyz10", "xyz10", 0),
        ("xyz10", "xyz10.1", -1),
        ("xyz10.1", "xyz10", 1),
        ("xyz.4", "xyz.4", 0),
        ("xyz.4", "8", -1),
        ("8", "xyz.4", 1),
        ("xyz.4", "2", -1),
        ("2", "xyz.4", 1),
        ("5.5p2", "5.6p1", -1),
        ("5.6p1", "5.5p2", 1),
        ("5.6p1", "6.5p1", -1),
        ("6.5p1", "5.6p1", 1),
        ("6.0.rc1", "6.0", 1),
        ("6.0", "6.0.rc1", -1),
        ("10b2", "10a1", 1),
        ("10a2", "10b2", -1),
        ("1.0aa", "1.0aa", 0),
        ("1.0a", "1.0aa", -1),
        ("1.0aa", "1.0a", 1),
        ("10.0001", "10.0001", 0),
        ("10.0001", "10.1", 0),
        ("10.1", "10.0001", 0),
        ("10.0001", "10.0039", -1),
        ("10.0039", "10.0001", 1),
        ("4.999.9", "5.0", -1),
        ("5.0", "4.999.9", 1),
        ("20101121", "20101121", 0),
        ("20101121", "20101122", -1),
        ("20101122", "20101121", 1),
        ("2_0", "2_0", 0),
        ("2.0", "2_0", 0),
        ("2_0", "2.0", 0),
        ("a", "a", 0),
        ("a+", "a+", 0),
        ("a+", "a_", 0),
        ("a_", "a+", 0),
        ("+a", "+a", 0),
        ("+a", "_a", 0),
        ("_a", "+a", 0),
        ("+_", "+_", 0),
        ("_+", "+_", 0),
        ("_+", "_+", 0),
        ("+", "_", 0),
        ("_", "+", 0),
        ("1.0~rc1", "1.0~rc1", 0),
        ("1.0~rc1", "1.0", -1),
        ("1.0", "1.0~rc1", 1),
        ("1.0~rc1", "1.0~rc2", -1),
        ("1.0~rc2", "1.0~rc1", 1),
        ("1.0~rc1~git123", "1.0~rc1~git123", 0),
        ("1.0~rc1~git123", "1.0~rc1", -1),
        ("1.0~rc1", "1.0~rc1~git123", 1),
        ("1.0^", "1.0^", 0),
        ("1.0^", "1.0", 1),
        ("1.0", "1.0^", -1),
        ("1.0^git1", "1.0^git1", 0),
        ("1.0^git1", "1.0", 1),
        ("1.0", "1.0^git1", -1),
        ("1.0^git1", "1.0^git2", -1),
        ("1.0^git2", "1.0^git1", 1),
        ("1.0^git1", "1.01", -1),
        ("1.01", "1.0^git1", 1),
        ("1.0^20160101", "1.0^20160101", 0),
        ("1.0^20160101", "1.0.1", -1),
        ("1.0.1", "1.0^20160101", 1),
        ("1.0^20160101^git1", "1.0^20160101^git1", 0),
        ("1.0^20160102", "1.0^20160101^git1", 1),
        ("1.0^20160101^git1", "1.0^20160102", -1),
        ("1.0~rc1^git1", "1.0~rc1^git1", 0),
        ("1.0~rc1^git1", "1.0~rc1", 1),
        ("1.0~rc1", "1.0~rc1^git1", -1),
        ("1.0^git1~pre", "1.0^git1~pre", 0),
        ("1.0^git1", "1.0^git1~pre", 1),
        ("1.0^git1~pre", "1.0^git1", -1),
        ("1b.fc17", "1b.fc17", 0),
        ("1b.fc17", "1.fc17", -1),
        ("1.fc17", "1b.fc17", 1),
        ("1g.fc17", "1g.fc17", 0),
        ("1g.fc17", "1.fc17", 1),
        ("1.fc17", "1g.fc17", -1),
    ];

    fn ordering(n: i8) -> Ordering {
        n.cmp(&0)
    }

    #[test]
    fn rpmvercmp_matches_rpms_own_test_suite() {
        for &(a, b, want) in RPM_CASES {
            assert_eq!(rpmvercmp(a, b), ordering(want), "rpmvercmp({a:?}, {b:?})");
        }
    }

    #[test]
    fn rpmvercmp_end_of_string_against_a_segment() {
        // Ran out on one side with segments left on the other.
        assert_eq!(rpmvercmp("", "1"), Ordering::Less);
        assert_eq!(rpmvercmp("1", ""), Ordering::Greater);
        assert_eq!(rpmvercmp("", ""), Ordering::Equal);
        // Only separators left over still count as characters left over.
        assert_eq!(rpmvercmp("1.", "1"), Ordering::Equal);
        assert_eq!(rpmvercmp("1", "1."), Ordering::Equal);
    }

    #[test]
    fn rpmvercmp_caret_against_the_end() {
        assert_eq!(rpmvercmp("1^", "1~"), Ordering::Greater);
        assert_eq!(rpmvercmp("1~", "1^"), Ordering::Less);
        assert_eq!(rpmvercmp("1^a", "1a"), Ordering::Less);
        assert_eq!(rpmvercmp("1a", "1^a"), Ordering::Greater);
    }

    #[test]
    fn rpmvercmp_numbers_longer_than_u64() {
        let big = "1".repeat(30);
        let bigger = format!("{big}0");
        assert_eq!(rpmvercmp(&big, &bigger), Ordering::Less);
        assert_eq!(rpmvercmp(&bigger, &big), Ordering::Greater);
        assert_eq!(rpmvercmp(&big, &big), Ordering::Equal);
    }

    fn evr(s: &str) -> Evr {
        Evr::parse(s)
    }

    #[test]
    fn evr_splits_epoch_version_release() {
        assert_eq!(
            evr("2:4.15.1-12.fc41"),
            Evr {
                epoch: 2,
                version: "4.15.1".into(),
                release: Some("12.fc41".into())
            }
        );
        assert_eq!(
            evr("1.7.1"),
            Evr {
                epoch: 0,
                version: "1.7.1".into(),
                release: None
            }
        );
        // The release is after the last dash.
        assert_eq!(
            evr("1.0-beta-3"),
            Evr {
                epoch: 0,
                version: "1.0-beta".into(),
                release: Some("3".into())
            }
        );
        // A non-numeric "epoch" is part of the version; an empty one is 0.
        assert_eq!(
            evr("a:1-2"),
            Evr {
                epoch: 0,
                version: "a:1".into(),
                release: Some("2".into())
            }
        );
        assert_eq!(evr(":1-2").epoch, 0);
        assert_eq!(evr(":1-2").version, "1");
    }

    #[test]
    fn evr_epoch_dominates() {
        assert!(evr("1:1.6-1.fc41") > evr("2.0-1.fc41"));
        assert!(evr("2.0-1.fc41") < evr("1:1.6-1.fc41"));
        assert_eq!(evr("0:1.0-1"), evr("1.0-1"));
    }

    #[test]
    fn evr_version_before_release() {
        assert!(evr("1.7.1-10.fc41") > evr("1.7.1-8.fc41"));
        assert!(evr("1.7.2-1.fc41") > evr("1.7.1-10.fc41"));
        assert!(evr("2026e-1.el9_8") > evr("2026a-1.el9"));
        assert!(evr("2.34-266.el9_8") < evr("2.34-270.el9_8"));
    }

    #[test]
    fn evr_absent_release_sorts_below_any_release() {
        assert!(evr("1.7.1") < evr("1.7.1-1"));
        assert!(evr("1.7.1-1") > evr("1.7.1"));
        assert_eq!(evr("1.7.1").cmp(&evr("1.7.1")), Ordering::Equal);
        // ...but never above a higher version.
        assert!(evr("1.7.2~") > evr("1.7.1-10.fc41"));
        assert!(evr("1.7.2~") < evr("1.7.2-1.fc41"));
    }
}
