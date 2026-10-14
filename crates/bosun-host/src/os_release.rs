//! `/etc/os-release` parsing (§5.4). Follows the freedesktop format: shell
//! `KEY=VALUE` lines, values optionally single- or double-quoted (backslash
//! escapes apply inside double quotes), `#` comments and blank lines ignored.
//! Only `ID`, `ID_LIKE` (space-separated) and `VERSION_ID` are read.
//!
//! A missing file means **no distro** (plain `linux` selectors only, §5.4) —
//! not an error.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use bosun_core::Distro;

const PRIMARY: &str = "/etc/os-release";
const FALLBACK: &str = "/usr/lib/os-release";

/// Parse os-release text into its key/value pairs (values unquoted).
pub fn parse(text: &str) -> BTreeMap<String, String> {
    let mut map = BTreeMap::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((key, value)) = line.split_once('=') {
            map.insert(key.trim().to_string(), unquote(value.trim()));
        }
    }
    map
}

/// Unquote one value per the freedesktop rules.
fn unquote(s: &str) -> String {
    let inner = |q: char| s.len() >= 2 && s.starts_with(q) && s.ends_with(q);
    if inner('"') {
        // Double quotes: backslash escapes `$`, `"`, `` ` `` and `\`.
        let mut out = String::new();
        let mut chars = s[1..s.len() - 1].chars();
        while let Some(c) = chars.next() {
            match c {
                '\\' => match chars.next() {
                    Some(e @ ('$' | '"' | '\\' | '`')) => out.push(e),
                    Some(other) => {
                        out.push('\\');
                        out.push(other);
                    }
                    None => out.push('\\'),
                },
                _ => out.push(c),
            }
        }
        out
    } else if inner('\'') {
        // Single quotes: literal, no escapes.
        s[1..s.len() - 1].to_string()
    } else {
        s.to_string()
    }
}

/// Build distro facts from os-release text. `ID` defaults to `linux` when the
/// file is present but omits it (freedesktop default).
pub fn distro_from(text: &str) -> Distro {
    let map = parse(text);
    Distro {
        id: map
            .get("ID")
            .cloned()
            .unwrap_or_else(|| "linux".to_string()),
        id_like: map
            .get("ID_LIKE")
            .map(|s| s.split_whitespace().map(String::from).collect())
            .unwrap_or_default(),
        version_id: map.get("VERSION_ID").cloned(),
    }
}

/// Detect the host distro by reading os-release, falling back to the
/// `/usr/lib` copy. `None` when neither file exists (§5.4).
pub fn detect() -> Option<Distro> {
    detect_from(&[Path::new(PRIMARY), Path::new(FALLBACK)])
}

/// Read the first existing os-release among `paths`; `None` if none exist.
/// Split out so the fallback logic is testable with temp files.
pub fn detect_from(paths: &[&Path]) -> Option<Distro> {
    for path in paths {
        if let Ok(text) = fs::read_to_string(path) {
            return Some(distro_from(&text));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn distro(id: &str, like: &[&str], ver: Option<&str>) -> Distro {
        Distro {
            id: id.to_string(),
            id_like: like.iter().map(|s| s.to_string()).collect(),
            version_id: ver.map(String::from),
        }
    }

    #[test]
    fn parses_the_distros_from_the_table() {
        // Realistic os-release snippets (§5.4 table).
        let cases: &[(&str, Distro)] = &[
            (
                "NAME=\"Ubuntu\"\nID=ubuntu\nID_LIKE=debian\nVERSION_ID=\"24.04\"\n",
                distro("ubuntu", &["debian"], Some("24.04")),
            ),
            (
                "PRETTY_NAME=\"Debian GNU/Linux 12 (bookworm)\"\nID=debian\nVERSION_ID=\"12\"\n",
                distro("debian", &[], Some("12")),
            ),
            (
                "ID=linuxmint\nID_LIKE=\"ubuntu debian\"\nVERSION_ID=\"22\"\n",
                distro("linuxmint", &["ubuntu", "debian"], Some("22")),
            ),
            (
                "ID=fedora\nVERSION_ID=41\n",
                distro("fedora", &[], Some("41")),
            ),
            (
                "ID=\"rhel\"\nID_LIKE=\"fedora\"\nVERSION_ID=\"9.4\"\n",
                distro("rhel", &["fedora"], Some("9.4")),
            ),
            (
                "ID=rocky\nID_LIKE=\"rhel centos fedora\"\nVERSION_ID=\"9.4\"\n",
                distro("rocky", &["rhel", "centos", "fedora"], Some("9.4")),
            ),
            (
                "ID=amzn\nID_LIKE=fedora\nVERSION_ID=2023\n",
                distro("amzn", &["fedora"], Some("2023")),
            ),
            (
                "ID=opensuse-tumbleweed\nID_LIKE=\"opensuse suse\"\n",
                distro("opensuse-tumbleweed", &["opensuse", "suse"], None),
            ),
            ("ID=arch\n", distro("arch", &[], None)),
            (
                "ID=manjaro\nID_LIKE=arch\n",
                distro("manjaro", &["arch"], None),
            ),
            (
                "ID=alpine\nVERSION_ID=3.20.3\n",
                distro("alpine", &[], Some("3.20.3")),
            ),
        ];
        for (text, expected) in cases {
            assert_eq!(&distro_from(text), expected, "os-release:\n{text}");
        }
    }

    #[test]
    fn ignores_comments_and_blank_lines() {
        let text = "# a comment\n\nID=debian\n\n# another\nVERSION_ID=\"12\"\n";
        assert_eq!(distro_from(text), distro("debian", &[], Some("12")));
        // A comment containing `=` must still be dropped, not parsed as a pair.
        let map = parse("# NAME=ignored\nID=debian\n");
        assert_eq!(map.get("ID").unwrap(), "debian");
        assert!(!map.contains_key("# NAME"));
    }

    #[test]
    fn handles_single_quotes_and_double_quote_escapes() {
        assert_eq!(parse("ID='fedora'\n").get("ID").unwrap(), "fedora");
        // A backslash-escaped quote inside double quotes.
        assert_eq!(
            parse("PRETTY=\"a \\\"b\\\" c\"\n").get("PRETTY").unwrap(),
            "a \"b\" c"
        );
        // A value with only a leading quote is not a quoted value: left literal.
        assert_eq!(parse("X=\"a\n").get("X").unwrap(), "\"a");
        assert_eq!(parse("Y='b\n").get("Y").unwrap(), "'b");
    }

    #[test]
    fn missing_id_defaults_to_linux() {
        assert_eq!(distro_from("VERSION_ID=1\n").id, "linux");
    }

    #[test]
    fn detect_from_reads_first_existing_and_is_none_when_absent() {
        let dir = tempfile::tempdir().unwrap();
        let primary = dir.path().join("os-release");
        let fallback = dir.path().join("usr-os-release");
        let missing = dir.path().join("nope");

        std::fs::write(&fallback, "ID=fedora\n").unwrap();
        // Primary missing → falls back to the second path.
        assert_eq!(detect_from(&[&missing, &fallback]).unwrap().id, "fedora");
        // Primary present wins.
        std::fs::write(&primary, "ID=debian\n").unwrap();
        assert_eq!(detect_from(&[&primary, &fallback]).unwrap().id, "debian");
        // Neither present → None.
        assert!(detect_from(&[&missing]).is_none());
    }
}
