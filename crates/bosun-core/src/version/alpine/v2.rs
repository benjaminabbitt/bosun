//! Alpine version ordering as apk-tools 2 orders it (Alpine 3.22 and
//! earlier): `digit{.digit}...{letter}{digit}{_suffix{#}}...{-r#}`.
//!
//! apk-tools 2 reads a version token by token, each token's value an
//! integer: a number, a letter's character code, a suffix's rank (negative
//! for a pre-release). Two versions compare value by value while their
//! token kinds agree; where they part, the side that goes on is greater
//! unless what it goes on with is a pre-release suffix. It differs from
//! apk-tools 3 in three ways:
//!
//! - a `.`-number's leading zeros count against it: each zero after the
//!   first sorts it lower (`1.0 > 1.00`, `1.01 < 1.1`);
//! - a letter may be followed by a number (`1.0b2`);
//! - `~` is not part of the grammar: a version stops being read there, and
//!   whatever follows it is not ordered (`1.0~ab = 1.0~b`).
//!
//! A number of 18 or more digits ends the version as invalid.

use std::cmp::Ordering;

/// Token kinds, in the order the grammar allows them; [`INVALID`] is below
/// all of them, as apk-tools 2 numbers it.
type Kind = i8;
const INVALID: Kind = -1;
const DIGIT_OR_ZERO: Kind = 0;
const DIGIT: Kind = 1;
const LETTER: Kind = 2;
const SUFFIX: Kind = 3;
const SUFFIX_NO: Kind = 4;
const REVISION_NO: Kind = 5;
const END: Kind = 6;

/// The pre-release suffixes, in order: each sorts below no suffix at all.
const PRE_RELEASE: [&str; 4] = ["alpha", "beta", "pre", "rc"];

/// The other suffixes, in order: each sorts above no suffix at all.
const POST_RELEASE: [&str; 5] = ["cvs", "svn", "git", "hg", "p"];

/// A number this many digits long or longer is invalid.
const MAX_DIGITS: usize = 18;

/// A version being read: what is left of it, and the kind of the token at
/// its head.
struct Reader<'a> {
    rest: &'a [u8],
    kind: Kind,
}

