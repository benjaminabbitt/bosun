//! Manifest loading (§4.4, §5.4) and the parsed-model hash.
//!
//! The manifest is read through `annals_yaml::versioned`: annals enforces the
//! strict YAML subset (anchors, tags and multiple documents are refused) and
//! the root `version`, migrating older versions through the steps registered
//! in [`loader`]; we only decode the current-version tree into [`Manifest`],
//! surfacing a [`ManifestError`] (an annals `Diagnostic`) for anything
//! structurally wrong — an unknown key, a bad selector, a missing name.
//!
//! [`manifest_hash`] hashes the *parsed model*, not the file text, so a
//! comment-only edit leaves the hash unchanged.

use std::collections::BTreeMap;
use std::fmt;
use std::path::Path;

use annals_diagnostic::{Arg, Diagnostic};
use annals_yaml::versioned::{version_of, Loader, MigrationError, Migrations, Upgraded};
use annals_yaml::Value;
use bosun_core::manifest::{
    is_variable_name, Backends, InstallBlock, Manifest, ManifestEntry, Options, PACKAGE_OPTION,
};
use bosun_core::platform::Selector;
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::errmsg;
use crate::error::IoError;

/// The manifest format version this build reads and writes.
pub const MANIFEST_VERSION: u32 = 2;

/// The keys of an install block. A mapping under `install` whose keys are all
/// among these is a bare block, the `any` selector.
const BLOCK_KEYS: &[&str] = &["backend", "backends", "package", "spec"];

/// Why a manifest's structure is invalid (after annals accepted the YAML).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ManifestError {
    /// A value expected to be a mapping was not.
    NotAMapping { context: String },
    /// A value expected to be a sequence was not.
    NotASequence { context: String },
    /// A value expected to be a scalar was not.
    NotAScalar { context: String },
    /// A key bosun does not recognise (§5.4: unknown key is an error).
    UnknownKey { context: String, key: String },
    /// An `install` key that is not a valid selector, and why.
    BadSelector { value: String, reason: String },
    /// A package entry with no `name`.
    MissingName,
    /// An entry's `var` that is not a variable name (§8.3).
    BadVar { package: String, value: String },
    /// An `install` mapping with both block keys and selector keys.
    InstallMixed { context: String },
    /// An install block with both `backend` and `backends`.
    BackendAndBackends { context: String },
    /// A block-level `package` beside a `backends` map, which names each
    /// backend's package itself.
    PackageBesideBackends { context: String },
    /// A `package` in `defaults.install`, which every entry would share.
    PackageInDefaults { context: String },
    /// A `backends` entry for a backend this build does not have.
    UnknownBackend { package: String, backend: String },
    /// An option the backend does not declare; `allowed` lists the ones it
    /// takes, comma-separated, starting with `package`, which every backend
    /// takes.
    UnknownOption {
        package: String,
        backend: String,
        key: String,
        allowed: String,
    },
}

impl fmt::Display for ManifestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ManifestError::NotAMapping { context } => write!(f, "{context} must be a mapping"),
            ManifestError::NotASequence { context } => write!(f, "{context} must be a sequence"),
            ManifestError::NotAScalar { context } => write!(f, "{context} must be a scalar"),
            ManifestError::UnknownKey { context, key } => {
                write!(f, "unknown key {key:?} in {context}")
            }
            ManifestError::BadSelector { value, reason } => {
                write!(f, "invalid selector {value:?}: {reason}")
            }
            ManifestError::MissingName => f.write_str("a package entry has no `name`"),
            ManifestError::BadVar { package, value } => {
                write!(f, "{package}: var {value:?}: {}", errmsg::BAD_VAR)
            }
            ManifestError::InstallMixed { context } => write!(
                f,
                "{context} mixes a bare install block with selector keys; put the bare block \
                 under `any`"
            ),
            ManifestError::BackendAndBackends { context } => {
                write!(f, "{context} has both `backend` and `backends`; use one")
            }
            ManifestError::PackageBesideBackends { context } => write!(
                f,
                "{context}: `package` beside `backends`; name each backend's package as \
                 backends.<backend>.package"
            ),
            ManifestError::PackageInDefaults { context } => write!(
                f,
                "{context}: `package` names one package, so it belongs in that package's entry, \
                 not defaults.install"
            ),
            ManifestError::UnknownBackend { package, backend } => {
                write!(f, "{package}: `backends` names unknown backend {backend:?}")
            }
            ManifestError::UnknownOption {
                package,
                backend,
                key,
                allowed,
            } => write!(
                f,
                "{package}: unknown key {key:?} in backends.{backend} (allowed: {allowed})"
            ),
        }
    }
}

