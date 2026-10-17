//! What one install scope wants of a package (§4.6, §5.6): every claim's vers
//! set at once. A version is wanted when each set contains it; selection then
//! takes the highest wanted version, skipping pre-releases unless some set
//! names one (the §7 guard), and preferring a version a lock already chose.
//!
//! A backend whose package manager ranks versions itself (apt's pin
//! priorities, dnf's repo priorities) or resolves a range itself (npm, uv)
//! reads the same [`Want`] and applies its own order, the latter through its
//! native range ([`Want::npm_range`], [`Want::pep440_sets`]);
//! [`Want::choose`] is the rule for the rest.
//!
//! PyPI follows uv's default pre-release rule too (`if-necessary-or-explicit`):
//! a pre-release is admissible when a set names one, or when every version
//! available is a pre-release.

use crate::native;
use crate::vers::{VersOp, VersRange};
use crate::version::{Scheme, Version};

/// The vers sets one package must satisfy together.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Want {
    ranges: Vec<VersRange>,
}

impl Want {
    /// Every set in `ranges` must contain a wanted version.
    pub fn new(ranges: Vec<VersRange>) -> Want {
        Want { ranges }
    }

    /// One set alone.
    pub fn of(range: VersRange) -> Want {
        Want::new(vec![range])
    }

    /// The sets, in claim order.
    pub fn ranges(&self) -> &[VersRange] {
        &self.ranges
    }

    /// Whether every set contains `version`.
    pub fn admits(&self, version: &Version) -> bool {
        self.ranges.iter().all(|r| r.contains(version))
    }

    /// Whether some set names a pre-release, which lets selection pick one.
    pub fn names_prerelease(&self) -> bool {
        self.ranges.iter().any(VersRange::mentions_prerelease)
    }

    /// Whether some set is one bare version: `version` asked for by name, as
    /// `apt-get install pkg=version` asks.
    pub fn names_exactly(&self, version: &Version) -> bool {
        self.ranges.iter().any(|r| match r.constraints() {
            [c] if c.op == VersOp::Eq => {
                Version::parse(&c.version, r.scheme()).is_ok_and(|v| &v == version)
            }
            _ => false,
        })
    }

    /// Every set at once as one node-semver range, for npm to resolve.
    pub fn npm_range(&self) -> String {
        native::npm_intersection(&self.ranges)
    }

    /// Every set at once as PEP 440 specifier sets, any of which admits a
    /// version, for uv to resolve.
    pub fn pep440_sets(&self) -> Vec<String> {
        native::pep440_intersection(&self.ranges)
    }

    /// Whether the sets are PyPI's, read under PEP 440.
    fn is_pep440(&self) -> bool {
        self.ranges.iter().all(|r| r.scheme() == Scheme::Pep440)
    }

