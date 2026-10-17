//! Alpine (apk) version ordering (§6), as the host's apk-tools orders
//! versions. apk-tools 2 (Alpine 3.22 and earlier, [`v2`]) and 3 order a few
//! versions differently, so the rule follows the major the apk backend
//! probes ([`set_apk_tools`]); apk-tools 3 until one is probed.
//!
//! apk-tools 3 reads `digit{.digit}...{letter}{_suffix{#}}...{~hash}{-r#}`.
//!
//! A version is a sequence of tokens whose kinds may only appear in grammar
//! order: an initial number, more `.`-numbers, one letter, `_` suffixes each
//! with an optional number, a `~` commit hash, and an `-r` revision. Two
//! versions compare token by token while their kinds agree; where they part,
//! the side that ends sorts above a pre-release suffix (`_alpha`, `_beta`,
//! `_pre`, `_rc`) and below anything else (`1.0_rc1 < 1.0 < 1.0_p1 <
//! 1.0-r1`).
//!
//! Numbers compare numerically, except a `.`-number with a leading zero on
//! either side, which compares as a string (`1.01 < 1.1`). Suffixes rank
//! `alpha < beta < pre < rc < (none) < cvs < svn < git < hg < p`.

use std::cmp::Ordering;
use std::sync::atomic::{AtomicU8, Ordering as AtomicOrdering};

mod v2;

/// Which apk-tools major's version rules bosun follows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ApkTools {
    /// apk-tools 2 (Alpine 3.22 and earlier).
    V2,
    /// apk-tools 3 (Alpine 3.23 and later).
    V3,
}

/// The rule in force, by major: a host fact, like its architecture, that
/// every Alpine version on this machine is read under.
static APK_TOOLS: AtomicU8 = AtomicU8::new(3);

/// Follow `rule` for every Alpine version from now on: the apk backend sets
/// the major it probed.
pub fn set_apk_tools(rule: ApkTools) {
    let major = match rule {
        ApkTools::V2 => 2,
        ApkTools::V3 => 3,
    };
    APK_TOOLS.store(major, AtomicOrdering::Relaxed);
}

/// The apk-tools rule in force.
pub fn apk_tools() -> ApkTools {
    match APK_TOOLS.load(AtomicOrdering::Relaxed) {
        2 => ApkTools::V2,
        _ => ApkTools::V3,
    }
}

/// Whether `raw` is a version `rule`'s apk-tools accepts.
pub(super) fn is_valid(rule: ApkTools, raw: &str) -> bool {
    match rule {
        ApkTools::V2 => v2::is_valid(raw),
        ApkTools::V3 => is_valid_v3(raw),
    }
}

/// Order two versions as `rule`'s apk-tools does.
pub(super) fn compare(rule: ApkTools, a: &str, b: &str) -> Ordering {
    match rule {
        ApkTools::V2 => v2::compare(a, b),
        ApkTools::V3 => compare_v3(a, b),
    }
}

/// Token kinds, in the order the grammar allows them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Kind {
    InitialDigit,
    Digit,
    Letter,
    Suffix,
    SuffixNo,
    CommitHash,
    RevisionNo,
    End,
    Invalid,
}

/// The pre-release suffixes, in order: each sorts below no suffix at all.
const PRE_RELEASE: [&str; 4] = ["alpha", "beta", "pre", "rc"];

/// The other suffixes, in order: each sorts above no suffix at all.
const POST_RELEASE: [&str; 5] = ["cvs", "svn", "git", "hg", "p"];

/// A suffix's rank: negative for a pre-release, from 0 for the rest.
fn suffix_rank(name: &str) -> Option<i8> {
    let at = |list: &[&str]| list.iter().position(|s| *s == name);
    match at(&PRE_RELEASE) {
        Some(i) => Some(i as i8 - PRE_RELEASE.len() as i8),
        None => at(&POST_RELEASE).map(|i| i as i8),
    }
}

/// One token: its kind, its text (digits without the `.`, the letter, the
/// suffix name, the hash) and, for a suffix, its [`suffix_rank`].
#[derive(Clone, Copy, Debug)]
struct Token<'a> {
    kind: Kind,
    text: &'a str,
    suffix: i8,
}

impl Token<'_> {
    fn is_pre_release(&self) -> bool {
        self.kind == Kind::Suffix && self.suffix < 0
    }
}

/// Reads tokens off a version string, each kind checked against the one
/// before it.
struct Tokens<'a> {
    rest: &'a str,
    last: Kind,
}

impl<'a> Tokens<'a> {
    fn new(raw: &'a str) -> Tokens<'a> {
        Tokens {
            rest: raw,
            last: Kind::InitialDigit,
        }
    }

    /// The leading run of `rest` whose bytes satisfy `pred`, consumed.
    fn take(&mut self, pred: impl Fn(u8) -> bool) -> &'a str {
        let len = self.rest.bytes().take_while(|b| pred(*b)).count();
        let (run, rest) = self.rest.split_at(len);
        self.rest = rest;
        run
    }

