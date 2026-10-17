//! apt's source lists (§4.7), as a machine's apt reads them: the one-line
//! `sources.list` format and the deb822 `.sources` format (sources.list(5)).
//! Only binary (`deb`) entries are kept; `deb-src` entries and deb822
//! stanzas with `Enabled: no` are dropped.

use std::fs;
use std::path::Path;

use tracing::debug;

use crate::logmsg;

/// One suite of one repository, as a source list names it. A deb822 stanza
/// with several URIs or suites is one entry per pair.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SourceEntry {
    /// The repository's base URI, without a trailing `/`.
    pub uri: String,
    /// The suite (`bookworm-updates`), or a flat repository's path ending
    /// in `/` (`./`).
    pub suite: String,
    /// The components (`main`, …); none for a flat repository.
    pub components: Vec<String>,
    /// `arch=` / `Architectures:`: the architectures the entry is read for;
    /// none means every one.
    pub architectures: Vec<String>,
    /// `signed-by=` / `Signed-By:`: keyring paths or fingerprints, or an
    /// inline key block.
    pub signed_by: Option<String>,
    /// `trusted=yes` / `Trusted: yes`.
    pub trusted: bool,
    /// Every other option, by its deb822 field name (`Check-Valid-Until`).
    pub options: Vec<(String, String)>,
}

impl SourceEntry {
    /// Whether this is a flat repository: no `dists/`, a path for a suite.
    pub fn is_flat(&self) -> bool {
        self.suite.ends_with('/')
    }

    /// Whether apt reads this entry for `arch`.
    pub fn applies_to(&self, arch: &str) -> bool {
        self.architectures.is_empty() || self.architectures.iter().any(|a| a == arch)
    }
}

/// A one-line option's deb822 field name: sources.list(5) gives the two
/// spellings of each; apt matches field names without regard to case.
fn deb822_name(option: &str) -> String {
    match option {
        "arch" => "Architectures".to_string(),
        "arch+" => "Architectures-Add".to_string(),
        "arch-" => "Architectures-Remove".to_string(),
        "lang" => "Languages".to_string(),
        "lang+" => "Languages-Add".to_string(),
        "lang-" => "Languages-Remove".to_string(),
        "target" => "Targets".to_string(),
        "target+" => "Targets-Add".to_string(),
        "target-" => "Targets-Remove".to_string(),
        other => other
            .split('-')
            .map(|word| {
                let mut chars = word.chars();
                chars.next().map_or(String::new(), |first| {
                    first.to_ascii_uppercase().to_string() + chars.as_str()
                })
            })
            .collect::<Vec<_>>()
            .join("-"),
    }
}

/// Fill `entry`'s option fields from one deb822 field.
fn set_option(entry: &mut SourceEntry, field: &str, value: &str) {
    match field.to_ascii_lowercase().as_str() {
        "architectures" => {
            entry.architectures = value
                .split([' ', '\t', ','])
                .filter(|a| !a.is_empty())
                .map(str::to_string)
                .collect();
        }
        "signed-by" => entry.signed_by = Some(value.to_string()),
        "trusted" => entry.trusted = value.eq_ignore_ascii_case("yes"),
        _ => entry.options.push((field.to_string(), value.to_string())),
    }
}

/// The base URI as apt joins paths onto it.
fn base(uri: &str) -> String {
    uri.trim_end_matches('/').to_string()
}

/// The `deb` entries of a one-line `sources.list`.
pub fn parse_one_line(text: &str) -> Vec<SourceEntry> {
    let mut entries = Vec::new();
    for line in text.lines() {
        let line = line.split('#').next().unwrap_or_default().trim();
        let Some((kind, rest)) = line.split_once(char::is_whitespace) else {
            continue;
        };
        if kind != "deb" {
            continue;
        }
        let mut template = SourceEntry::default();
        let mut rest = rest.trim_start();
        if let Some(opened) = rest.strip_prefix('[') {
            let Some((opts, after)) = opened.split_once(']') else {
                debug!(message = logmsg::APT_SOURCE_SKIPPED, line = %line);
                continue;
            };
            for opt in opts.split_whitespace() {
                let Some((key, value)) = opt.split_once('=') else {
                    continue;
                };
                set_option(&mut template, &deb822_name(key), &value.replace(',', " "));
            }
            rest = after;
        }
        let mut words = rest.split_whitespace();
        let (Some(uri), Some(suite)) = (words.next(), words.next()) else {
            debug!(message = logmsg::APT_SOURCE_SKIPPED, line = %line);
            continue;
        };
        entries.push(SourceEntry {
            uri: base(uri),
            suite: suite.to_string(),
            components: words.map(str::to_string).collect(),
            ..template
        });
    }
    entries
}

