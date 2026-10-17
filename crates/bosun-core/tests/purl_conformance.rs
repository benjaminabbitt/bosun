//! purl conformance against the official purl-spec suites (vendored under
//! tests/conformance/purl, rev ebbeb6fd). The core spec suite and the types
//! bosun installs (deb, cargo, github, generic, npm, pypi) must pass every case;
//! the other registered types run best-effort under the type-agnostic rules
//! (their type-specific quirks are out of M1 scope). See the fixtures README.

use std::collections::BTreeMap;
use std::str::FromStr;

use bosun_core::purl::Purl;
use serde::Deserialize;
use serde_json::{json, Value};

const DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/conformance/purl");

#[derive(Deserialize)]
struct Suite {
    tests: Vec<Case>,
}

#[derive(Deserialize)]
struct Case {
    test_type: String,
    input: Value,
    #[serde(default)]
    expected_output: Value,
    #[serde(default)]
    expected_failure: bool,
}

/// Build a canonical purl from a `build` test's component object.
fn build_from(obj: &Value) -> Result<Purl, bosun_core::PurlError> {
    let s = |k: &str| obj.get(k).and_then(Value::as_str);
    let quals: BTreeMap<String, String> = obj
        .get("qualifiers")
        .and_then(Value::as_object)
        .map(|m| {
            m.iter()
                .filter_map(|(k, v)| v.as_str().map(|v| (k.clone(), v.to_string())))
                .collect()
        })
        .unwrap_or_default();
    Purl::from_components(
        s("type").unwrap_or(""),
        s("namespace"),
        s("name").unwrap_or(""),
        s("version"),
        quals,
        s("subpath"),
    )
}

/// The parsed purl as the suite's component JSON, for comparison.
fn purl_to_value(p: &Purl) -> Value {
    let quals = if p.qualifiers().is_empty() {
        Value::Null
    } else {
        json!(p
            .qualifiers()
            .iter()
            .map(|(k, v)| (k.clone(), json!(v)))
            .collect::<serde_json::Map<_, _>>())
    };
    json!({
        "type": p.ty(),
        "namespace": p.namespace(),
        "name": p.name(),
        "version": p.version(),
        "qualifiers": quals,
        "subpath": p.subpath(),
    })
}

/// Run one case. Returns Ok(()) on conformance, Err(reason) otherwise.
fn run_case(c: &Case) -> Result<(), String> {
    match c.test_type.as_str() {
        "parse" => {
            let input = c.input.as_str().ok_or("parse input not a string")?;
            match (Purl::from_str(input), c.expected_failure) {
                (Err(_), true) => Ok(()),
                (Ok(p), false) if purl_to_value(&p) == c.expected_output => Ok(()),
                (got, _) => Err(format!("parse {input:?}: got {got:?}")),
            }
        }
        "validate" => {
            let input = c.input.as_str().ok_or("validate input not a string")?;
            match (Purl::from_str(input), c.expected_failure) {
                (Err(_), true) => Ok(()),
                (Ok(p), false) if c.expected_output.as_str() == Some(p.to_string().as_str()) => {
                    Ok(())
                }
                (got, _) => Err(format!("validate {input:?}: got {got:?}")),
            }
        }
        "build" => match (build_from(&c.input), c.expected_failure) {
            (Err(_), true) => Ok(()),
            (Ok(p), false) if c.expected_output.as_str() == Some(p.to_string().as_str()) => Ok(()),
            (got, _) => Err(format!("build {:?}: got {got:?}", c.input)),
        },
        other => Err(format!("unknown test_type {other}")),
    }
}

fn load(name: &str) -> Vec<Case> {
    let data = std::fs::read_to_string(format!("{DIR}/{name}-test.json")).unwrap();
    serde_json::from_str::<Suite>(&data).unwrap().tests
}

#[test]
fn purl_spec_and_m1_types_conformance() {
    let mut run = 0;
    for suite in [
        "specification",
        "deb",
        "cargo",
        "github",
        "generic",
        "npm",
        "pypi",
    ] {
        for c in &load(suite) {
            run += 1;
            if let Err(e) = run_case(c) {
                panic!("[{suite}] {e}");
            }
        }
    }
    // Exact count, so a fixture or filter regression fails loudly.
    assert_eq!(run, 98, "in-scope purl conformance case count changed");
}

#[test]
fn purl_other_types_best_effort() {
    let mut run = 0;
    let mut pass = 0;
    for suite in ["rpm", "gem", "golang", "apk", "alpm", "brew"] {
        for c in &load(suite) {
            run += 1;
            if run_case(c).is_ok() {
                pass += 1;
            }
        }
    }
    eprintln!("purl other-type cases: {pass}/{run} pass under type-agnostic rules");
    // Loud on a fixture regression; type-specific failures are expected and
    // out of M1 scope (documented in tests/conformance/README.md).
    assert_eq!(run, 74, "other-type purl case count changed");
}
