//! Manifest edits for `bosun add` and `bosun remove` (§4.2, §4.4): one
//! package entry is spliced into or out of the text through annals-yaml's
//! editor, so every other byte of `bosun.yaml`, comments included, stays as
//! the person wrote it.

use std::path::Path;

use annals_yaml::edit::Document;
use annals_yaml::Value;
use bosun_core::manifest::{InstallBlock, ManifestEntry};

use crate::error::IoError;

/// How deep a new entry is written in block style before collections of
/// scalars go on one line: `formerly: [fdfind]`, `linux: {backend: apt}`.
const ENTRY_FLOW_DEPTH: usize = 2;

/// `text` with `entry` appended to `packages`. An entry with the same name is
/// an error; the manifest keeps one entry per name.
pub fn add_entry(text: &str, source: &Path, entry: &ManifestEntry) -> Result<String, IoError> {
    let mut doc = document(text, source)?;
    if index_of(&doc, &entry.name).is_some() {
        return Err(IoError::DuplicateEntry {
            path: source.to_path_buf(),
            name: entry.name.clone(),
        });
    }
    doc.push(&["packages"], &entry_value(entry), ENTRY_FLOW_DEPTH)
        .map_err(|e| edit_error(source, e))?;
    Ok(doc.into_string())
}

/// `text` without the entry named `name`.
pub fn remove_entry(text: &str, source: &Path, name: &str) -> Result<String, IoError> {
    let mut doc = document(text, source)?;
    let index = index_of(&doc, name).ok_or_else(|| IoError::NoSuchEntry {
        path: source.to_path_buf(),
        name: name.to_string(),
    })?;
    doc.remove(&["packages"], index)
        .map_err(|e| edit_error(source, e))?;
    Ok(doc.into_string())
}

fn document(text: &str, source: &Path) -> Result<Document, IoError> {
    Document::parse(text).map_err(|e| IoError::Parse {
        path: source.to_path_buf(),
        detail: e.to_string(),
    })
}

fn edit_error(source: &Path, e: annals_yaml::edit::EditError) -> IoError {
    IoError::Edit {
        path: source.to_path_buf(),
        detail: e.to_string(),
    }
}

/// The position of the entry named `name` in `packages`.
fn index_of(doc: &Document, name: &str) -> Option<usize> {
    doc.value()
        .get("packages")?
        .as_seq()?
        .iter()
        .position(|e| e.get("name").and_then(Value::as_str) == Some(name))
}

fn scalar(s: &str) -> Value {
    Value::scalar(s)
}

/// The entry as the manifest writes it: `name`, `spec`, `formerly`, then
/// `install` by selector; empty fields are left out.
fn entry_value(entry: &ManifestEntry) -> Value {
    let mut fields = vec![("name".to_string(), scalar(&entry.name))];
    if let Some(spec) = &entry.spec {
        fields.push(("spec".to_string(), scalar(spec)));
    }
    if !entry.formerly.is_empty() {
        let names = entry.formerly.iter().map(|n| scalar(n)).collect();
        fields.push(("formerly".to_string(), Value::Seq(names)));
    }
    if !entry.install.is_empty() {
        let blocks = entry
            .install
            .iter()
            .map(|(selector, block)| (selector.to_string(), block_value(block)))
            .collect();
        fields.push(("install".to_string(), Value::Map(blocks)));
    }
    Value::Map(fields)
}