    /// The versions in `available` selection may take: wanted, and not a
    /// pre-release unless some set names one (or, for PyPI, every version
    /// available is one).
    pub fn admissible<'a>(&self, available: &'a [Version]) -> Vec<&'a Version> {
        let only_pre = !available.is_empty() && available.iter().all(Version::is_prerelease);
        let allow_pre = self.names_prerelease() || (self.is_pep440() && only_pre);
        available
            .iter()
            .filter(|v| self.admits(v))
            .filter(|v| allow_pre || !v.is_prerelease())
            .collect()
    }

    /// The version to take from `available`: the first of `prefer` (versions
    /// a lock already chose, best first) that is admissible and available,
    /// else the highest admissible one.
    pub fn choose(&self, available: &[Version], prefer: &[Version]) -> Option<Version> {
        let admissible = self.admissible(available);
        prefer
            .iter()
            .find(|p| admissible.contains(p))
            .or_else(|| admissible.iter().copied().max())
            .cloned()
    }

    /// The sets in canonical vers form, comma-separated, for a diagnostic.
    pub fn summary(&self) -> String {
        self.ranges
            .iter()
            .map(VersRange::to_string)
            .collect::<Vec<_>>()
            .join(", ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::version::Scheme;

    fn want(ranges: &[&str]) -> Want {
        Want::new(
            ranges
                .iter()
                .map(|r| VersRange::parse(r).unwrap())
                .collect(),
        )
    }

    fn deb(raw: &[&str]) -> Vec<Version> {
        raw.iter()
            .map(|v| Version::parse(*v, Scheme::Debian).unwrap())
            .collect()
    }

    fn sem(raw: &[&str]) -> Vec<Version> {
        raw.iter()
            .map(|v| Version::parse(*v, Scheme::Semver).unwrap())
            .collect()
    }

    #[test]
    fn a_wanted_version_is_in_every_set() {
        let w = want(&["vers:deb/>=1.7|<2", "vers:deb/>=1.7.1|<1.7.2~"]);
        let v = deb(&["1.7.0-1", "1.7.1-3", "1.8.0-1"]);
        assert!(!w.admits(&v[0]));
        assert!(w.admits(&v[1]));
        assert!(!w.admits(&v[2]));
    }

    #[test]
    fn choose_takes_the_highest_admissible_version() {
        let w = want(&["vers:deb/>=1.7|<2", "vers:deb/>=1.7.1|<1.7.2~"]);
        let v = deb(&["1.7.0-1", "1.7.1-4", "1.7.1-3", "1.8.0-1"]);
        assert_eq!(w.choose(&v, &[]).unwrap().as_str(), "1.7.1-4");
    }

    #[test]
    fn choose_prefers_the_first_locked_version_that_is_admissible() {
        let w = want(&["vers:deb/>=1.7|<2"]);
        let v = deb(&["1.7.1-3", "1.7.1-4", "1.8.0-1"]);
        let locked = deb(&["2.0-1", "1.7.1-3", "1.7.1-4"]);
        assert_eq!(w.choose(&v, &locked).unwrap().as_str(), "1.7.1-3");
        // A locked version no longer available is passed over.
        let gone = deb(&["1.7.1-9"]);
        assert_eq!(w.choose(&v, &gone).unwrap().as_str(), "1.8.0-1");
    }

    #[test]
    fn choose_is_none_when_nothing_is_admissible() {
        let w = want(&["vers:deb/>=3"]);
        assert_eq!(w.choose(&deb(&["1.0-1", "2.0-1"]), &[]), None);
    }

    #[test]
    fn a_prerelease_is_admissible_only_when_a_set_names_one() {
        let v = sem(&["1.0.0", "1.1.0-rc1"]);
        let plain = want(&["vers:cargo/>=1.0.0"]);
        assert_eq!(plain.choose(&v, &[]).unwrap().as_str(), "1.0.0");
        // A locked pre-release the sets do not name is not kept either.
        assert_eq!(plain.choose(&v, &v[1..]).unwrap().as_str(), "1.0.0");
        let named = want(&["vers:cargo/>=1.0.0", "vers:cargo/>=1.1.0-rc1"]);
        assert!(named.names_prerelease());
        assert_eq!(named.choose(&v, &[]).unwrap().as_str(), "1.1.0-rc1");
    }

    #[test]
    fn names_exactly_is_a_bare_version_set() {
        let v = deb(&["1.0-1", "1.0-2"]);
        let exact = want(&["vers:deb/>=1", "vers:deb/1.0-1"]);
        assert!(exact.names_exactly(&v[0]));
        assert!(!exact.names_exactly(&v[1]));
        assert!(!want(&["vers:deb/>=1.0-1"]).names_exactly(&v[0]));
        assert!(!want(&["vers:deb/1.0-1|1.0-2"]).names_exactly(&v[0]));
    }

    fn pep(raw: &[&str]) -> Vec<Version> {
        raw.iter()
            .map(|v| Version::parse(*v, Scheme::Pep440).unwrap())
            .collect()
    }

    #[test]
    fn pypi_admits_a_prerelease_when_every_version_is_one() {
        let w = want(&["vers:pypi/>=1.0a1"]);
        assert!(w.names_prerelease());
        let plain = want(&["vers:pypi/*"]);
        assert_eq!(
            plain
                .choose(&pep(&["1.0a1", "1.0b2"]), &[])
                .unwrap()
                .as_str(),
            "1.0b2"
        );
        // A final release anywhere keeps pre-releases out, as uv does.
        assert_eq!(
            plain.choose(&pep(&["0.9", "1.0b2"]), &[]).unwrap().as_str(),
            "0.9"
        );
        assert_eq!(
            want(&["vers:pypi/>=1.0"]).choose(&pep(&["0.9", "1.0b2"]), &[]),
            None
        );
        // Other schemes keep the guard alone.
        let only_pre = sem(&["1.0.0-rc.1"]);
        assert_eq!(want(&["vers:cargo/*"]).choose(&only_pre, &[]), None);
        assert_eq!(plain.choose(&[], &[]), None);
    }

    #[test]
    fn the_sets_read_as_npm_and_pep440_ranges() {
        let npm = want(&["vers:npm/>=1.0.0|<2.0.0", "vers:npm/!=1.5.0"]);
        assert_eq!(npm.npm_range(), "<0.0.0-0");
        let npm = want(&["vers:npm/>=1.0.0|<2.0.0", "vers:npm/>=1.5.0"]);
        assert_eq!(npm.npm_range(), ">=1.0.0 <2.0.0 >=1.5.0");
        let pypi = want(&["vers:pypi/>=1.0|<2.0", "vers:pypi/>=1.5"]);
        assert_eq!(pypi.pep440_sets(), vec![">=1.0,<2.0,>=1.5"]);
    }

    #[test]
    fn summary_lists_each_set_canonically() {
        let w = want(&["vers:deb/>=1.7|<2", "vers:deb/1.7.1-3"]);
        assert_eq!(w.summary(), "vers:deb/>=1.7|<2, vers:deb/1.7.1-3");
        assert_eq!(w.ranges().len(), 2);
    }
}
