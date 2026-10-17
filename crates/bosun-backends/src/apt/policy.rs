//! apt's own choice among the versions a spec admits (§5.6), read from
//! `apt-cache policy <pkg>`: every version apt knows, each with the pin
//! priority apt gives it, and which one is installed.
//!
//! apt's candidate is the version of highest priority, the newest among
//! equals, except that below priority 1000 it never goes back past the
//! installed version (apt_preferences(5)). So a NotAutomatic suite
//! (backports, priority 100) loses to the release (500) unless the spec
//! admits only it, and a pin of 1000 or more may take a package down. A
//! version of priority 0 or below is never apt's candidate; bosun takes one
//! only when the spec names it exactly, as `apt-get install pkg=version`
//! installs it despite the pin.
//!
//! Phased updates (Ubuntu's `(phased N%)`) do not change apt's candidate:
//! apt defers them only in `upgrade`, while `apt-get install pkg` takes the
//! phased version, so bosun, which installs by name, takes it too.

use bosun_core::{Scheme, Version, Want};
use bosun_ports::PortError;

/// What dpkg's status file contributes to a version table: it marks the
/// installed version, and offers nothing to install.
const STATUS_FILE: &str = "/var/lib/dpkg/status";

/// The priority from which apt's candidate may go below the installed
/// version.
const DOWNGRADE_PRIORITY: i32 = 1000;

/// One version in apt's version table.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Entry {
    /// The version.
    pub version: Version,
    /// The pin priority apt gives this version.
    pub priority: i32,
    /// Whether some source other than dpkg's status file offers it.
    pub offered: bool,
}

/// `apt-cache policy` for one package.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Policy {
    /// The installed version, when one is.
    pub installed: Option<Version>,
    /// Every version in the version table, newest first as apt lists them.
    pub entries: Vec<Entry>,
}

/// Read `apt-cache policy <pkg>` output (run under `LC_ALL=C`). The version
/// table is read by its layout: a version line is indented five columns
/// (` *** ` marks the installed one) and gives the version and its priority;
/// the source lines under it are indented eight and give each source's own
/// priority and location. An unknown package prints nothing.
pub(crate) fn parse(stdout: &str) -> Result<Policy, PortError> {
    let mut policy = Policy::default();
    for line in stdout.lines() {
        let (installed, rest) = if let Some(rest) = line.strip_prefix(" *** ") {
            (true, rest)
        } else if let Some(rest) = line.strip_prefix("     ").filter(|r| !r.starts_with(' ')) {
            (false, rest)
        } else if let Some(source) = line.strip_prefix("        ") {
            let Some(entry) = policy.entries.last_mut() else {
                return Err(bad(line));
            };
            let location = source.trim().split_once(' ').map(|(_, l)| l.trim());
            if location != Some(STATUS_FILE) {
                entry.offered = true;
            }
            continue;
        } else {
            continue; // the package name and header lines
        };
        let mut words = rest.split_whitespace();
        let (Some(raw), Some(priority)) = (words.next(), words.next()) else {
            return Err(bad(line));
        };
        let version = Version::parse(raw, Scheme::Debian).map_err(|_| bad(line))?;
        let priority = priority.parse::<i32>().map_err(|_| bad(line))?;
        if installed {
            policy.installed = Some(version.clone());
        }
        policy.entries.push(Entry {
            version,
            priority,
            offered: false,
        });
    }
    Ok(policy)
}

fn bad(line: &str) -> PortError {
    PortError::CommandOutput {
        detail: format!("apt-cache policy line {line:?}"),
    }
}

impl Policy {
    /// The versions some source offers to install.
    pub fn offered(&self) -> Vec<Version> {
        self.entries
            .iter()
            .filter(|e| e.offered)
            .map(|e| e.version.clone())
            .collect()
    }

    /// The priority apt gives `version` (0 when apt does not list it).
    pub fn priority(&self, version: &Version) -> i32 {
        self.entries
            .iter()
            .find(|e| &e.version == version)
            .map_or(0, |e| e.priority)
    }