impl Diagnostic for ManifestError {
    fn code(&self) -> &'static str {
        match self {
            ManifestError::NotAMapping { .. } => "bosun.manifest.not_a_mapping",
            ManifestError::NotASequence { .. } => "bosun.manifest.not_a_sequence",
            ManifestError::NotAScalar { .. } => "bosun.manifest.not_a_scalar",
            ManifestError::UnknownKey { .. } => "bosun.manifest.unknown_key",
            ManifestError::BadSelector { .. } => "bosun.manifest.bad_selector",
            ManifestError::MissingName => "bosun.manifest.missing_name",
            ManifestError::BadVar { .. } => "bosun.manifest.bad_var",
            ManifestError::InstallMixed { .. } => "bosun.manifest.install_mixed",
            ManifestError::BackendAndBackends { .. } => "bosun.manifest.backend_and_backends",
            ManifestError::PackageBesideBackends { .. } => "bosun.manifest.package_beside_backends",
            ManifestError::PackageInDefaults { .. } => "bosun.manifest.package_in_defaults",
            ManifestError::UnknownBackend { .. } => "bosun.manifest.unknown_backend",
            ManifestError::UnknownOption { .. } => "bosun.manifest.unknown_option",
        }
    }

    fn args(&self) -> Vec<(&'static str, Arg)> {
        match self {
            ManifestError::NotAMapping { context }
            | ManifestError::NotASequence { context }
            | ManifestError::NotAScalar { context }
            | ManifestError::InstallMixed { context }
            | ManifestError::BackendAndBackends { context }
            | ManifestError::PackageBesideBackends { context }
            | ManifestError::PackageInDefaults { context } => {
                vec![("context", context.clone().into())]
            }
            ManifestError::UnknownKey { context, key } => {
                vec![
                    ("context", context.clone().into()),
                    ("key", key.clone().into()),
                ]
            }
            ManifestError::BadSelector { value, reason } => vec![
                ("value", value.clone().into()),
                ("reason", reason.clone().into()),
            ],
            ManifestError::MissingName => vec![],
            ManifestError::BadVar { package, value } => vec![
                ("package", package.clone().into()),
                ("value", value.clone().into()),
            ],
            ManifestError::UnknownBackend { package, backend } => vec![
                ("package", package.clone().into()),
                ("backend", backend.clone().into()),
            ],
            ManifestError::UnknownOption {
                package,
                backend,
                key,
                allowed,
            } => vec![
                ("package", package.clone().into()),
                ("backend", backend.clone().into()),
                ("key", key.clone().into()),
                ("allowed", allowed.clone().into()),
            ],
        }
    }
}

type Result2<T> = std::result::Result<T, ManifestError>;

fn as_map<'a>(v: &'a Value, context: &str) -> Result2<&'a [(String, Value)]> {
    v.as_map().ok_or_else(|| ManifestError::NotAMapping {
        context: context.to_string(),
    })
}

fn check_keys(entries: &[(String, Value)], context: &str, allowed: &[&str]) -> Result2<()> {
    for (k, _) in entries {
        if !allowed.contains(&k.as_str()) {
            return Err(ManifestError::UnknownKey {
                context: context.to_string(),
                key: k.clone(),
            });
        }
    }
    Ok(())
}

fn string_seq(v: &Value, context: &str) -> Result2<Vec<String>> {
    v.as_seq()
        .ok_or_else(|| ManifestError::NotASequence {
            context: context.to_string(),
        })?
        .iter()
        .map(|e| {
            e.as_str()
                .map(str::to_string)
                .ok_or_else(|| ManifestError::NotAScalar {
                    context: context.to_string(),
                })
        })
        .collect()
}

fn scalar(v: &Value, context: &str) -> Result2<String> {
    v.as_str()
        .map(str::to_string)
        .ok_or_else(|| ManifestError::NotAScalar {
            context: context.to_string(),
        })
}

/// One backend's options: a mapping of scalars, or empty (`cargo:` / `{}`).
fn options(v: &Value, context: &str) -> Result2<Options> {
    if *v == Value::Null {
        return Ok(Options::new());
    }
    as_map(v, context)?
        .iter()
        .map(|(key, value)| Ok((key.clone(), scalar(value, &format!("{context}.{key}"))?)))
        .collect()
}

/// `backends: {<backend>: {<option>: <scalar>}}`, kept verbatim; which
/// backends and options are valid is [`check_options`]'s job, against the
/// compiled-in backends.
fn backends_map(v: &Value) -> Result2<Backends> {
    as_map(v, "backends")?
        .iter()
        .map(|(backend, section)| {
            Ok((
                backend.clone(),
                options(section, &format!("backends.{backend}"))?,
            ))
        })
        .collect()
}

/// An install block: `backend` (+ `package`) for one backend, or a
/// `backends` map; `package` alone is for an inherited backend.
fn install_block(v: &Value, context: &str) -> Result2<InstallBlock> {
    let entries = as_map(v, context)?;
    check_keys(entries, context, BLOCK_KEYS)?;
    let package = v.get("package").map(|p| scalar(p, "package")).transpose()?;
    let (backends, package) = match (v.get("backend"), v.get("backends")) {
        (Some(_), Some(_)) => {
            return Err(ManifestError::BackendAndBackends {
                context: context.to_string(),
            })
        }
        (Some(one), None) => {
            let mut opts = Options::new();
            if let Some(p) = package {
                opts.insert(PACKAGE_OPTION.to_string(), p);
            }
            (Backends::from([(scalar(one, "backend")?, opts)]), None)
        }
        (None, Some(map)) => {
            if package.is_some() {
                return Err(ManifestError::PackageBesideBackends {
                    context: context.to_string(),
                });
            }
            (backends_map(map)?, None)
        }
        (None, None) => (Backends::new(), package),
    };
    Ok(InstallBlock {
        backends,
        package,
        spec: v.get("spec").map(|s| scalar(s, "spec")).transpose()?,
    })
}

/// Whether an `install` mapping is a bare block: every key a block key.
/// Mixed keys are an error, so a typo'd selector is never read as a block.
fn is_bare_block(entries: &[(String, Value)], context: &str) -> Result2<bool> {
    let block_keys = entries
        .iter()
        .filter(|(k, _)| BLOCK_KEYS.contains(&k.as_str()))
        .count();
    match block_keys {
        0 => Ok(false),
        n if n == entries.len() => Ok(true),
        _ => Err(ManifestError::InstallMixed {
            context: context.to_string(),
        }),
    }
}

/// An `install` value: selector → block, or a bare block for `any`.
fn selector_map(v: &Value, context: &str) -> Result2<BTreeMap<Selector, InstallBlock>> {
    let entries = as_map(v, context)?;
    if is_bare_block(entries, context)? {
        return Ok(BTreeMap::from([(
            Selector::Any,
            install_block(v, context)?,
        )]));
    }
    entries
        .iter()
        .map(|(sel, block)| {
            let selector = sel
                .parse::<Selector>()
                .map_err(|e| ManifestError::BadSelector {
                    value: sel.clone(),
                    reason: e.to_string(),
                })?;
            Ok((selector, install_block(block, &format!("{context}.{sel}"))?))
        })
        .collect()
}

