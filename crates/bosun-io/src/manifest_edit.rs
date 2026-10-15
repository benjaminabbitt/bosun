//! Manifest edits for `bosun add` and `bosun remove` (§4.2, §4.4): one
//! package entry is spliced into or out of the text through annals-yaml's
//! editor, so every other byte of `bosun.yaml`, comments included, stays as
//! the person wrote it.

use std::path::Path;

use annals_yaml::edit::Document;
use annals_yaml::Value;
use bosun_core::manifest::{InstallBlock, ManifestEntry, Options, PACKAGE_OPTION};
use bosun_core::platform::Selector;

use crate::error::IoError;
use crate::manifest::{manifest_version, MANIFEST_VERSION};

/// How deep a new entry is written in block style before collections of
/// scalars go on one line: `formerly: [fdfind]`, `linux: {backend: apt}`.
const ENTRY_FLOW_DEPTH: usize = 2;

/// `text` with `entry` appended to `packages`. An entry with the same name is
/// an error; the manifest keeps one entry per name.
pub fn add_entry(text: &str, source: &Path, entry: &ManifestEntry) -> Result<String, IoError> {
    let mut doc = current_document(text, source)?;
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
    let mut doc = current_document(text, source)?;
    let index = index_of(&doc, name).ok_or_else(|| IoError::NoSuchEntry {
        path: source.to_path_buf(),
        name: name.to_string(),
    })?;
    doc.remove(&["packages"], index)
        .map_err(|e| edit_error(source, e))?;
    Ok(doc.into_string())
}

