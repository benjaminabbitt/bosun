//! dnf's own choice among the builds a spec admits (§5.6): by repository
//! priority, then version.
//!
//! `dnf repoquery` lists every build the enabled repositories offer, with
//! their excludes and module filtering already applied, but not their
//! priority. `dnf install <name>` takes the newest build of the
//! best-priority repository offering the name (the lowest `priority=`
//! number; 99 by default), even when another repository has a newer one,
//! while an explicit `<name>-<version>` from a lower-priority repository
//! installs. So bosun ranks the admitted builds by their best repository's
//! priority, then version, and takes a lower-priority repository's build
//! only when the spec admits nothing better.
//!
//! The priorities are dnf's own configuration as dnf reads it: dnf5 prints
//! it (`dnf --dump-repo-config='*'`); dnf4 is a Python library, asked
//! through its API.

use std::collections::BTreeMap;

use bosun_core::{Version, Want};
use bosun_ports::PortError;

/// dnf's priority for a repository that sets none.
pub(crate) const DEFAULT_PRIORITY: i32 = 99;

/// dnf4's enabled repositories and their priorities, one `id priority` per
/// line, through its Python API (it has no command that prints them).
pub(crate) const DNF4_PRIORITIES: &str = "import dnf; b = dnf.Base(); b.conf.read(); \
     b.conf.substitutions.update_from_etc(b.conf.installroot); b.read_all_repos(); \
     [print(r.id, r.priority) for r in b.repos.iter_enabled()]";

/// Repository id → priority.
pub(crate) type Priorities = BTreeMap<String, i32>;

/// One build `dnf repoquery` lists: its version and the repository it is in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Build {
    /// `[epoch:]version-release`.
    pub version: Version,
    /// The repository id.
    pub repo: String,
}

/// dnf5's `--dump-repo-config='*'`: a `======== "<id>" repository
/// configuration: ========` header per repository, then `key = value` lines.
pub(crate) fn parse_dnf5(stdout: &str) -> Result<Priorities, PortError> {
    let mut found = Priorities::new();
    let mut repo: Option<&str> = None;
    for line in stdout.lines() {
        if let Some(header) = line.strip_prefix("======== \"") {
            repo = header.split_once('"').map(|(id, _)| id);
        } else if let Some(value) = line.strip_prefix("priority = ") {
            let id = repo.ok_or_else(|| bad(line))?;
            found.insert(id.to_string(), value.trim().parse().map_err(|_| bad(line))?);
        }
    }
    Ok(found)
}

/// [`DNF4_PRIORITIES`]'s output: `<id> <priority>` per line.
pub(crate) fn parse_dnf4(stdout: &str) -> Result<Priorities, PortError> {
    let mut found = Priorities::new();
    for line in stdout.lines().filter(|l| !l.trim().is_empty()) {
        let (id, priority) = line.split_once(' ').ok_or_else(|| bad(line))?;
        found.insert(
            id.to_string(),
            priority.trim().parse().map_err(|_| bad(line))?,
        );
    }
    Ok(found)
}

fn bad(line: &str) -> PortError {
    PortError::CommandOutput {
        detail: format!("dnf repository priority line {line:?}"),
    }
}

/// How dnf ranks `builds` (module docs).
pub(crate) struct Ranking<'a> {
    builds: &'a [Build],
    priorities: &'a Priorities,
}