impl<'a> Reader<'a> {
    fn new(raw: &'a str) -> Reader<'a> {
        Reader {
            rest: raw.as_bytes(),
            kind: DIGIT,
        }
    }

    /// Work out the kind of the token at the head of `rest`, consuming a
    /// separator (`.`, `_`, `-r`) but not the token itself.
    fn next_kind(&mut self) {
        let head = self.rest.first().copied().unwrap_or(0);
        let mut next = INVALID;
        if head == 0 {
            next = END;
        } else if (self.kind == DIGIT || self.kind == DIGIT_OR_ZERO) && head.is_ascii_lowercase() {
            next = LETTER;
        } else if self.kind == LETTER && head.is_ascii_digit() {
            next = DIGIT;
        } else {
            match head {
                b'.' => next = DIGIT_OR_ZERO,
                b'_' => next = SUFFIX,
                b'-' if self.rest.get(1) == Some(&b'r') => {
                    next = REVISION_NO;
                    self.rest = &self.rest[1..];
                }
                _ => {}
            }
            self.rest = &self.rest[1..];
        }
        let allowed_back = (next == DIGIT_OR_ZERO && self.kind == DIGIT)
            || (next == SUFFIX && self.kind == SUFFIX_NO)
            || (next == DIGIT && self.kind == LETTER);
        if next < self.kind && !allowed_back {
            next = INVALID;
        }
        self.kind = next;
    }

    /// The value of the token at the head, consumed, leaving `kind` the
    /// kind of the token after it.
    fn token(&mut self) -> i64 {
        if self.rest.is_empty() {
            self.kind = END;
            return 0;
        }
        let mut used = 0;
        let mut then = INVALID;
        let value: i64 = match self.kind {
            DIGIT_OR_ZERO if self.rest[0] == b'0' => {
                // Each zero after the first lowers the value; the last zero
                // is read again as a number.
                while self.rest.get(used + 1) == Some(&b'0') {
                    used += 1;
                }
                then = DIGIT;
                -(used as i64)
            }
            DIGIT_OR_ZERO | DIGIT | SUFFIX_NO | REVISION_NO => {
                let mut v: i64 = 0;
                while let Some(d) = self.rest.get(used).filter(|b| b.is_ascii_digit()) {
                    v = v.wrapping_mul(10).wrapping_add(i64::from(d - b'0'));
                    used += 1;
                }
                if used >= MAX_DIGITS {
                    self.kind = INVALID;
                    return -1;
                }
                v
            }
            LETTER => {
                used = 1;
                i64::from(self.rest[0])
            }
            SUFFIX => match suffix(self.rest) {
                Some((len, rank)) => {
                    used = len;
                    then = SUFFIX_NO;
                    rank
                }
                None => {
                    self.kind = INVALID;
                    return -1;
                }
            },
            _ => {
                self.kind = INVALID;
                return -1;
            }
        };
        self.rest = &self.rest[used..];
        if self.rest.is_empty() {
            self.kind = END;
        } else if then != INVALID {
            self.kind = then;
        } else {
            self.next_kind();
        }
        value
    }
}

/// The suffix `rest` starts with, by prefix as apk-tools 2 matches it: its
/// length and rank (negative for a pre-release).
fn suffix(rest: &[u8]) -> Option<(usize, i64)> {
    if let Some(i) = PRE_RELEASE
        .iter()
        .position(|s| rest.starts_with(s.as_bytes()))
    {
        return Some((PRE_RELEASE[i].len(), i as i64 - PRE_RELEASE.len() as i64));
    }
    POST_RELEASE
        .iter()
        .position(|s| rest.starts_with(s.as_bytes()))
        .map(|i| (POST_RELEASE[i].len(), i as i64))
}

/// Whether apk-tools 2 accepts `raw` as a version.
pub(super) fn is_valid(raw: &str) -> bool {
    let mut r = Reader::new(raw);
    while r.kind != END && r.kind != INVALID {
        r.token();
    }
    r.kind == END
}

/// Order two versions as apk-tools 2 does; any string compares.
pub(super) fn compare(a: &str, b: &str) -> Ordering {
    let (mut ra, mut rb) = (Reader::new(a), Reader::new(b));
    let (mut va, mut vb) = (0, 0);
    while ra.kind == rb.kind && ra.kind != END && ra.kind != INVALID && va == vb {
        va = ra.token();
        vb = rb.token();
    }
    if va != vb {
        return va.cmp(&vb);
    }
    if ra.kind == rb.kind {
        return Ordering::Equal;
    }
    // Equal so far: the side that goes on is greater, unless what it goes on
    // with is a pre-release suffix.
    let (ka, kb) = (ra.kind, rb.kind);
    if ka == SUFFIX && ra.token() < 0 {
        return Ordering::Less;
    }
    if kb == SUFFIX && rb.token() < 0 {
        return Ordering::Greater;
    }
    kb.cmp(&ka)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn orders_as_apk_tools_2_does() {
        // Checked against apk-tools 2.14.4 (`apk version -t`, Alpine 3.20).
        for (a, b, want) in [
            ("1.0~ab", "1.0", Ordering::Greater),
            ("1.0~ab", "1.0-r1", Ordering::Greater),
            ("1.0~ab", "1.0~b", Ordering::Equal),
            ("1.0~a-r1", "1.0~a", Ordering::Equal),
            ("1.0_rc1~ab", "1.0_rc1", Ordering::Greater),
            ("1.0~ab-r1", "1.0-r2", Ordering::Greater),
            ("1.0~ab-r2", "1.0~cd-r1", Ordering::Equal),
            ("1.0~ab", "1.0_p1", Ordering::Greater),
            ("1.01", "1.1", Ordering::Less),
            ("1.0", "1.00", Ordering::Greater),
            ("1.0bc", "1.0", Ordering::Greater),
            ("1.0-1", "1.0", Ordering::Greater),
            ("1.10", "1.9", Ordering::Greater),
            ("01.2", "1.2", Ordering::Equal),
            ("1.010", "1.01", Ordering::Greater),
            ("1.0-r1", "1.0", Ordering::Greater),
            ("1.0_rc1-r5", "1.0", Ordering::Less),
            ("1.0b2", "1.0b10", Ordering::Less),
            ("1.0_cvs", "1.0", Ordering::Greater),
            ("1.0_foo", "1.0_cvs", Ordering::Less),
            ("1.123456789012345678", "1.0", Ordering::Less),
            ("1.0-r1-r2", "1.0-r1", Ordering::Greater),
        ] {
            assert_eq!(compare(a, b), want, "{a} vs {b}");
            assert_eq!(compare(b, a), want.reverse(), "{b} vs {a}");
        }
    }

    #[test]
    fn suffixes_rank_pre_releases_below_the_release_and_patches_above() {
        let ordered = [
            "1.0_alpha",
            "1.0_alpha1",
            "1.0_beta",
            "1.0_pre",
            "1.0_rc",
            "1.0_rc1",
            "1.0",
            "1.0_cvs",
            "1.0_svn",
            "1.0_git",
            "1.0_hg",
            "1.0_p",
            "1.0_p1",
            "1.0a",
            "1.0.1",
        ];
        for pair in ordered.windows(2) {
            assert_eq!(compare(pair[0], pair[1]), Ordering::Less, "{pair:?}");
        }
    }

    #[test]
    fn accepts_what_apk_tools_2_accepts() {
        // Each checked with `apk version -c` on apk-tools 2.14.4.
        for v in [
            "1",
            "1.0",
            "1.0b2",
            "1.0_rc1_p2",
            "1.0_p1_rc1",
            "2.4_p1-r1",
            "17.0-r0",
            "1.0-r1-r2",
        ] {
            assert!(is_valid(v), "{v} should be valid");
        }
        for v in [
            "1.0~ab",
            "1.0~ab-r1",
            "1.0bc",
            "1.0-1",
            "1.0~ABC",
            "1.0~",
            "1.0a.1",
            "1.0_foo",
            "1.0_p1.2",
            "1.0-r1_p1",
        ] {
            assert!(!is_valid(v), "{v:?} should be invalid");
        }
    }

    #[test]
    fn a_number_of_eighteen_digits_is_invalid() {
        assert!(is_valid("12345678901234567"));
        assert!(!is_valid("123456789012345678"));
        assert_eq!(
            compare("1.12345678901234567", "1.12345678901234566"),
            Ordering::Greater
        );
    }
}
