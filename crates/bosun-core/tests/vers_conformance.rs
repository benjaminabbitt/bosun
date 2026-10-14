//! vers conformance against the official vers-spec suites (vendored under
//! tests/conformance/vers, rev 7d0cc191). bosun exercises the cases whose
//! vers type maps onto a §6 scheme it implements (`npm`→SemVer); it reports
//! and skips ecosystems it does not model (datetime, lexicographic, PEP 440,
//! and native→vers conversion suites). See tests/conformance/README.md.

use bosun_core::vers::{scheme_for_type, VersRange};
use bosun_core::Version;
use serde::Deserialize;

const DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/conformance/vers");

/// vers types bosun models (map to a §6 scheme with faithful ordering).
fn in_scope(ty: &str) -> bool {
    matches!(ty, "npm" | "deb" | "cargo" | "golang")
}

#[derive(Deserialize)]
struct ParseSuite {
    tests: Vec<ParseCase>,
}

#[derive(Deserialize)]
struct ParseCase {
    test_type: String,
    input: String,
    // Lenient: a "validate" case uses a string here; we only interpret it as
    // structured components for the in-scope "parse" cases we run.
    #[serde(default)]
    expected_output: Option<serde_json::Value>,
    #[serde(default)]
    expected_failure: bool,
}

#[derive(Deserialize)]
struct ExpectedParse {
    #[serde(rename = "type")]
    ty: String,
    constraints: Vec<(String, String)>,
}

fn ty_of(input: &str) -> &str {
    input
        .strip_prefix("vers:")
        .and_then(|r| r.split('/').next())
        .unwrap_or("")
}

#[test]
fn vers_canonical_parse_conformance() {
    let data = std::fs::read_to_string(format!("{DIR}/vers_canonical_parse_test.json")).unwrap();
    let suite: ParseSuite = serde_json::from_str(&data).unwrap();
    let (mut run, mut skip) = (0, 0);
    for c in &suite.tests {
        if c.test_type != "parse" || !in_scope(ty_of(&c.input)) {
            skip += 1;
            continue;
        }
        run += 1;
        let got = VersRange::parse(&c.input);
        if c.expected_failure {
            assert!(got.is_err(), "expected parse failure for {:?}", c.input);
        } else {
            let range = got.unwrap_or_else(|e| panic!("parse {:?} failed: {e}", c.input));
            let exp: ExpectedParse =
                serde_json::from_value(c.expected_output.clone().unwrap()).unwrap();
            assert_eq!(range.ty(), exp.ty, "type mismatch for {:?}", c.input);
            let got_cs: Vec<(String, String)> = range
                .constraints()
                .iter()
                .map(|k| (k.op.as_str().to_string(), k.version.clone()))
                .collect();
            assert_eq!(
                got_cs, exp.constraints,
                "constraints mismatch for {:?}",
                c.input
            );
        }
    }
    eprintln!("vers canonical parse: {run} run, {skip} skipped (out of scope)");
    // Exact counts, so a fixture or filter regression fails loudly.
    assert_eq!(run, 12, "in-scope vers canonical-parse count changed");
    assert_eq!(skip, 8, "out-of-scope vers canonical-parse count changed");
}

#[derive(Deserialize)]
struct ContainSuite {
    tests: Vec<ContainCase>,
}

#[derive(Deserialize)]
struct ContainCase {
    input: ContainInput,
    expected_output: bool,
}

#[derive(Deserialize)]
struct ContainInput {
    vers: String,
    version: String,
}

#[test]
fn vers_npm_containment_conformance() {
    let data = std::fs::read_to_string(format!("{DIR}/npm_range_containment_test.json")).unwrap();
    let suite: ContainSuite = serde_json::from_str(&data).unwrap();
    for c in &suite.tests {
        let range = VersRange::parse(&c.input.vers).unwrap();
        let v = Version::parse(&c.input.version, scheme_for_type(range.ty())).unwrap();
        assert_eq!(
            range.contains(&v),
            c.expected_output,
            "containment {:?} ∋ {:?}",
            c.input.vers,
            c.input.version
        );
    }
}