/// An install block: `backend` for one, `backends` for a preference list.
fn block_value(block: &InstallBlock) -> Value {
    let mut fields = Vec::new();
    match block.backends.as_slice() {
        [] => {}
        [one] => fields.push(("backend".to_string(), scalar(one))),
        many => fields.push((
            "backends".to_string(),
            Value::Seq(many.iter().map(|b| scalar(b)).collect()),
        )),
    }
    if let Some(package) = &block.package {
        fields.push(("package".to_string(), scalar(package)));
    }
    if let Some(spec) = &block.spec {
        fields.push(("spec".to_string(), scalar(spec)));
    }
    if !block.ext.is_empty() {
        let sections = block
            .ext
            .iter()
            .map(|(backend, settings)| {
                let settings = settings
                    .iter()
                    .map(|(k, v)| (k.clone(), scalar(v)))
                    .collect();
                (backend.clone(), Value::Map(settings))
            })
            .collect();
        fields.push(("ext".to_string(), Value::Map(sections)));
    }
    Value::Map(fields)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::manifest::load_manifest;

    const MANIFEST: &str = "\
# bosun's own toolbelt.
version: 1
targets: [linux/amd64]

packages:
  # Any Debian revision of upstream 1.7.1.
  - name: jq
    spec: \">=1.7.1|<1.7.2~\"
    install:
      linux:debian: {backend: apt}
";

    fn path() -> &'static Path {
        Path::new("bosun.yaml")
    }

    fn gum() -> ManifestEntry {
        let block = InstallBlock {
            backends: vec!["github-release".to_string()],
            package: Some("charmbracelet/gum".to_string()),
            spec: None,
            ext: BTreeMap::from([(
                "github-release".to_string(),
                BTreeMap::from([(
                    "asset".to_string(),
                    "{{ binary }}_{{ version }}_{{ os | title }}_{{ arch_goreleaser }}.tar.gz"
                        .to_string(),
                )]),
            )]),
        };
        ManifestEntry {
            name: "gum".to_string(),
            spec: Some(">=2.0.2|<3.0.0".to_string()),
            formerly: Vec::new(),
            install: BTreeMap::from([("linux".parse().unwrap(), block)]),
        }
    }

    #[test]
    fn add_appends_the_entry_and_keeps_the_rest_as_written() {
        let out = add_entry(MANIFEST, path(), &gum()).unwrap();
        assert!(out.starts_with(MANIFEST), "existing text changed:\n{out}");
        assert_eq!(
            &out[MANIFEST.len()..],
            "  - name: gum
    spec: \">=2.0.2|<3.0.0\"
    install:
      linux:
        backend: github-release
        package: charmbracelet/gum
        ext:
          github-release: {asset: \"{{ binary }}_{{ version }}_{{ os | title }}_{{ arch_goreleaser }}.tar.gz\"}
"
        );
    }

    #[test]
    fn an_added_entry_loads_back_as_itself() {
        let out = add_entry(MANIFEST, path(), &gum()).unwrap();
        let m = load_manifest(&out, path()).unwrap();
        assert_eq!(m.packages.last(), Some(&gum()));
    }

    #[test]
    fn a_preference_list_is_written_as_backends() {
        let mut entry = gum();
        let block = entry.install.values_mut().next().unwrap();
        block.backends = vec!["cargo".to_string(), "apt".to_string()];
        block.package = Some("fd-find".to_string());
        block.ext.clear();
        entry.name = "fd".to_string();
        entry.formerly = vec!["fdfind".to_string()];
        let out = add_entry(MANIFEST, path(), &entry).unwrap();
        // A list inside the block keeps the block itself in block style.
        assert!(
            out.ends_with(
                "  - name: fd
    spec: \">=2.0.2|<3.0.0\"
    formerly: [fdfind]
    install:
      linux:
        backends: [cargo, apt]
        package: fd-find
"
            ),
            "got:\n{out}"
        );
        let m = load_manifest(&out, path()).unwrap();
        assert_eq!(m.packages.last(), Some(&entry));
    }

    #[test]
    fn a_block_of_plain_settings_goes_on_one_line() {
        let entry = ManifestEntry {
            name: "just".to_string(),
            spec: Some(">=1.40|<1.41~".to_string()),
            formerly: Vec::new(),
            install: BTreeMap::from([(
                "linux:debian".parse().unwrap(),
                InstallBlock {
                    backends: vec!["apt".to_string()],
                    ..InstallBlock::default()
                },
            )]),
        };
        let out = add_entry(MANIFEST, path(), &entry).unwrap();
        assert_eq!(
            &out[MANIFEST.len()..],
            "  - name: just\n    spec: \">=1.40|<1.41~\"\n    install:\n      linux:debian: {backend: apt}\n"
        );
    }

    #[test]
    fn adding_a_name_already_there_is_an_error() {
        let mut jq = gum();
        jq.name = "jq".to_string();
        let err = add_entry(MANIFEST, path(), &jq).unwrap_err();
        assert!(matches!(err, IoError::DuplicateEntry { ref name, .. } if name == "jq"));
        assert!(err.to_string().contains("jq"), "{err}");
    }

    #[test]
    fn add_to_a_manifest_with_no_packages_key_adds_it() {
        let out = add_entry("version: 1\n", path(), &gum()).unwrap();
        let m = load_manifest(&out, path()).unwrap();
        assert_eq!(m.packages, vec![gum()]);
    }

    #[test]
    fn remove_takes_the_entry_and_its_comment() {
        let added = add_entry(MANIFEST, path(), &gum()).unwrap();
        let out = remove_entry(&added, path(), "jq").unwrap();
        assert_eq!(
            out,
            "\
# bosun's own toolbelt.
version: 1
targets: [linux/amd64]

packages:
  - name: gum
    spec: \">=2.0.2|<3.0.0\"
    install:
      linux:
        backend: github-release
        package: charmbracelet/gum
        ext:
          github-release: {asset: \"{{ binary }}_{{ version }}_{{ os | title }}_{{ arch_goreleaser }}.tar.gz\"}
"
        );
    }

    #[test]
    fn add_then_remove_restores_the_text() {
        let added = add_entry(MANIFEST, path(), &gum()).unwrap();
        assert_eq!(remove_entry(&added, path(), "gum").unwrap(), MANIFEST);
    }

    #[test]
    fn removing_a_name_not_there_is_an_error() {
        let err = remove_entry(MANIFEST, path(), "gum").unwrap_err();
        assert!(matches!(err, IoError::NoSuchEntry { ref name, .. } if name == "gum"));
    }

    #[test]
    fn a_manifest_that_does_not_parse_is_a_parse_error() {
        let err = remove_entry("a: &x 1\n", path(), "gum").unwrap_err();
        assert!(matches!(err, IoError::Parse { .. }));
    }
}