    fn token(&mut self, kind: Kind, text: &'a str) -> Token<'a> {
        let kind = if text.is_empty() { Kind::Invalid } else { kind };
        self.last = kind;
        Token {
            kind,
            text,
            suffix: 0,
        }
    }

    fn invalid(&mut self) -> Token<'a> {
        self.token(Kind::Invalid, "")
    }

    /// The first token: the initial number.
    fn first(&mut self) -> Token<'a> {
        let digits = self.take(|b| b.is_ascii_digit());
        self.token(Kind::InitialDigit, digits)
    }

    /// The token after the last one, [`Kind::End`] when none is left.
    fn next_token(&mut self) -> Token<'a> {
        let Some(&head) = self.rest.as_bytes().first() else {
            return Token {
                kind: Kind::End,
                text: "",
                suffix: 0,
            };
        };
        let last = self.last;
        match head {
            b'a'..=b'z' if last <= Kind::Digit => {
                let letter = &self.rest[..1];
                self.rest = &self.rest[1..];
                self.token(Kind::Letter, letter)
            }
            b'.' if last <= Kind::Digit => {
                self.rest = &self.rest[1..];
                let digits = self.take(|b| b.is_ascii_digit());
                self.token(Kind::Digit, digits)
            }
            b'0'..=b'9' if last == Kind::Suffix => {
                let digits = self.take(|b| b.is_ascii_digit());
                self.token(Kind::SuffixNo, digits)
            }
            b'_' if last <= Kind::SuffixNo => {
                self.rest = &self.rest[1..];
                let name = self.take(|b| b.is_ascii_lowercase());
                match suffix_rank(name) {
                    Some(rank) => Token {
                        suffix: rank,
                        ..self.token(Kind::Suffix, name)
                    },
                    None => self.invalid(),
                }
            }
            b'~' if last < Kind::CommitHash => {
                self.rest = &self.rest[1..];
                let hash = self.take(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
                self.token(Kind::CommitHash, hash)
            }
            b'-' if last < Kind::RevisionNo && self.rest.starts_with("-r") => {
                self.rest = &self.rest[2..];
                let digits = self.take(|b| b.is_ascii_digit());
                self.token(Kind::RevisionNo, digits)
            }
            _ => self.invalid(),
        }
    }
}

/// Whether `raw` is a version apk-tools 3 accepts.
fn is_valid_v3(raw: &str) -> bool {
    let mut tokens = Tokens::new(raw);
    let mut t = tokens.first();
    while t.kind < Kind::End {
        t = tokens.next_token();
    }
    t.kind == Kind::End
}

/// Two digit runs by value, however long.
fn numeric(a: &str, b: &str) -> Ordering {
    let (a, b) = (a.trim_start_matches('0'), b.trim_start_matches('0'));
    a.len().cmp(&b.len()).then_with(|| a.cmp(b))
}

/// Two tokens of the same kind.
fn token_cmp(a: &Token, b: &Token) -> Ordering {
    match a.kind {
        Kind::Digit if a.text.starts_with('0') || b.text.starts_with('0') => a.text.cmp(b.text),
        Kind::InitialDigit | Kind::Digit | Kind::SuffixNo | Kind::RevisionNo => {
            numeric(a.text, b.text)
        }
        Kind::Suffix => a.suffix.cmp(&b.suffix),
        _ => a.text.cmp(b.text),
    }
}