impl<'a> Ranking<'a> {
    pub fn new(builds: &'a [Build], priorities: &'a Priorities) -> Ranking<'a> {
        Ranking { builds, priorities }
    }

    /// Every version offered, each once.
    pub fn versions(&self) -> Vec<Version> {
        let mut versions: Vec<Version> = Vec::new();
        for b in self.builds {
            if !versions.contains(&b.version) {
                versions.push(b.version.clone());
            }
        }
        versions
    }

    /// The best (lowest) priority of a repository offering `version`.
    pub fn priority(&self, version: &Version) -> i32 {
        self.builds
            .iter()
            .filter(|b| &b.version == version)
            .map(|b| {
                self.priorities
                    .get(&b.repo)
                    .copied()
                    .unwrap_or(DEFAULT_PRIORITY)
            })
            .min()
            .unwrap_or(DEFAULT_PRIORITY)
    }

    /// The version dnf would install among those `want` admits, or the
    /// first of `prefer` offered and admitted.
    pub fn choose(&self, want: &Want, prefer: &[Version]) -> Option<Version> {
        let versions = self.versions();
        let admissible = want.admissible(&versions);
        if let Some(locked) = prefer.iter().find(|p| admissible.contains(p)) {
            return Some(locked.clone());
        }
        admissible
            .into_iter()
            .max_by(|a, b| {
                self.priority(b)
                    .cmp(&self.priority(a))
                    .then_with(|| a.cmp(b))
            })
            .cloned()
    }

    /// The newest version `want` admits above `chosen`, when repository
    /// priority passed one over.
    pub fn passed_over(&self, want: &Want, chosen: &Version) -> Option<Version> {
        let versions = self.versions();
        want.admissible(&versions)
            .into_iter()
            .filter(|v| *v > chosen)
            .max()
            .cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bosun_core::{Scheme, VersRange};

    const DNF5_DUMP: &str = include_str!("testdata/dnf5-dump-repo-config.txt");
    const DNF4_PRIORITIES_OUT: &str = include_str!("testdata/dnf4-repo-priorities.txt");

    fn rpm(v: &str) -> Version {
        Version::parse(v, Scheme::Rpm).unwrap()
    }

    fn want(ranges: &[&str]) -> Want {
        Want::new(
            ranges
                .iter()
                .map(|r| VersRange::parse(&format!("vers:rpm/{r}")).unwrap())
                .collect(),
        )
    }

    fn builds(pairs: &[(&str, &str)]) -> Vec<Build> {
        pairs
            .iter()
            .map(|(v, repo)| Build {
                version: rpm(v),
                repo: repo.to_string(),
            })
            .collect()
    }

    fn priorities(pairs: &[(&str, i32)]) -> Priorities {
        pairs.iter().map(|(id, p)| (id.to_string(), *p)).collect()
    }

    #[test]
    fn dnf5_prints_each_repositorys_priority() {
        let p = parse_dnf5(DNF5_DUMP).unwrap();
        assert_eq!(p["fedora"], 1);
        assert_eq!(p["updates"], 99);
        assert_eq!(p["updates-testing"], 99);
    }

    #[test]
    fn dnf4_reports_its_enabled_repositories_priorities() {
        assert_eq!(
            parse_dnf4(DNF4_PRIORITIES_OUT).unwrap(),
            priorities(&[
                ("vault95", 1),
                ("baseos", 99),
                ("appstream", 99),
                ("extras", 99)
            ])
        );
    }

    #[test]
    fn a_priority_line_dnf_should_not_print_is_an_error() {
        assert!(parse_dnf5("priority = 1\n").is_err());
        assert!(
            parse_dnf5("======== \"x\" repository configuration: ========\npriority = high\n")
                .is_err()
        );
        assert!(parse_dnf4("baseos\n").is_err());
        assert!(parse_dnf4("baseos high\n").is_err());
        assert_eq!(parse_dnf4("\n").unwrap(), Priorities::new());
    }

    #[test]
    fn the_best_priority_repository_wins_over_a_newer_build() {
        // Fedora 41 with `priority=1` on fedora: dnf installs its vim-enhanced
        // 9.1.737, though updates has 9.1.1914.
        let b = builds(&[
            ("2:9.1.1914-1.fc41", "updates"),
            ("2:9.1.737-1.fc41", "fedora"),
        ]);
        let p = priorities(&[("fedora", 1), ("updates", 99)]);
        let r = Ranking::new(&b, &p);
        assert_eq!(r.choose(&want(&["*"]), &[]), Some(rpm("2:9.1.737-1.fc41")));
        assert_eq!(
            r.passed_over(&want(&["*"]), &rpm("2:9.1.737-1.fc41")),
            Some(rpm("2:9.1.1914-1.fc41"))
        );
        // A spec admitting only the lower-priority build takes it, as
        // `dnf install name-version` does.
        assert_eq!(
            r.choose(&want(&[">=2:9.1.1000"]), &[]),
            Some(rpm("2:9.1.1914-1.fc41"))
        );
        assert_eq!(
            r.passed_over(&want(&[">=2:9.1.1000"]), &rpm("2:9.1.1914-1.fc41")),
            None
        );
    }

    #[test]
    fn equal_priorities_take_the_newest() {
        let b = builds(&[
            ("7.76.1-31.el9", "vault95"),
            ("7.76.1-40.el9_8.8", "baseos"),
        ]);
        let p = priorities(&[("vault95", 99), ("baseos", 99)]);
        let r = Ranking::new(&b, &p);
        assert_eq!(r.choose(&want(&["*"]), &[]), Some(rpm("7.76.1-40.el9_8.8")));
    }

    #[test]
    fn a_repository_dnf_did_not_report_has_the_default_priority() {
        let b = builds(&[("1.0-1", "known"), ("2.0-1", "unknown")]);
        let r_better = priorities(&[("known", 98)]);
        assert_eq!(Ranking::new(&b, &r_better).priority(&rpm("2.0-1")), 99);
        assert_eq!(
            Ranking::new(&b, &r_better).choose(&want(&["*"]), &[]),
            Some(rpm("1.0-1"))
        );
        let r_worse = priorities(&[("known", 100)]);
        assert_eq!(
            Ranking::new(&b, &r_worse).choose(&want(&["*"]), &[]),
            Some(rpm("2.0-1"))
        );
        assert_eq!(Ranking::new(&b, &r_worse).priority(&rpm("9.0-1")), 99);
    }

    #[test]
    fn a_build_in_several_repositories_has_the_best_of_their_priorities() {
        let b = builds(&[("1.0-1", "a"), ("1.0-1", "b"), ("2.0-1", "c")]);
        let p = priorities(&[("a", 50), ("b", 10), ("c", 20)]);
        let r = Ranking::new(&b, &p);
        assert_eq!(r.priority(&rpm("1.0-1")), 10);
        assert_eq!(r.versions(), vec![rpm("1.0-1"), rpm("2.0-1")]);
        assert_eq!(r.choose(&want(&["*"]), &[]), Some(rpm("1.0-1")));
    }

    #[test]
    fn a_locked_version_offered_and_admitted_is_kept() {
        let b = builds(&[
            ("2:9.1.1914-1.fc41", "updates"),
            ("2:9.1.737-1.fc41", "fedora"),
        ]);
        let p = priorities(&[("fedora", 1), ("updates", 99)]);
        let r = Ranking::new(&b, &p);
        let locked = [rpm("2:9.1.1914-1.fc41")];
        assert_eq!(r.choose(&want(&["*"]), &locked), Some(locked[0].clone()));
        assert_eq!(
            r.choose(&want(&["<2:9.1.1000"]), &locked),
            Some(rpm("2:9.1.737-1.fc41"))
        );
        assert_eq!(r.choose(&want(&[">=3:0"]), &locked), None);
    }
}