/// The fields of each deb822 stanza: name and value, continuation lines
/// joined with newlines (a ` .` line is an empty one).
fn stanzas(text: &str) -> Vec<Vec<(String, String)>> {
    let mut all = Vec::new();
    let mut fields: Vec<(String, String)> = Vec::new();
    for line in text.lines().chain(std::iter::once("")) {
        if line.trim().is_empty() {
            if !fields.is_empty() {
                all.push(std::mem::take(&mut fields));
            }
        } else if line.starts_with('#') {
            continue;
        } else if line.starts_with([' ', '\t']) {
            if let Some((_, value)) = fields.last_mut() {
                let cont = line.trim();
                value.push('\n');
                value.push_str(if cont == "." { "" } else { cont });
            }
        } else if let Some((name, value)) = line.split_once(':') {
            fields.push((name.trim().to_string(), value.trim().to_string()));
        }
    }
    all
}

/// The `deb` entries of a deb822 `.sources` file.
pub fn parse_deb822(text: &str) -> Vec<SourceEntry> {
    let mut entries = Vec::new();
    for fields in stanzas(text) {
        let get = |name: &str| {
            fields
                .iter()
                .find(|(n, _)| n.eq_ignore_ascii_case(name))
                .map(|(_, v)| v.as_str())
        };
        let words = |name: &str| get(name).unwrap_or_default().split_whitespace();
        if get("Enabled").is_some_and(|v| v.eq_ignore_ascii_case("no"))
            || !words("Types").any(|t| t == "deb")
        {
            continue;
        }
        let mut template = SourceEntry::default();
        for (name, value) in &fields {
            let known = ["Types", "URIs", "Suites", "Components", "Enabled"];
            if !known.iter().any(|k| k.eq_ignore_ascii_case(name)) {
                set_option(&mut template, name, value);
            }
        }
        let components: Vec<String> = words("Components").map(str::to_string).collect();
        for uri in words("URIs") {
            for suite in words("Suites") {
                entries.push(SourceEntry {
                    uri: base(uri),
                    suite: suite.to_string(),
                    components: components.clone(),
                    ..template.clone()
                });
            }
        }
    }
    entries
}