fn entry(v: &Value) -> Result2<ManifestEntry> {
    let entries = as_map(v, "a package")?;
    check_keys(
        entries,
        "a package",
        &["name", "spec", "formerly", "var", "install"],
    )?;
    let name = v
        .get("name")
        .and_then(Value::as_str)
        .ok_or(ManifestError::MissingName)?
        .to_string();
    let formerly = match v.get("formerly") {
        Some(f) => string_seq(f, "formerly")?,
        None => Vec::new(),
    };
    let install = match v.get("install") {
        Some(m) => selector_map(m, &format!("{name}.install"))?,
        None => BTreeMap::new(),
    };
    let var = v
        .get("var")
        .map(|s| scalar(s, &format!("{name}.var")))
        .transpose()?;
    if let Some(value) = var.as_ref().filter(|v| !is_variable_name(v)) {
        return Err(ManifestError::BadVar {
            package: name,
            value: value.clone(),
        });
    }
    Ok(ManifestEntry {
        name,
        spec: v.get("spec").and_then(Value::as_str).map(str::to_string),
        formerly,
        install,
        var,
    })
}

/// `defaults.install`: like an entry's, but naming no package.
fn defaults_install(v: &Value) -> Result2<BTreeMap<Selector, InstallBlock>> {
    let blocks = selector_map(v, "defaults.install")?;
    for (selector, block) in &blocks {
        let named = block.package.is_some()
            || block
                .backends
                .values()
                .any(|o| o.contains_key(PACKAGE_OPTION));
        if named {
            return Err(ManifestError::PackageInDefaults {
                context: format!("defaults.install.{selector}"),
            });
        }
    }
    Ok(blocks)
}

/// Decode a current-version manifest tree into the model.
fn decode(root: &Value) -> Result2<Manifest> {
    let entries = as_map(root, "the manifest root")?;
    check_keys(
        entries,
        "the manifest root",
        &["version", "defaults", "packages"],
    )?;

    let defaults = match root.get("defaults") {
        Some(d) => {
            let dmap = as_map(d, "defaults")?;
            check_keys(dmap, "defaults", &["install"])?;
            match d.get("install") {
                Some(i) => defaults_install(i)?,
                None => BTreeMap::new(),
            }
        }
        None => BTreeMap::new(),
    };

    let packages = match root.get("packages") {
        Some(p) => p
            .as_seq()
            .ok_or_else(|| ManifestError::NotASequence {
                context: "packages".to_string(),
            })?
            .iter()
            .map(entry)
            .collect::<Result2<Vec<_>>>()?,
        None => Vec::new(),
    };

    Ok(Manifest {
        version: MANIFEST_VERSION,
        defaults,
        packages,
    })
}

// --- migrations ------------------------------------------------------------

/// The manifest loader: version 1 and up, migrated to [`MANIFEST_VERSION`].
fn loader() -> Loader {
    Migrations::new("manifest", MANIFEST_VERSION)
        .step(1, v1_to_v2)
        .build()
        .expect("the manifest migration chain is whole")
}

/// Version 1 → 2: `targets` goes (the lock is the machine's own), and each
/// install block's backend list, shared `package` and `ext` sections become
/// one `backends` map. The list's order was a manifest-side preference, which
/// version 2 leaves to the machine, so it is dropped.
fn v1_to_v2(root: &mut Value) -> Result<(), MigrationError> {
    root.remove("targets");
    if let Some(install) = root.get_mut("defaults").and_then(|d| d.get_mut("install")) {
        migrate_selector_map(install);
    }
    if let Some(Value::Seq(packages)) = root.get_mut("packages") {
        for package in packages {
            if let Some(install) = package.get_mut("install") {
                migrate_selector_map(install);
            }
        }
    }
    Ok(())
}

fn migrate_selector_map(install: &mut Value) {
    if let Value::Map(blocks) = install {
        for (_, block) in blocks {
            migrate_block(block);
        }
    }
}

/// One v1 block into v2. Anything not the v1 shape is left for the decoder to
/// report.
fn migrate_block(block: &mut Value) {
    if block.as_map().is_none() {
        return;
    }
    let listed: Vec<String> = match block.remove("backends").or_else(|| block.remove("backend")) {
        Some(Value::Scalar(one)) => vec![one],
        Some(Value::Seq(items)) => items
            .iter()
            .filter_map(|i| i.as_str().map(str::to_string))
            .collect(),
        Some(other) => {
            *block.entry("backends") = other;
            return;
        }
        None => Vec::new(),
    };
    let ext: Vec<(String, Value)> = match block.remove("ext") {
        Some(Value::Map(sections)) => sections,
        Some(_) | None => Vec::new(),
    };
    // v1 read an ext section only for the backend chosen from the list; with no
    // list the backends came from defaults, and the sections name them.
    let names: Vec<String> = if listed.is_empty() {
        ext.iter().map(|(b, _)| b.clone()).collect()
    } else {
        listed
    };
    if names.is_empty() {
        return;
    }
    let shared = block.remove("package");
    let spec = block.remove("spec");
    let section = |name: &str| -> Vec<(String, Value)> {
        let mut opts = Vec::new();
        let own = ext.iter().find(|(b, _)| b == name).map(|(_, s)| s);
        let own_package = own.and_then(|s| s.get(PACKAGE_OPTION)).cloned();
        if let Some(p) = own_package.or_else(|| shared.clone()) {
            opts.push((PACKAGE_OPTION.to_string(), p));
        }
        if let Some(Value::Map(settings)) = own {
            opts.extend(
                settings
                    .iter()
                    .filter(|(k, _)| k != PACKAGE_OPTION)
                    .cloned(),
            );
        }
        opts
    };
    let mut fields = Vec::new();
    match names.as_slice() {
        [one] if section(one).iter().all(|(k, _)| k == PACKAGE_OPTION) => {
            fields.push(("backend".to_string(), Value::scalar(one.as_str())));
            fields.extend(section(one));
        }
        _ => {
            let map = names
                .iter()
                .map(|name| (name.clone(), Value::Map(section(name))))
                .collect();
            fields.push(("backends".to_string(), Value::Map(map)));
        }
    }
    if let Some(spec) = spec {
        fields.push(("spec".to_string(), spec));
    }
    if let Value::Map(rest) = block {
        fields.append(rest);
    }
    *block = Value::Map(fields);
}