    /// Whether apt's pins keep `version` from being its candidate (priority
    /// 0 or below).
    pub fn pinned_away(&self, version: &Version) -> bool {
        self.priority(version) <= 0
    }

    /// The newest version `want` admits above `chosen`, when apt's
    /// priorities passed one over.
    pub fn passed_over(&self, want: &Want, chosen: &Version) -> Option<Version> {
        let offered = self.offered();
        want.admissible(&offered)
            .into_iter()
            .filter(|v| *v > chosen)
            .max()
            .cloned()
    }

    /// Whether `version` is behind the installed version, which apt's
    /// candidate goes back to only at priority 1000 or more.
    fn behind_installed(&self, version: &Version) -> bool {
        self.installed.as_ref().is_some_and(|i| version < i)
            && self.priority(version) < DOWNGRADE_PRIORITY
    }

    /// The version apt would install among those `want` admits (module
    /// docs), or the first of `prefer` apt may install and `want` admits.
    pub fn choose(&self, want: &Want, prefer: &[Version]) -> Option<Version> {
        let offered = self.offered();
        let admissible: Vec<&Version> = want
            .admissible(&offered)
            .into_iter()
            .filter(|v| !self.pinned_away(v) || want.names_exactly(v))
            .collect();
        if let Some(locked) = prefer.iter().find(|p| admissible.contains(p)) {
            return Some(locked.clone());
        }
        admissible
            .into_iter()
            .max_by(|a, b| {
                let key = |v: &Version| (!self.behind_installed(v), self.priority(v));
                key(a).cmp(&key(b)).then_with(|| a.cmp(b))
            })
            .cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bosun_core::VersRange;

    const BACKPORTS: &str =
        include_str!("../../tests/fixtures/apt/bookworm-policy-curl-backports.txt");
    const BACKPORTS_INSTALLED: &str =
        include_str!("../../tests/fixtures/apt/bookworm-policy-curl-backports-installed.txt");
    const PIN_1001: &str =
        include_str!("../../tests/fixtures/apt/bookworm-policy-curl-pin-1001.txt");
    const PINNED_AWAY: &str =
        include_str!("../../tests/fixtures/apt/bookworm-policy-jq-pinned-away.txt");
    const NEVER: &str = include_str!("../../tests/fixtures/apt/bookworm-policy-jq-never.txt");
    const PHASED: &str = include_str!("../../tests/fixtures/apt/noble-policy-apparmor-phased.txt");
    const PHASED_INSTALLED: &str =
        include_str!("../../tests/fixtures/apt/noble-policy-apparmor-phased-installed.txt");

    const BPO: &str = "8.14.1-2+deb13u2~bpo13+1";
    const STABLE: &str = "7.88.1-10+deb12u15";
    const SECURITY: &str = "7.88.1-10+deb12u5";

    fn deb(v: &str) -> Version {
        Version::parse(v, Scheme::Debian).unwrap()
    }

    fn want(ranges: &[&str]) -> Want {
        Want::new(
            ranges
                .iter()
                .map(|r| VersRange::parse(&format!("vers:deb/{r}")).unwrap())
                .collect(),
        )
    }

    fn choose(policy: &str, ranges: &[&str], prefer: &[&str]) -> Option<String> {
        let prefer: Vec<Version> = prefer.iter().map(|v| deb(v)).collect();
        parse(policy)
            .unwrap()
            .choose(&want(ranges), &prefer)
            .map(|v| v.as_str().to_string())
    }

    #[test]
    fn parse_reads_each_version_its_priority_and_the_installed_one() {
        let p = parse(BACKPORTS_INSTALLED).unwrap();
        assert_eq!(p.installed, Some(deb(BPO)));
        assert_eq!(
            p.entries,
            vec![
                Entry {
                    version: deb(BPO),
                    priority: 100,
                    offered: true
                },
                Entry {
                    version: deb(STABLE),
                    priority: 500,
                    offered: true
                },
                Entry {
                    version: deb(SECURITY),
                    priority: 500,
                    offered: true
                },
            ]
        );
        assert_eq!(parse(BACKPORTS).unwrap().installed, None);
    }

    #[test]
    fn parse_reads_a_version_pin_over_its_sources_priority() {
        let p = parse(PINNED_AWAY).unwrap();
        assert_eq!(p.priority(&deb("1.6-2.1+deb12u2")), -1);
        assert_eq!(p.priority(&deb("1.6-2.1+deb12u3")), 500);
        assert_eq!(p.priority(&deb("9")), 0);
        assert_eq!(parse(PIN_1001).unwrap().priority(&deb(SECURITY)), 1001);
    }

    #[test]
    fn parse_reads_a_phased_version() {
        let p = parse(PHASED).unwrap();
        assert_eq!(
            p.offered(),
            vec![
                deb("4.0.1really4.0.1-0ubuntu0.24.04.9"),
                deb("4.0.0-beta3-0ubuntu3")
            ]
        );
        assert_eq!(p.priority(&deb("4.0.1really4.0.1-0ubuntu0.24.04.9")), 500);
    }

    #[test]
    fn a_version_only_dpkg_status_lists_is_installed_but_not_offered() {
        let p = parse(
            "jq:\n  Installed: 1.5-1\n  Candidate: 1.6-2\n  Version table:\n     \
             1.6-2 500\n        500 http://deb.debian.org/debian bookworm/main amd64 Packages\n \
             *** 1.5-1 100\n        100 /var/lib/dpkg/status\n",
        )
        .unwrap();
        assert_eq!(p.installed, Some(deb("1.5-1")));
        assert_eq!(p.offered(), vec![deb("1.6-2")]);
    }

    #[test]
    fn an_unknown_package_lists_nothing() {
        assert_eq!(parse("").unwrap(), Policy::default());
        assert_eq!(
            parse("awk:\n  Installed: (none)\n  Candidate: (none)\n  Version table:\n").unwrap(),
            Policy::default()
        );
    }

    #[test]
    fn a_line_apt_cache_should_not_print_is_an_error() {
        for line in [
            "        500 http://x before any version\n",
            "     1.0-1\n",
            "     1.0-1 high\n",
        ] {
            assert!(
                matches!(parse(line), Err(PortError::CommandOutput { .. })),
                "{line:?}"
            );
        }
    }

    #[test]
    fn backports_lose_to_the_release_unless_the_spec_admits_only_them() {
        assert_eq!(choose(BACKPORTS, &["*"], &[]).as_deref(), Some(STABLE));
        assert_eq!(choose(BACKPORTS, &[">=8"], &[]).as_deref(), Some(BPO));
        assert_eq!(
            choose(BACKPORTS, &["<7.88.1-10+deb12u10"], &[]).as_deref(),
            Some(SECURITY)
        );
    }

    #[test]
    fn below_1000_the_candidate_does_not_go_back_past_the_installed_version() {
        // apt keeps the installed backport over the release's older one.
        assert_eq!(
            choose(BACKPORTS_INSTALLED, &["*"], &[]).as_deref(),
            Some(BPO)
        );
        // A spec that excludes it takes the release, as an explicit downgrade.
        assert_eq!(
            choose(BACKPORTS_INSTALLED, &["<8"], &[]).as_deref(),
            Some(STABLE)
        );
    }

    #[test]
    fn a_pin_of_1000_or_more_wins_even_downwards() {
        assert_eq!(choose(PIN_1001, &["*"], &[]).as_deref(), Some(SECURITY));
        assert_eq!(
            choose(PIN_1001, &[">=7.88.1-10+deb12u10"], &[]).as_deref(),
            Some(STABLE)
        );
    }

    #[test]
    fn a_version_pinned_away_is_taken_only_when_named_exactly() {
        assert_eq!(
            choose(PINNED_AWAY, &["*"], &[]).as_deref(),
            Some("1.6-2.1+deb12u3")
        );
        assert_eq!(choose(PINNED_AWAY, &["<1.6-2.1+deb12u3"], &[]), None);
        assert_eq!(
            choose(PINNED_AWAY, &["1.6-2.1+deb12u2"], &[]).as_deref(),
            Some("1.6-2.1+deb12u2")
        );
        assert_eq!(choose(NEVER, &["*"], &[]), None);
        assert_eq!(
            choose(NEVER, &["1.6-2.1+deb12u3"], &[]).as_deref(),
            Some("1.6-2.1+deb12u3")
        );
    }

    #[test]
    fn a_phased_version_is_taken_as_apt_get_install_takes_it() {
        let newest = "4.0.1really4.0.1-0ubuntu0.24.04.9";
        assert_eq!(choose(PHASED, &["*"], &[]).as_deref(), Some(newest));
        assert_eq!(
            choose(PHASED_INSTALLED, &["*"], &[]).as_deref(),
            Some(newest)
        );
    }

    #[test]
    fn a_locked_version_apt_may_install_is_kept() {
        assert_eq!(
            choose(BACKPORTS, &["*"], &[SECURITY]).as_deref(),
            Some(SECURITY)
        );
        // The backport, lower priority but admitted, is kept too: the lock chose it.
        assert_eq!(choose(BACKPORTS, &["*"], &[BPO]).as_deref(), Some(BPO));
        // A lock on a version pinned away is not kept for a range.
        assert_eq!(
            choose(PINNED_AWAY, &["*"], &["1.6-2.1+deb12u2"]).as_deref(),
            Some("1.6-2.1+deb12u3")
        );
        // Nor one the spec no longer admits.
        assert_eq!(choose(BACKPORTS, &["<8"], &[BPO]).as_deref(), Some(STABLE));
    }

    #[test]
    fn a_pin_of_exactly_1000_may_go_back_past_the_installed_version() {
        let policy = BACKPORTS_INSTALLED
            .replace("     7.88.1-10+deb12u5 500", "     7.88.1-10+deb12u5 1000");
        assert_eq!(choose(&policy, &["*"], &[]).as_deref(), Some(SECURITY));
        let at_999 =
            BACKPORTS_INSTALLED.replace("     7.88.1-10+deb12u5 500", "     7.88.1-10+deb12u5 999");
        assert_eq!(choose(&at_999, &["*"], &[]).as_deref(), Some(BPO));
    }

    #[test]
    fn priority_0_is_never_the_candidate_either() {
        let policy = PINNED_AWAY.replace("1.6-2.1+deb12u2 -1", "1.6-2.1+deb12u2 0");
        assert_eq!(choose(&policy, &["<1.6-2.1+deb12u3"], &[]), None);
        let p = parse(&policy).unwrap();
        assert!(p.pinned_away(&deb("1.6-2.1+deb12u2")));
        assert!(!p.pinned_away(&deb("1.6-2.1+deb12u3")));
        assert!(parse(PINNED_AWAY)
            .unwrap()
            .pinned_away(&deb("1.6-2.1+deb12u2")));
        assert!(!parse(BACKPORTS).unwrap().pinned_away(&deb(BPO)));
    }

    #[test]
    fn passed_over_names_the_newest_admissible_version_above_the_choice() {
        let p = parse(BACKPORTS).unwrap();
        assert_eq!(p.passed_over(&want(&["*"]), &deb(STABLE)), Some(deb(BPO)));
        assert_eq!(p.passed_over(&want(&["<8"]), &deb(STABLE)), None);
        assert_eq!(p.passed_over(&want(&["*"]), &deb(BPO)), None);
        assert_eq!(
            p.passed_over(&want(&["<8"]), &deb(SECURITY)),
            Some(deb(STABLE))
        );
    }

    #[test]
    fn every_set_must_admit_the_choice() {
        assert_eq!(choose(BACKPORTS, &["*", ">=8"], &[]).as_deref(), Some(BPO));
        assert_eq!(choose(BACKPORTS, &[">=8", "<8"], &[]), None);
    }
}