/// The `deb` entries a machine's apt reads, from the root at `root`:
/// `etc/apt/sources.list`, then each `etc/apt/sources.list.d` file ending in
/// `.list` or `.sources`, in name order, as apt reads them. Files it can't
/// read are left out.
pub fn read_root(root: &Path) -> Vec<SourceEntry> {
    let apt = root.join("etc/apt");
    let mut entries = fs::read_to_string(apt.join("sources.list"))
        .map(|t| parse_one_line(&t))
        .unwrap_or_default();
    let mut parts: Vec<_> = fs::read_dir(apt.join("sources.list.d"))
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .map(|e| e.path())
        .collect();
    parts.sort();
    for path in parts {
        let Ok(text) = fs::read_to_string(&path) else {
            continue;
        };
        match path.extension().and_then(|e| e.to_str()) {
            Some("list") => entries.extend(parse_one_line(&text)),
            Some("sources") => entries.extend(parse_deb822(&text)),
            _ => {}
        }
    }
    entries
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(uri: &str, suite: &str, components: &[&str]) -> SourceEntry {
        SourceEntry {
            uri: uri.to_string(),
            suite: suite.to_string(),
            components: components.iter().map(|c| c.to_string()).collect(),
            ..SourceEntry::default()
        }
    }

    #[test]
    fn a_one_line_list_gives_its_deb_entries() {
        let text = "\
# a comment
deb http://archive.ubuntu.com/ubuntu/ jammy main restricted # trailing
deb-src http://archive.ubuntu.com/ubuntu/ jammy main

deb [arch=amd64,arm64 signed-by=/usr/share/keyrings/x.gpg check-valid-until=no] https://x.example/apt stable main
deb https://flat.example/repo ./
deb [broken https://x.example y z
deb lonely
";
        let got = parse_one_line(text);
        assert_eq!(got.len(), 3, "{got:#?}");
        assert_eq!(
            got[0],
            entry(
                "http://archive.ubuntu.com/ubuntu",
                "jammy",
                &["main", "restricted"]
            )
        );
        assert_eq!(got[1].architectures, vec!["amd64", "arm64"]);
        assert_eq!(
            got[1].signed_by.as_deref(),
            Some("/usr/share/keyrings/x.gpg")
        );
        assert_eq!(
            got[1].options,
            vec![("Check-Valid-Until".to_string(), "no".to_string())]
        );
        assert!(!got[1].trusted);
        assert!(got[2].is_flat());
        assert!(got[2].components.is_empty());
        assert!(!got[0].is_flat());
    }

    #[test]
    fn one_line_options_take_their_deb822_names() {
        assert_eq!(deb822_name("arch"), "Architectures");
        assert_eq!(deb822_name("arch+"), "Architectures-Add");
        assert_eq!(deb822_name("arch-"), "Architectures-Remove");
        assert_eq!(deb822_name("lang"), "Languages");
        assert_eq!(deb822_name("lang+"), "Languages-Add");
        assert_eq!(deb822_name("lang-"), "Languages-Remove");
        assert_eq!(deb822_name("target"), "Targets");
        assert_eq!(deb822_name("target+"), "Targets-Add");
        assert_eq!(deb822_name("target-"), "Targets-Remove");
        assert_eq!(deb822_name("by-hash"), "By-Hash");
        assert_eq!(deb822_name("pdiffs"), "Pdiffs");
        assert_eq!(deb822_name("a--b"), "A--B");
    }

    #[test]
    fn trusted_and_arch_options_are_read_from_one_line() {
        let got = parse_one_line("deb [trusted=yes arch=arm64] https://a.example s c\n");
        assert!(got[0].trusted);
        assert!(got[0].applies_to("arm64"));
        assert!(!got[0].applies_to("amd64"));
        assert!(entry("u", "s", &[]).applies_to("amd64"));
        let no = parse_one_line("deb [trusted=no] https://a.example s c\n");
        assert!(!no[0].trusted);
    }

    #[test]
    fn a_deb822_file_gives_one_entry_per_uri_and_suite() {
        let text = "\
Types: deb
# http://snapshot.debian.org/archive/debian/20261005T000000Z
URIs: http://deb.debian.org/debian
Suites: bookworm bookworm-updates
Components: main
Signed-By: /usr/share/keyrings/debian-archive-keyring.gpg

Types: deb-src
URIs: http://deb.debian.org/debian
Suites: bookworm
Components: main

Types: deb deb-src
URIs: https://a.example/x/ https://b.example/x
Suites: s
Components: main contrib
Architectures: arm64
Enabled: yes
Trusted: yes
X-Repolib-Name: vendor

Types: deb
URIs: https://off.example
Suites: s
Components: main
Enabled: no
";
        let got = parse_deb822(text);
        assert_eq!(got.len(), 4, "{got:#?}");
        assert_eq!(got[0].suite, "bookworm");
        assert_eq!(got[1].suite, "bookworm-updates");
        assert_eq!(got[1].uri, "http://deb.debian.org/debian");
        assert_eq!(
            got[1].signed_by.as_deref(),
            Some("/usr/share/keyrings/debian-archive-keyring.gpg")
        );
        assert_eq!(got[2].uri, "https://a.example/x");
        assert_eq!(got[3].uri, "https://b.example/x");
        assert_eq!(got[2].components, vec!["main", "contrib"]);
        assert_eq!(got[2].architectures, vec!["arm64"]);
        assert!(got[2].trusted);
        assert_eq!(
            got[2].options,
            vec![("X-Repolib-Name".to_string(), "vendor".to_string())]
        );
    }

    #[test]
    fn a_deb822_inline_key_keeps_its_lines() {
        let text = "\
types: deb
uris: https://a.example
suites: s
components: main
signed-by:
 -----BEGIN PGP PUBLIC KEY BLOCK-----
 .
 abc
 -----END PGP PUBLIC KEY BLOCK-----
";
        let got = parse_deb822(text);
        assert_eq!(
            got[0].signed_by.as_deref(),
            Some(
                "\n-----BEGIN PGP PUBLIC KEY BLOCK-----\n\nabc\n-----END PGP PUBLIC KEY BLOCK-----"
            )
        );
    }

    #[test]
    fn a_root_is_read_as_apt_reads_it() {
        let dir = tempfile::tempdir().unwrap();
        let apt = dir.path().join("etc/apt");
        fs::create_dir_all(apt.join("sources.list.d")).unwrap();
        fs::write(apt.join("sources.list"), "deb https://a.example a main\n").unwrap();
        fs::write(
            apt.join("sources.list.d/b.sources"),
            "Types: deb\nURIs: https://b.example\nSuites: b\nComponents: main\n",
        )
        .unwrap();
        fs::write(
            apt.join("sources.list.d/c.list"),
            "deb https://c.example c main\n",
        )
        .unwrap();
        fs::write(
            apt.join("sources.list.d/d.list.save"),
            "deb https://d.example d main\n",
        )
        .unwrap();
        fs::create_dir(apt.join("sources.list.d/e.list")).unwrap();
        let suites: Vec<_> = read_root(dir.path()).into_iter().map(|e| e.suite).collect();
        assert_eq!(suites, vec!["a", "b", "c"]);
        assert!(read_root(&dir.path().join("none")).is_empty());
    }
}