// --- loading ---------------------------------------------------------------

fn parse_error(source: &Path, e: impl fmt::Display) -> IoError {
    IoError::Parse {
        path: source.to_path_buf(),
        detail: e.to_string(),
    }
}

/// Load a manifest from YAML text (§4.4), migrating an older version in
/// memory. annals enforces the strict subset and the `version`; structural
/// problems surface as [`IoError::Parse`] carrying the [`ManifestError`]
/// diagnostic.
pub fn load_manifest(text: &str, source: &Path) -> Result<Manifest, IoError> {
    loader()
        .load_as(Some(&source.display().to_string()), text, decode)
        .map_err(|e| parse_error(source, e))
}

/// The manifest's own `version`, unmigrated: edits refuse a file older than
/// [`MANIFEST_VERSION`], since they would splice new-shape entries into it.
pub fn manifest_version(text: &str, source: &Path) -> Result<u32, IoError> {
    let doc = annals_yaml::parse(text).map_err(|e| parse_error(source, e))?;
    version_of(&doc).map_err(|e| parse_error(source, e))
}

/// How deep an upgraded manifest is written in block style: down to each
/// install block, whose scalars go on one line (`linux: {backend: apt}`).
const UPGRADE_FLOW_DEPTH: usize = 4;

/// `text` migrated to [`MANIFEST_VERSION`], or `None` when it is already
/// current. annals edits only the entries the migration changes, so comments
/// elsewhere stay; when it can't edit in place it writes the whole tree and
/// [`Upgraded::comments_kept`] says so.
pub fn upgrade_manifest(text: &str, source: &Path) -> Result<Option<Upgraded>, IoError> {
    let upgraded = loader()
        .upgrade_keeping(
            Some(&source.display().to_string()),
            text,
            UPGRADE_FLOW_DEPTH,
        )
        .map_err(|e| parse_error(source, e))?;
    if let Some(up) = &upgraded {
        // The rewrite must load as written, or it is not written at all.
        load_manifest(&up.text, source)?;
    }
    Ok(upgraded)
}

/// The options each compiled-in backend declares, by backend id. Built by the
/// CLI from every compiled-in backend (present this run or not), like
/// [`crate::BackendSchemes`].
pub type BackendOptionKeys = BTreeMap<String, Vec<String>>;

/// Check every `backends` entry in `m` against the backends this build has: an
/// unknown backend or an undeclared option is an error, never silently
/// ignored. [`PACKAGE_OPTION`] is bosun's and always allowed. What a declared
/// option's value means is the backend's to validate when it reads it.
pub fn check_options(m: &Manifest, known: &BackendOptionKeys) -> Result<(), ManifestError> {
    let blocks = m.defaults.values().map(|b| ("defaults.install", b)).chain(
        m.packages
            .iter()
            .flat_map(|e| e.install.values().map(move |b| (e.name.as_str(), b))),
    );
    for (package, block) in blocks {
        for (backend, options) in &block.backends {
            let allowed = known
                .get(backend)
                .ok_or_else(|| ManifestError::UnknownBackend {
                    package: package.to_string(),
                    backend: backend.clone(),
                })?;
            if let Some(key) = options
                .keys()
                .find(|k| k.as_str() != PACKAGE_OPTION && !allowed.contains(k))
            {
                return Err(ManifestError::UnknownOption {
                    package: package.to_string(),
                    backend: backend.clone(),
                    key: key.clone(),
                    allowed: std::iter::once(PACKAGE_OPTION)
                        .chain(allowed.iter().map(String::as_str))
                        .collect::<Vec<_>>()
                        .join(", "),
                });
            }
        }
    }
    Ok(())
}

// --- canonical hash --------------------------------------------------------

#[derive(Serialize)]
struct CanonManifest {
    version: u32,
    defaults: BTreeMap<String, CanonBlock>,
    packages: Vec<CanonEntry>,
}

#[derive(Serialize)]
struct CanonEntry {
    name: String,
    spec: Option<String>,
    formerly: Vec<String>,
    install: BTreeMap<String, CanonBlock>,
    #[serde(skip_serializing_if = "Option::is_none")]
    var: Option<String>,
}

#[derive(Serialize)]
struct CanonBlock {
    backends: Backends,
    package: Option<String>,
    spec: Option<String>,
}

fn canon_block(b: &InstallBlock) -> CanonBlock {
    CanonBlock {
        backends: b.backends.clone(),
        package: b.package.clone(),
        spec: b.spec.clone(),
    }
}

fn canon_selector_map(m: &BTreeMap<Selector, InstallBlock>) -> BTreeMap<String, CanonBlock> {
    m.iter()
        .map(|(s, b)| (s.to_string(), canon_block(b)))
        .collect()
}

