//! Finding a release's published sha256 for one asset. bosun uses a checksum
//! when the release publishes one and does not audit releases that don't: a
//! release with no sums file, or whose sums file doesn't list the asset, has
//! no checksum. Only a download that doesn't match a found checksum fails
//! (enforced by the `Fetch` step).
//!
//! Lookup order: a per-asset file (`<asset>.sha256`, `<asset>.sha256sum`),
//! then a combined sums file by the names releases actually use. Signature
//! files are never sums files.

/// Per-asset checksum file suffixes, in preference order.
const PER_ASSET: &[&str] = &[".sha256", ".sha256sum"];

/// Combined sums files with an exact name, in preference order.
const COMBINED: &[&str] = &[
    "checksums.txt",
    "SHA256SUMS",
    "sha256sum.txt",
    "SHASUMS256.txt",
    "sha256.sum",
    "checksums.sha256",
];

/// Combined sums files named `<anything><suffix>` (`gh_2.80.0_checksums.txt`).
const COMBINED_SUFFIX: &[&str] = &["_checksums.txt", "-checksums.txt"];

/// Signature and certificate files: never read as sums.
const SIGNATURES: &[&str] = &[".asc", ".sig", ".minisig", ".sigstore.json", ".pem"];

/// A sums file to read for the asset, and how to read it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct SumsFile<'a> {
    /// The release asset name of the sums file.
    pub name: &'a str,
    /// A per-asset file, which may hold the bare hex with no name.
    pub per_asset: bool,
}

/// The sums files among `names` that could hold `asset`'s sha256, in lookup
/// order: per-asset files first, then combined files.
pub(super) fn sums_files<'a>(asset: &str, names: &[&'a str]) -> Vec<SumsFile<'a>> {
    let candidates: Vec<&'a str> = names
        .iter()
        .copied()
        .filter(|n| !SIGNATURES.iter().any(|s| n.ends_with(s)))
        .collect();
    let mut found = Vec::new();
    for suffix in PER_ASSET {
        let want = format!("{asset}{suffix}");
        found.extend(candidates.iter().filter(|n| **n == want).map(|n| SumsFile {
            name: n,
            per_asset: true,
        }));
    }
    for exact in COMBINED {
        found.extend(candidates.iter().filter(|n| *n == exact).map(|n| SumsFile {
            name: n,
            per_asset: false,
        }));
    }
    for suffix in COMBINED_SUFFIX {
        found.extend(
            candidates
                .iter()
                .filter(|n| n.len() > suffix.len() && n.ends_with(suffix))
                .map(|n| SumsFile {
                    name: n,
                    per_asset: false,
                }),
        );
    }
    found
}

/// The sha256 (lower-case hex) for `asset` in a sums file body: a
/// `<hex>  <name>` line (`*name` accepted), or, in a per-asset file, a line
/// holding the bare hex. Anything not a 64-digit hex is not a sha256.
pub(super) fn sha256_in(body: &str, asset: &str, per_asset: bool) -> Option<String> {
    body.lines().find_map(|line| {
        let mut it = line.split_whitespace();
        let hex = it.next()?;
        if !is_sha256(hex) {
            return None;
        }
        let named = match it.next() {
            Some(name) => name.trim_start_matches('*') == asset,
            None => per_asset,
        };
        named.then(|| hex.to_ascii_lowercase())
    })
}

fn is_sha256(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHA: &str = "89a356c697c9458929ecf851bc85da3a9548d19aa978837b54c0b0b27100810f";
    const OTHER: &str = "0000000000000000000000000000000000000000000000000000000000000001";
    const ASSET: &str = "tool_1.0.0_linux_amd64.tar.gz";

    fn names_of<'a>(files: &[SumsFile<'a>]) -> Vec<&'a str> {
        files.iter().map(|f| f.name).collect()
    }

    #[test]
    fn per_asset_files_come_first() {
        let names = [
            "checksums.txt",
            ASSET,
            "tool_1.0.0_linux_amd64.tar.gz.sha256sum",
            "tool_1.0.0_linux_amd64.tar.gz.sha256",
        ];
        let files = sums_files(ASSET, &names);
        assert_eq!(
            names_of(&files),
            vec![
                "tool_1.0.0_linux_amd64.tar.gz.sha256",
                "tool_1.0.0_linux_amd64.tar.gz.sha256sum",
                "checksums.txt"
            ]
        );
        assert!(files[0].per_asset && files[1].per_asset && !files[2].per_asset);
    }

    #[test]
    fn another_assets_per_asset_file_is_not_this_ones() {
        let names = ["tool_1.0.0_linux_arm64.tar.gz.sha256"];
        assert!(sums_files(ASSET, &names).is_empty());
    }

    #[test]
    fn each_combined_style_is_found() {
        for name in [
            "checksums.txt",
            "gh_2.80.0_checksums.txt",
            "golangci-lint-2.5.0-checksums.txt",
            "SHA256SUMS",
            "sha256sum.txt",
            "SHASUMS256.txt",
            "sha256.sum",
            "checksums.sha256",
        ] {
            let files = sums_files(ASSET, &[ASSET, name]);
            assert_eq!(names_of(&files), vec![name], "{name}");
            assert!(!files[0].per_asset);
        }
    }

    #[test]
    fn signatures_and_unknown_names_are_not_sums_files() {
        let names = [
            "checksums.txt.asc",
            "checksums.txt.sig",
            "SHA256SUMS.minisig",
            "checksums.txt.sigstore.json",
            "checksums.txt.pem",
            "tool_1.0.0_linux_amd64.tar.gz.sha256.asc",
            // yq's multi-hash file is treated as not published.
            "checksums",
            "_checksums.txt",
        ];
        assert!(sums_files(ASSET, &names).is_empty());
    }

    #[test]
    fn combined_line_with_two_spaces_or_a_star() {
        let body = format!("{OTHER}  other.tar.gz\n{SHA}  {ASSET}\n");
        assert_eq!(sha256_in(&body, ASSET, false), Some(SHA.to_string()));
        let starred = format!("{SHA} *{ASSET}\n");
        assert_eq!(sha256_in(&starred, ASSET, false), Some(SHA.to_string()));
    }

    #[test]
    fn upper_case_hex_is_normalised() {
        let body = format!("{}  {ASSET}\n", SHA.to_ascii_uppercase());
        assert_eq!(sha256_in(&body, ASSET, false), Some(SHA.to_string()));
    }

    #[test]
    fn per_asset_file_may_hold_just_the_hex() {
        assert_eq!(
            sha256_in(&format!("{SHA}\n"), ASSET, true),
            Some(SHA.to_string())
        );
        assert_eq!(
            sha256_in(&format!("{SHA}  {ASSET}\n"), ASSET, true),
            Some(SHA.to_string())
        );
        // A bare hex in a combined file names no asset.
        assert_eq!(sha256_in(&format!("{SHA}\n"), ASSET, false), None);
    }

    #[test]
    fn a_sums_file_not_listing_the_asset_has_no_checksum_for_it() {
        let body = format!("{SHA}  other.tar.gz\n");
        assert_eq!(sha256_in(&body, ASSET, false), None);
    }

    #[test]
    fn a_non_sha256_hash_is_not_used() {
        // sha512 (128 hex) and junk are not a sha256.
        let sha512 = "ab".repeat(64);
        assert_eq!(
            sha256_in(&format!("{sha512}  {ASSET}\n"), ASSET, false),
            None
        );
        assert_eq!(sha256_in(&format!("nothex  {ASSET}\n"), ASSET, false), None);
    }
}