/// The manifest to edit, refused unless it is at [`MANIFEST_VERSION`]: an
/// edit splices current-shape entries in, which an older file can't hold.
fn current_document(text: &str, source: &Path) -> Result<Document, IoError> {
    let doc = Document::parse(text).map_err(|e| IoError::Parse {
        path: source.to_path_buf(),
        detail: e.to_string(),
    })?;
    let found = manifest_version(text, source)?;
    if found != MANIFEST_VERSION {
        return Err(IoError::ManifestOutdated {
            path: source.to_path_buf(),
            found,
            current: MANIFEST_VERSION,
        });
    }
    Ok(doc)
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
/// `install` by selector (a lone `any` block bare); empty fields are left out.
fn entry_value(entry: &ManifestEntry) -> Value {
    let mut fields = vec![("name".to_string(), scalar(&entry.name))];
    if let Some(spec) = &entry.spec {
        fields.push(("spec".to_string(), scalar(spec)));
    }
    if !entry.formerly.is_empty() {
        let names = entry.formerly.iter().map(|n| scalar(n)).collect();
        fields.push(("formerly".to_string(), Value::Seq(names)));
    }
    let install = match entry.install.iter().collect::<Vec<_>>().as_slice() {
        [] => None,
        [(Selector::Any, block)] => Some(block_value(block)),
        blocks => Some(Value::Map(
            blocks
                .iter()
                .map(|(selector, block)| (selector.to_string(), block_value(block)))
                .collect(),
        )),
    };
    if let Some(install) = install {
        fields.push(("install".to_string(), install));
    }
    Value::Map(fields)
}

/// Whether `options` say nothing but the package, so the one-backend
/// shorthand (`backend: apt, package: fd-find`) can write them.
fn package_only(options: &Options) -> bool {
    options.keys().all(|k| k == PACKAGE_OPTION)
}

fn options_value(options: &Options) -> Value {
    // `package` first: it is what a reader looks for.
    let (package, rest): (Vec<_>, Vec<_>) = options.iter().partition(|(k, _)| *k == PACKAGE_OPTION);
    Value::Map(
        package
            .into_iter()
            .chain(rest)
            .map(|(k, v)| (k.clone(), scalar(v)))
            .collect(),
    )
}

/// An install block: `backend` (+ `package`) for one backend with no other
/// options, else a `backends` map.
fn block_value(block: &InstallBlock) -> Value {
    let mut fields = Vec::new();
    let mut backends = block.backends.iter();
    match (backends.next(), backends.next()) {
        (None, _) => {}
        (Some((one, options)), None) if package_only(options) => {
            fields.push(("backend".to_string(), scalar(one)));
            if let Some(package) = options.get(PACKAGE_OPTION) {
                fields.push(("package".to_string(), scalar(package)));
            }
        }
        _ => fields.push((
            "backends".to_string(),
            Value::Map(
                block
                    .backends
                    .iter()
                    .map(|(backend, options)| (backend.clone(), options_value(options)))
                    .collect(),
            ),
        )),
    }
    if let Some(package) = &block.package {
        fields.push(("package".to_string(), scalar(package)));
    }
    if let Some(spec) = &block.spec {
        fields.push(("spec".to_string(), scalar(spec)));
    }
    Value::Map(fields)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use bosun_core::manifest::Backends;

    use super::*;
    use crate::manifest::load_manifest;

    const MANIFEST: &str = "\
# bosun's own toolbelt.
version: 2

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

    fn options(pairs: &[(&str, &str)]) -> Options {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    fn entry(name: &str, selector: Selector, backends: Backends) -> ManifestEntry {
        ManifestEntry {
            name: name.to_string(),
            spec: Some(">=2.0.2|<3.0.0".to_string()),
            formerly: Vec::new(),
            install: BTreeMap::from([(
                selector,
                InstallBlock {
                    backends,
                    ..InstallBlock::default()
                },
            )]),
        }
    }

    fn gum() -> ManifestEntry {
        entry(
            "gum",
            "linux".parse().unwrap(),
            Backends::from([(
                "github-release".to_string(),
                options(&[
                    ("package", "charmbracelet/gum"),
                    (
                        "asset",
                        "{{ binary }}_{{ version }}_{{ os | title }}_{{ arch_goreleaser }}.tar.gz",
                    ),
                ]),
            )]),
        )
    }

    const GUM_TEXT: &str = "  - name: gum
    spec: \">=2.0.2|<3.0.0\"
    install:
      linux:
        backends:
          github-release: {package: charmbracelet/gum, asset: \"{{ binary }}_{{ version }}_{{ os | title }}_{{ arch_goreleaser }}.tar.gz\"}
";

    #[test]
    fn add_appends_the_entry_and_keeps_the_rest_as_written() {
        let out = add_entry(MANIFEST, path(), &gum()).unwrap();
        assert!(out.starts_with(MANIFEST), "existing text changed:\n{out}");
        assert_eq!(&out[MANIFEST.len()..], GUM_TEXT);
    }

    #[test]
    fn an_added_entry_loads_back_as_itself() {
        let out = add_entry(MANIFEST, path(), &gum()).unwrap();
        let m = load_manifest(&out, path()).unwrap();
        assert_eq!(m.packages.last(), Some(&gum()));
    }

    #[test]
    fn one_backend_with_only_a_package_is_the_shorthand() {
        let fd = entry(
            "fd",
            "linux:debian".parse().unwrap(),
            Backends::from([("apt".to_string(), options(&[("package", "fd-find")]))]),
        );
        let out = add_entry(MANIFEST, path(), &fd).unwrap();
        assert_eq!(
            &out[MANIFEST.len()..],
            "  - name: fd\n    spec: \">=2.0.2|<3.0.0\"\n    install:\n      linux:debian: {backend: apt, package: fd-find}\n"
        );
        assert_eq!(
            load_manifest(&out, path()).unwrap().packages.last(),
            Some(&fd)
        );
    }

    #[test]
    fn several_backends_are_written_as_a_map() {
        let mut fd = entry(
            "fd",
            "windows".parse().unwrap(),
            Backends::from([
                ("scoop".to_string(), Options::new()),
                ("winget".to_string(), options(&[("package", "sharkdp.fd")])),
            ]),
        );
        fd.formerly = vec!["fdfind".to_string()];
        let out = add_entry(MANIFEST, path(), &fd).unwrap();
        assert_eq!(
            &out[MANIFEST.len()..],
            "  - name: fd
    spec: \">=2.0.2|<3.0.0\"
    formerly: [fdfind]
    install:
      windows:
        backends:
          scoop: {}
          winget: {package: sharkdp.fd}
"
        );
        assert_eq!(
            load_manifest(&out, path()).unwrap().packages.last(),
            Some(&fd)
        );
    }

    #[test]
    fn a_lone_any_block_is_written_bare() {
        let prettier = entry(
            "prettier",
            Selector::Any,
            Backends::from([("npm".to_string(), Options::new())]),
        );
        let out = add_entry(MANIFEST, path(), &prettier).unwrap();
        assert_eq!(
            &out[MANIFEST.len()..],
            "  - name: prettier\n    spec: \">=2.0.2|<3.0.0\"\n    install: {backend: npm}\n"
        );
        assert_eq!(
            load_manifest(&out, path()).unwrap().packages.last(),
            Some(&prettier)
        );
    }

    #[test]
    fn a_block_naming_no_backend_writes_only_its_package() {
        let mut fd = entry("fd", "windows".parse().unwrap(), Backends::new());
        fd.install.values_mut().next().unwrap().package = Some("sharkdp.fd".to_string());
        let out = add_entry(MANIFEST, path(), &fd).unwrap();
        assert_eq!(
            &out[MANIFEST.len()..],
            "  - name: fd\n    spec: \">=2.0.2|<3.0.0\"\n    install:\n      windows: {package: sharkdp.fd}\n"
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
        let out = add_entry("version: 2\n", path(), &gum()).unwrap();
        let m = load_manifest(&out, path()).unwrap();
        assert_eq!(m.packages, vec![gum()]);
    }

    #[test]
    fn an_older_manifest_is_not_edited() {
        let old = "version: 1\npackages: []\n";
        let err = add_entry(old, path(), &gum()).unwrap_err();
        assert!(
            matches!(
                err,
                IoError::ManifestOutdated {
                    found: 1,
                    current: 2,
                    ..
                }
            ),
            "{err}"
        );
        assert!(err.to_string().contains("bosun migrate"), "{err}");
        assert!(matches!(
            remove_entry(old, path(), "gum"),
            Err(IoError::ManifestOutdated { .. })
        ));
    }

    #[test]
    fn remove_takes_the_entry_and_its_comment() {
        let added = add_entry(MANIFEST, path(), &gum()).unwrap();
        let out = remove_entry(&added, path(), "jq").unwrap();
        assert_eq!(
            out,
            format!("# bosun's own toolbelt.\nversion: 2\n\npackages:\n{GUM_TEXT}")
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