/// A sha256 over a deterministic serialization of the parsed model. Because
/// it hashes the model (comments already stripped by annals, the backends a
/// sorted map), a comment-only or reordering edit does not change it.
pub fn manifest_hash(m: &Manifest) -> String {
    let canon = CanonManifest {
        version: m.version,
        defaults: canon_selector_map(&m.defaults),
        packages: m
            .packages
            .iter()
            .map(|e| CanonEntry {
                name: e.name.clone(),
                spec: e.spec.clone(),
                formerly: e.formerly.clone(),
                install: canon_selector_map(&e.install),
                var: e.var.clone(),
            })
            .collect(),
    };
    let bytes = serde_json::to_vec(&canon).expect("canonical manifest serialization is infallible");
    let hex: String = Sha256::digest(&bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    format!("sha256:{hex}")
}

#[cfg(test)]
mod tests {
    use annals_yaml::versioned::Fixture;

    use super::*;

    const SAMPLE: &str = r#"version: 2
defaults:
  install:
    linux: {backend: cargo}
packages:
  - name: fd
    spec: "10.2.0"
    formerly: [fdfind]
    install:
      linux:debian: {backend: apt, package: fd-find, spec: ">=9|<10"}
      windows:
        backends:
          scoop: {}
          winget: {package: sharkdp.fd}
  - name: jq
    spec: "vers:deb/>=1.7.1|<1.7.2~"
"#;

    fn load(text: &str) -> Manifest {
        load_manifest(text, Path::new("bosun.yaml")).unwrap()
    }

    fn load_err(text: &str) -> String {
        load_manifest(text, Path::new("bosun.yaml"))
            .unwrap_err()
            .to_string()
    }

    fn sel(s: &str) -> Selector {
        s.parse().unwrap()
    }

    fn options(pairs: &[(&str, &str)]) -> Options {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    /// One package, `x`, with `install` written as given (indented under it).
    fn with_install(install: &str) -> String {
        format!("version: 2\npackages:\n  - name: x\n    install:{install}\n")
    }

    // --- the shapes ----------------------------------------------------------

    #[test]
    fn loads_defaults_and_packages() {
        let m = load(SAMPLE);
        assert_eq!(m.version, 2);
        assert_eq!(m.defaults.len(), 1);
        assert_eq!(m.packages.len(), 2);
        let fd = &m.packages[0];
        assert_eq!(fd.name, "fd");
        assert_eq!(fd.spec.as_deref(), Some("10.2.0"));
        assert_eq!(fd.formerly, vec!["fdfind"]);
        assert_eq!(
            fd.install[&sel("linux:debian")].spec.as_deref(),
            Some(">=9|<10")
        );
    }

    #[test]
    fn backend_shorthand_is_one_backend_with_its_package() {
        let deb = &load(SAMPLE).packages[0].install[&sel("linux:debian")];
        assert_eq!(
            deb.backends,
            Backends::from([("apt".to_string(), options(&[("package", "fd-find")]))])
        );
        assert_eq!(deb.package, None);
    }

    #[test]
    fn backends_map_names_each_backend_with_its_options() {
        let windows = &load(SAMPLE).packages[0].install[&sel("windows")];
        assert_eq!(
            windows.backends,
            Backends::from([
                ("scoop".to_string(), Options::new()),
                ("winget".to_string(), options(&[("package", "sharkdp.fd")])),
            ])
        );
    }

    #[test]
    fn a_backend_with_no_options_may_be_written_empty() {
        let m = load(&with_install(
            "\n      linux:\n        backends:\n          cargo:\n          apt: {}",
        ));
        let block = &m.packages[0].install[&sel("linux")];
        assert_eq!(block.backends.len(), 2);
        assert!(block.backends["cargo"].is_empty());
    }

    #[test]
    fn a_package_without_a_backend_is_for_the_inherited_backend() {
        let m = load(&with_install(" {windows: {package: sharkdp.fd}}"));
        let block = &m.packages[0].install[&sel("windows")];
        assert!(block.backends.is_empty());
        assert_eq!(block.package.as_deref(), Some("sharkdp.fd"));
    }

    #[test]
    fn the_any_selector_is_read() {
        let m = load(&with_install(
            " {any: {backend: npm}, windows: {backend: scoop}}",
        ));
        assert_eq!(m.packages[0].install.len(), 2);
        assert!(m.packages[0].install[&Selector::Any]
            .backends
            .contains_key("npm"));
    }

    #[test]
    fn a_bare_install_block_is_the_any_selector() {
        let m = load(&with_install(" {backend: npm, spec: ^3}"));
        let install = &m.packages[0].install;
        assert_eq!(install.keys().collect::<Vec<_>>(), vec![&Selector::Any]);
        assert!(install[&Selector::Any].backends.contains_key("npm"));
        assert_eq!(install[&Selector::Any].spec.as_deref(), Some("^3"));
    }

    #[test]
    fn a_bare_install_block_in_defaults_is_the_any_selector() {
        let m = load("version: 2\ndefaults:\n  install: {backend: cargo}\n");
        assert!(m.defaults[&Selector::Any].backends.contains_key("cargo"));
    }

    // --- refusals ------------------------------------------------------------

    #[test]
    fn a_bare_block_mixed_with_selectors_is_refused() {
        let err = load_err(&with_install(" {backend: npm, windows: {backend: scoop}}"));
        assert!(err.contains("mixes"), "message: {err}");
    }

    #[test]
    fn backend_and_backends_together_are_refused() {
        let err = load_err(&with_install(
            " {linux: {backend: apt, backends: {cargo: {}}}}",
        ));
        assert!(
            err.contains("both `backend` and `backends`"),
            "message: {err}"
        );
    }

    #[test]
    fn a_shared_package_beside_a_backends_map_is_refused() {
        let err = load_err(&with_install(
            " {linux: {package: fd-find, backends: {apt: {}, cargo: {}}}}",
        ));
        assert!(err.contains("backends.<backend>.package"), "message: {err}");
    }

    #[test]
    fn a_package_in_defaults_is_refused() {
        let shorthand =
            "version: 2\ndefaults:\n  install:\n    linux: {backend: apt, package: fd-find}\n";
        let in_map = "version: 2\ndefaults:\n  install:\n    linux:\n      backends: {apt: {package: fd-find}}\n";
        let bare = "version: 2\ndefaults:\n  install:\n    linux: {package: fd-find}\n";
        for text in [shorthand, in_map, bare] {
            let err = load_err(text);
            assert!(err.contains("defaults.install"), "message: {err}");
        }
    }

    #[test]
    fn a_backends_list_is_refused_at_version_2() {
        let err = load_err(&with_install(" {linux: {backends: [cargo, apt]}}"));
        assert!(err.contains("backends must be a mapping"), "message: {err}");
    }

    #[test]
    fn ext_and_targets_are_unknown_at_version_2() {
        let ext = load_err(&with_install(" {linux: {backend: x, ext: {x: {a: b}}}}"));
        assert!(ext.contains("\"ext\""), "message: {ext}");
        let targets = load_err("version: 2\ntargets: [linux/amd64]\n");
        assert!(targets.contains("\"targets\""), "message: {targets}");
    }

    #[test]
    fn an_option_value_must_be_a_scalar() {
        let err = load_err(&with_install(
            " {linux: {backends: {github-release: {asset: [a, b]}}}}",
        ));
        assert!(
            err.contains("backends.github-release.asset"),
            "message: {err}"
        );
    }

    #[test]
    fn a_backends_entry_must_be_a_mapping() {
        let err = load_err(&with_install(
            " {linux: {backends: {github-release: a.tar.gz}}}",
        ));
        assert!(err.contains("backends.github-release"), "message: {err}");
    }

    #[test]
    fn an_entrys_var_is_read() {
        let m = load(
            &with_install(" {backend: apt}")
                .replace("    install:", "    var: FD_TOOL\n    install:"),
        );
        assert_eq!(m.packages[0].var.as_deref(), Some("FD_TOOL"));
        assert_eq!(load(&with_install(" {backend: apt}")).packages[0].var, None);
    }

    #[test]
    fn a_var_that_is_not_a_variable_name_is_refused() {
        for bad in ["fd-tool", "7ZIP", "FD TOOL", "\"\""] {
            let text = with_install(" {backend: apt}")
                .replace("    install:", &format!("    var: {bad}\n    install:"));
            let err = load_err(&text);
            assert!(err.contains("var"), "{bad}: {err}");
        }
        for good in ["FD", "_FD", "fd_2", "Fd"] {
            let text = with_install(" {backend: apt}")
                .replace("    install:", &format!("    var: {good}\n    install:"));
            assert_eq!(load(&text).packages[0].var.as_deref(), Some(good));
        }
    }

    #[test]
    fn a_var_must_be_a_scalar() {
        let text =
            with_install(" {backend: apt}").replace("    install:", "    var: [FD]\n    install:");
        assert!(load_err(&text).contains("var"));
    }

    #[test]
    fn hash_covers_var_and_is_unchanged_without_one() {
        let plain = load(&with_install(" {backend: apt}"));
        let mut named = plain.clone();
        named.packages[0].var = Some("FD".to_string());
        assert_ne!(manifest_hash(&plain), manifest_hash(&named));
        // No `var` serialises nothing, so locks written before `var` existed
        // keep matching.
        let canon = serde_json::to_string(&CanonEntry {
            name: "x".to_string(),
            spec: None,
            formerly: Vec::new(),
            install: BTreeMap::new(),
            var: None,
        })
        .unwrap();
        assert!(!canon.contains("var"), "{canon}");
    }

    #[test]
    fn unknown_key_is_rejected() {
        let err = load_err("version: 2\npackages:\n  - name: fd\n    policy: exact\n");
        assert!(err.contains("policy"), "message: {err}");
    }

    #[test]
    fn non_subset_yaml_is_refused_by_annals() {
        assert!(load_manifest("version: 2\npackages: &a []\n", Path::new("m")).is_err());
    }

    #[test]
    fn bad_selector_is_rejected() {
        assert!(
            load_manifest(&with_install(" {plan9: {backend: cargo}}"), Path::new("m")).is_err()
        );
        assert!(load_manifest(
            &with_install(" {any:debian: {backend: cargo}}"),
            Path::new("m")
        )
        .is_err());
    }

    #[test]
    fn a_bad_selector_says_why() {
        let err = load_err(&with_install(
            "\n      \"linux:gnu@>=two|<\": {backend: cargo}",
        ));
        assert!(
            err.contains("invalid selector \"linux:gnu@>=two|<\""),
            "{err}"
        );
        assert!(
            err.contains(bosun_core::errmsg::PLATFORM_INVALID_LIBC_RANGE),
            "{err}"
        );
    }

    #[test]
    fn libc_selectors_load() {
        let m = load(&with_install(
            "\n      \"linux:gnu@>=2.31\": {backend: cargo}\n      linux:musl: {backend: cargo}",
        ));
        let keys: Vec<String> = m.packages[0]
            .install
            .keys()
            .map(ToString::to_string)
            .collect();
        assert_eq!(keys, vec!["linux:gnu@>=2.31", "linux:musl"]);
    }

    #[test]
    fn a_newer_manifest_is_refused() {
        let err = load_err("version: 3\n");
        assert!(err.contains("newer"), "message: {err}");
    }

    // --- migration -----------------------------------------------------------

    #[test]
    fn version_1_manifests_migrate_to_version_2() {
        loader().assert_fixtures(&[
            Fixture {
                name: "current",
                input: SAMPLE,
                expected: None,
            },
            Fixture {
                name: "targets dropped, shorthand kept",
                input: "version: 1\ntargets: [linux/amd64]\npackages:\n  - name: fd\n    install:\n      linux: {backend: apt, package: fd-find}\n",
                expected: Some("version: 2\npackages:\n  - name: fd\n    install:\n      linux: {backend: apt, package: fd-find}\n"),
            },
            Fixture {
                name: "a one-element list is the shorthand",
                input: "version: 1\npackages:\n  - name: x\n    install:\n      linux: {backends: [cargo]}\n",
                expected: Some("version: 2\npackages:\n  - name: x\n    install:\n      linux: {backend: cargo}\n"),
            },
            Fixture {
                name: "a list becomes a map, the shared package copied to each",
                input: "version: 1\npackages:\n  - name: fd\n    install:\n      linux: {backends: [apt, cargo], package: fd-find, spec: \"9\"}\n",
                expected: Some("version: 2\npackages:\n  - name: fd\n    install:\n      linux:\n        backends: {apt: {package: fd-find}, cargo: {package: fd-find}}\n        spec: \"9\"\n"),
            },
            Fixture {
                name: "ext folds into its backend, a per-backend package winning",
                input: "version: 1\npackages:\n  - name: fd\n    install:\n      windows:\n        backends: [scoop, winget]\n        package: fd\n        ext: {winget: {package: sharkdp.fd}}\n",
                expected: Some("version: 2\npackages:\n  - name: fd\n    install:\n      windows:\n        backends: {scoop: {package: fd}, winget: {package: sharkdp.fd}}\n"),
            },
            Fixture {
                name: "ext on one backend needs the map",
                input: "version: 1\npackages:\n  - name: gum\n    install:\n      linux:\n        backend: github-release\n        package: charmbracelet/gum\n        ext: {github-release: {asset: gum.tar.gz}}\n",
                expected: Some("version: 2\npackages:\n  - name: gum\n    install:\n      linux:\n        backends: {github-release: {package: charmbracelet/gum, asset: gum.tar.gz}}\n"),
            },
            Fixture {
                name: "ext for an inherited backend names that backend",
                input: "version: 1\npackages:\n  - name: duf\n    install:\n      linux:\n        package: muesli/duf\n        ext: {github-release: {asset: duf.tar.gz}}\n",
                expected: Some("version: 2\npackages:\n  - name: duf\n    install:\n      linux:\n        backends: {github-release: {package: muesli/duf, asset: duf.tar.gz}}\n"),
            },
            Fixture {
                name: "ext for a backend the list doesn't name is dropped, as v1 ignored it",
                input: "version: 1\npackages:\n  - name: x\n    install:\n      linux: {backend: cargo, ext: {github-release: {asset: a}}}\n",
                expected: Some("version: 2\npackages:\n  - name: x\n    install:\n      linux: {backend: cargo}\n"),
            },
            Fixture {
                name: "defaults migrate too",
                input: "version: 1\ndefaults:\n  install:\n    darwin: {backends: [brew]}\n    linux: {ext: {github-release: {tag: \"v{{ version }}\"}}}\n",
                expected: Some("version: 2\ndefaults:\n  install:\n    darwin: {backend: brew}\n    linux:\n      backends: {github-release: {tag: \"v{{ version }}\"}}\n"),
            },
        ]);
    }

    #[test]
    fn a_version_1_manifest_loads_as_version_2() {
        let m = load("version: 1\ntargets: [linux/amd64]\npackages:\n  - name: fd\n    install:\n      linux: {backends: [apt, cargo], package: fd-find}\n");
        assert_eq!(m.version, 2);
        let block = &m.packages[0].install[&sel("linux")];
        assert_eq!(block.package_for("cargo"), Some("fd-find"));
        assert_eq!(block.package_for("apt"), Some("fd-find"));
    }

    #[test]
    fn a_manifest_without_a_version_is_too_old() {
        let err = load_err("packages: []\n");
        assert!(err.contains("older"), "message: {err}");
    }

    #[test]
    fn manifest_version_reads_the_stamp_without_migrating() {
        assert_eq!(
            manifest_version("version: 1\n", Path::new("m")).ok(),
            Some(1)
        );
        assert_eq!(
            manifest_version("version: 2\n", Path::new("m")).ok(),
            Some(2)
        );
        assert!(manifest_version("version: x\n", Path::new("m")).is_err());
    }

    #[test]
    fn upgrade_rewrites_an_old_manifest_and_leaves_a_current_one() {
        let old =
            "version: 1\npackages:\n  - name: x\n    install:\n      linux: {backends: [cargo]}\n";
        let upgraded = upgrade_manifest(old, Path::new("m")).unwrap().unwrap();
        assert_eq!(
            upgraded.text,
            "version: 2\npackages:\n  - name: x\n    install:\n      linux: {backend: cargo}\n"
        );
        assert!(upgraded.comments_kept);
        assert_eq!(upgrade_manifest(SAMPLE, Path::new("m")).unwrap(), None);
    }

    #[test]
    fn upgrade_keeps_the_comments_outside_what_it_rewrites() {
        let old = "\
# my tools
version: 1
targets: [linux/amd64]   # just the one
packages:
  # pinned for the CI image
  - name: just
    spec: \"1.40.0\"   # exact
    install:
      linux: {backend: cargo}
  - name: fd
    install:
      linux: {backends: [apt, cargo], package: fd-find}
";
        let upgraded = upgrade_manifest(old, Path::new("m")).unwrap().unwrap();
        assert!(upgraded.comments_kept);
        assert_eq!(
            upgraded.text,
            "\
# my tools
version: 2
packages:
  # pinned for the CI image
  - name: just
    spec: \"1.40.0\"   # exact
    install:
      linux: {backend: cargo}
  - name: fd
    install:
      linux:
        backends:
          apt: {package: fd-find}
          cargo: {package: fd-find}
"
        );
    }

    // --- option checking -----------------------------------------------------

    fn known() -> BackendOptionKeys {
        BackendOptionKeys::from([
            ("apt".to_string(), vec![]),
            ("github-release".to_string(), vec!["asset".to_string()]),
        ])
    }

    const GUM: &str = r#"version: 2
packages:
  - name: gum
    spec: ">=2.0.0|<3.0.0"
    install:
      linux:
        backends:
          github-release:
            package: charmbracelet/gum
            asset: "{{ binary }}_{{ version }}_{{ os | title }}_{{ arch_goreleaser }}.tar.gz"
"#;

    #[test]
    fn options_are_kept_verbatim_per_backend_and_hashed() {
        let m = load(GUM);
        let options = &m.packages[0].install[&sel("linux")].backends["github-release"];
        assert_eq!(
            options["asset"],
            "{{ binary }}_{{ version }}_{{ os | title }}_{{ arch_goreleaser }}.tar.gz"
        );
        let other = GUM.replace("os | title", "os");
        assert_ne!(manifest_hash(&m), manifest_hash(&load(&other)));
    }

    #[test]
    fn check_options_accepts_declared_keys_and_package() {
        assert_eq!(check_options(&load(GUM), &known()), Ok(()));
        let text = with_install(" {linux: {backends: {apt: {package: fd-find}, github-release: {package: sharkdp/fd}}}}");
        assert_eq!(check_options(&load(&text), &known()), Ok(()));
    }

    #[test]
    fn check_options_rejects_a_backend_this_build_does_not_know() {
        let m = load(&GUM.replace("          github-release:", "          gihtub-release:"));
        assert_eq!(
            check_options(&m, &known()),
            Err(ManifestError::UnknownBackend {
                package: "gum".into(),
                backend: "gihtub-release".into(),
            })
        );
    }

    #[test]
    fn check_options_rejects_a_key_the_backend_does_not_declare_and_lists_the_allowed() {
        let m = load(&GUM.replace("            asset:", "            aset:"));
        let err = check_options(&m, &known()).unwrap_err();
        assert_eq!(
            err,
            ManifestError::UnknownOption {
                package: "gum".into(),
                backend: "github-release".into(),
                key: "aset".into(),
                allowed: "package, asset".into(),
            }
        );
        assert!(
            err.to_string()
                .contains("in backends.github-release (allowed: package, asset)"),
            "message: {err}"
        );
    }

    #[test]
    fn check_options_covers_defaults_install() {
        let text =
            "version: 2\ndefaults:\n  install:\n    linux:\n      backends: {apt: {asset: a}}\n";
        let err = check_options(&load(text), &known()).unwrap_err();
        assert_eq!(
            err,
            ManifestError::UnknownOption {
                package: "defaults.install".into(),
                backend: "apt".into(),
                key: "asset".into(),
                allowed: "package".into(),
            }
        );
    }

    // --- hash ----------------------------------------------------------------

    #[test]
    fn hash_ignores_comments_but_not_content() {
        let with_comments = format!("# a comment\n{SAMPLE}# trailing\n");
        assert_eq!(
            manifest_hash(&load(SAMPLE)),
            manifest_hash(&load(&with_comments)),
            "comment-only edits must not change the hash"
        );
        let changed = SAMPLE.replace("10.2.0", "10.3.0");
        assert_ne!(manifest_hash(&load(SAMPLE)), manifest_hash(&load(&changed)));
    }

    #[test]
    fn hash_ignores_how_the_backends_are_written() {
        // The backends are a set: shorthand or map, any order, same manifest.
        let shorthand = with_install(" {linux: {backend: apt, package: fd-find}}");
        let map = with_install(" {linux: {backends: {apt: {package: fd-find}}}}");
        let ab = with_install(" {linux: {backends: {apt: {}, cargo: {}}}}");
        let ba = with_install(" {linux: {backends: {cargo: {}, apt: {}}}}");
        assert_eq!(manifest_hash(&load(&shorthand)), manifest_hash(&load(&map)));
        assert_eq!(manifest_hash(&load(&ab)), manifest_hash(&load(&ba)));
    }

    #[test]
    fn hash_is_sha256_prefixed() {
        assert!(manifest_hash(&load(SAMPLE)).starts_with("sha256:"));
    }

    // --- diagnostics ---------------------------------------------------------

    #[test]
    fn diagnostic_codes_are_stable_per_variant() {
        // The codes are the diagnostic's identity for annals; pin them so a
        // rename is a deliberate, reviewed change, not a silent one.
        assert_eq!(
            ManifestError::NotAMapping {
                context: "x".into()
            }
            .code(),
            "bosun.manifest.not_a_mapping"
        );
        assert_eq!(
            ManifestError::UnknownKey {
                context: "x".into(),
                key: "k".into()
            }
            .code(),
            "bosun.manifest.unknown_key"
        );
        assert_eq!(
            ManifestError::BadSelector {
                value: "s".into(),
                reason: "r".into()
            }
            .code(),
            "bosun.manifest.bad_selector"
        );
        assert_eq!(
            ManifestError::MissingName.code(),
            "bosun.manifest.missing_name"
        );
        assert_eq!(
            ManifestError::BadVar {
                package: "p".into(),
                value: "v".into()
            }
            .code(),
            "bosun.manifest.bad_var"
        );
    }

    #[test]
    fn diagnostic_args_carry_the_offending_values() {
        // `args` feeds the rendered message; an empty list would drop the key
        // and context the diagnostic names.
        let args = ManifestError::UnknownKey {
            context: "the manifest root".into(),
            key: "policy".into(),
        }
        .args();
        let keys: Vec<&str> = args.iter().map(|(k, _)| *k).collect();
        assert_eq!(keys, vec!["context", "key"]);
    }
}