/// Order two valid versions as apk-tools 3 does.
fn compare_v3(a: &str, b: &str) -> Ordering {
    let (mut ia, mut ib) = (Tokens::new(a), Tokens::new(b));
    let (mut ta, mut tb) = (ia.first(), ib.first());
    while ta.kind == tb.kind && ta.kind < Kind::End {
        let r = token_cmp(&ta, &tb);
        if r != Ordering::Equal {
            return r;
        }
        ta = ia.next_token();
        tb = ib.next_token();
    }
    if ta.kind == tb.kind {
        return Ordering::Equal;
    }
    // Equal so far: the side that goes on is greater, unless what it goes on
    // with is a pre-release suffix.
    if ta.is_pre_release() {
        return Ordering::Less;
    }
    if tb.is_pre_release() {
        return Ordering::Greater;
    }
    tb.kind.cmp(&ta.kind)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cmp(a: &str, b: &str) -> Ordering {
        compare(ApkTools::V3, a, b)
    }

    fn is_valid(raw: &str) -> bool {
        super::is_valid(ApkTools::V3, raw)
    }

    #[test]
    fn the_rule_picks_the_majors_ordering_and_grammar() {
        assert_eq!(compare(ApkTools::V2, "1.0", "1.00"), Ordering::Greater);
        assert_eq!(compare(ApkTools::V3, "1.0", "1.00"), Ordering::Less);
        assert_eq!(compare(ApkTools::V2, "1.0~ab", "1.0~b"), Ordering::Equal);
        assert_eq!(compare(ApkTools::V3, "1.0~ab", "1.0~b"), Ordering::Less);
        assert!(super::is_valid(ApkTools::V2, "1.0b2"));
        assert!(!super::is_valid(ApkTools::V3, "1.0b2"));
        assert!(!super::is_valid(ApkTools::V2, "1.0~ab"));
        assert!(super::is_valid(ApkTools::V3, "1.0~ab"));
    }

    #[test]
    fn numbers_compare_numerically() {
        assert_eq!(cmp("1.10", "1.9"), Ordering::Greater);
        assert_eq!(cmp("2.34", "0.1.0_alpha"), Ordering::Greater);
        assert_eq!(cmp("1.0", "1.0"), Ordering::Equal);
        assert_eq!(cmp("1.0.1", "1.0"), Ordering::Greater);
        assert_eq!(cmp("20260909", "20260413"), Ordering::Greater);
    }

    #[test]
    fn a_leading_zero_compares_as_a_string() {
        // Checked against apk-tools 3.0.8 (`apk version -t`).
        assert_eq!(cmp("1.01", "1.1"), Ordering::Less);
        assert_eq!(cmp("1.010", "1.01"), Ordering::Greater);
        assert_eq!(cmp("1.0", "1.00"), Ordering::Less);
        // The initial number is always numeric.
        assert_eq!(cmp("01.2", "1.2"), Ordering::Equal);
        assert_eq!(cmp("00.1", "0.1"), Ordering::Equal);
    }

    #[test]
    fn numbers_longer_than_a_machine_word_still_compare() {
        assert_eq!(
            cmp("123456789012345678901234567890", "99999999999999999999999"),
            Ordering::Greater
        );
        assert_eq!(cmp("1-r10", "1-r9"), Ordering::Greater);
    }

    #[test]
    fn suffixes_rank_pre_releases_below_the_release_and_patches_above() {
        let ordered = [
            "1.0_alpha",
            "1.0_alpha1",
            "1.0_alpha2",
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
            assert_eq!(cmp(pair[0], pair[1]), Ordering::Less, "{pair:?}");
            assert_eq!(cmp(pair[1], pair[0]), Ordering::Greater, "{pair:?}");
        }
    }

    #[test]
    fn letters_compare_by_character() {
        assert_eq!(cmp("1.0a", "1.0b"), Ordering::Less);
        assert_eq!(cmp("1.0a", "1.0"), Ordering::Greater);
        assert_eq!(cmp("1.0a", "1.0.1"), Ordering::Less);
    }

    #[test]
    fn the_revision_comes_last() {
        assert_eq!(cmp("1.0.4-r3", "1.0.4-r4"), Ordering::Less);
        assert_eq!(cmp("1.0-r0", "1.0"), Ordering::Greater);
        assert_eq!(cmp("1.0_rc1-r5", "1.0"), Ordering::Less);
        assert_eq!(cmp("1.0_p1", "1.0.1"), Ordering::Less);
        assert_eq!(cmp("1.0_p1-r0", "1.0_p1"), Ordering::Greater);
    }

    #[test]
    fn a_commit_hash_compares_as_a_string_before_the_revision() {
        // Checked against apk-tools 3.0.8 (`apk version -t`).
        assert_eq!(cmp("1.0~ab", "1.0"), Ordering::Greater);
        assert_eq!(cmp("1.0~ab", "1.0-r1"), Ordering::Greater);
        assert_eq!(cmp("1.0~ab", "1.0~b"), Ordering::Less);
        assert_eq!(cmp("1.0~b", "1.0~ab"), Ordering::Greater);
        assert_eq!(cmp("1.0~a-r1", "1.0~a"), Ordering::Greater);
        assert_eq!(cmp("1.0_rc1~ab", "1.0_rc1"), Ordering::Greater);
    }

    #[test]
    fn valid_versions_follow_the_grammar() {
        for v in [
            "1",
            "1.0",
            "01.2",
            "1.02",
            "1.0a",
            "1.0_p",
            "1.0_pre",
            "1.0_rc",
            "1.0_rc1_p2",
            "1.0~abc",
            "1.0~abc-r1",
            "1.0_rc1~ab",
            "2.4_p1-r1",
            "1.7.1-r0",
        ] {
            assert!(is_valid(v), "{v} should be valid");
        }
    }

    #[test]
    fn invalid_versions_are_rejected() {
        // Each rejected by apk-tools 3.0.8 (`apk version -c`).
        for v in [
            "",
            "a1",
            "1.0b2",
            "1.0bc",
            "1.0~ABC",
            "1.0~",
            "1.0_foo",
            "1.0_RC1",
            "1.0-r",
            "1.0-1",
            "1.0-r1-r2",
            "1.0~ab_rc1",
            "1.0~ab~cd",
            "1.0..1",
            "1.0.",
            "1.0 ",
            "1.0-r1a",
            "1.0a.1",
            "1.0_p1.2",
        ] {
            assert!(!is_valid(v), "{v:?} should be invalid");
        }
    }
}
