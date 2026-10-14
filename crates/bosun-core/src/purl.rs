//! purl — the Package-URL identity (ECMA-427, §5.6): `pkg:type/namespace/
//! name@version?qualifiers#subpath`. bosun records the **unversioned**
//! canonical purl (plus `backend`) as a package's identity in the lock and
//! machine state (§4.6), replacing a separate package id and source.
//!
//! bosun implements the general purl algorithm and the type-specific
//! normalisation M1 installs (`deb`, `cargo`, `github`, `generic`). Other
//! registered types parse under the type-agnostic rules; their type-specific
//! quirks are out of M1 scope (and reported as skipped conformance cases).
//! Unregistered types are allowed (`pkg:winget/sharkdp.fd`), per the spec.

use std::collections::BTreeMap;
use std::fmt;
use std::str::FromStr;

use crate::errmsg;

/// A parsed, canonical Package-URL.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Purl {
    ty: String,
    namespace: Option<String>,
    name: String,
    version: Option<String>,
    qualifiers: BTreeMap<String, String>,
    subpath: Option<String>,
}

/// Why a purl string or component set is invalid.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum PurlError {
    /// Missing the literal `pkg:` scheme.
    #[error("{kind}: {0:?}", kind = errmsg::PURL_NO_SCHEME)]
    NoScheme(String),
    /// The type is empty or has an invalid character.
    #[error("{kind}: {0:?}", kind = errmsg::PURL_INVALID_TYPE)]
    InvalidType(String),
    /// No name was given.
    #[error("{kind}", kind = errmsg::PURL_EMPTY_NAME)]
    EmptyName,
    /// A qualifier key is empty or has an invalid character.
    #[error("{kind}: {0:?}", kind = errmsg::PURL_INVALID_QUALIFIER)]
    InvalidQualifier(String),
    /// A `%HH` escape was malformed.
    #[error("{kind}: {0:?}", kind = errmsg::PURL_BAD_PERCENT)]
    BadPercent(String),
}

impl Purl {
    /// Build a canonical purl from components (the `build` conformance path),
    /// validating and normalising: lowercase type, per-type lowercasing of
    /// namespace/name, sorted qualifiers with empty values dropped, and a
    /// trimmed subpath.
    pub fn from_components(
        ty: &str,
        namespace: Option<&str>,
        name: &str,
        version: Option<&str>,
        qualifiers: BTreeMap<String, String>,
        subpath: Option<&str>,
    ) -> Result<Purl, PurlError> {
        if !valid_type(ty) {
            return Err(PurlError::InvalidType(ty.to_string()));
        }
        let ty = ty.to_ascii_lowercase();
        let lower = type_lowercases(&ty);

        let namespace = namespace
            .map(|ns| segments(ns, lower).join("/"))
            .filter(|ns| !ns.is_empty());

        let name = non_empty(name).ok_or(PurlError::EmptyName)?;
        let name = if lower {
            name.to_ascii_lowercase()
        } else {
            name.to_string()
        };

        let version = version.and_then(non_empty).map(str::to_string);

        let mut quals = BTreeMap::new();
        for (k, v) in qualifiers {
            if v.is_empty() {
                continue; // empty qualifier values are dropped (canonical)
            }
            let k = k.to_ascii_lowercase();
            if !valid_qualifier_key(&k) {
                return Err(PurlError::InvalidQualifier(k));
            }
            quals.insert(k, v);
        }

        let subpath = subpath
            .map(|sp| subpath_segments(sp).join("/"))
            .filter(|sp| !sp.is_empty());

        Ok(Purl {
            ty,
            namespace,
            name,
            version,
            qualifiers: quals,
            subpath,
        })
    }

    /// The purl type (lowercased), e.g. `deb`, `cargo`, `github`.
    pub fn ty(&self) -> &str {
        &self.ty
    }
    /// The namespace, if any.
    pub fn namespace(&self) -> Option<&str> {
        self.namespace.as_deref()
    }
    /// The name.
    pub fn name(&self) -> &str {
        &self.name
    }
    /// The version, if any.
    pub fn version(&self) -> Option<&str> {
        self.version.as_deref()
    }
    /// The qualifiers, sorted by key.
    pub fn qualifiers(&self) -> &BTreeMap<String, String> {
        &self.qualifiers
    }
    /// The subpath, if any.
    pub fn subpath(&self) -> Option<&str> {
        self.subpath.as_deref()
    }

    /// The same purl with its version cleared — the unversioned identity bosun
    /// records (§5.6).
    pub fn unversioned(&self) -> Purl {
        Purl {
            version: None,
            ..self.clone()
        }
    }

    /// The backend's native package id derived from the purl (§5.6): `github`
    /// uses `namespace/name`; every other M1 type uses the bare `name`.
    pub fn native_id(&self) -> String {
        match (self.ty.as_str(), &self.namespace) {
            ("github", Some(ns)) => format!("{ns}/{}", self.name),
            _ => self.name.clone(),
        }
    }
}

impl FromStr for Purl {
    type Err = PurlError;
    fn from_str(s: &str) -> Result<Purl, PurlError> {
        // Scheme: a literal `pkg:` (not percent-encoded), then optional slashes.
        let rest = s
            .strip_prefix("pkg:")
            .ok_or_else(|| PurlError::NoScheme(s.to_string()))?
            .trim_start_matches('/');

        // Peel off subpath (#) and qualifiers (?), in that order.
        let (rest, subpath) = split1(rest, '#');
        let (rest, qstr) = split1(rest, '?');

        // type / namespace… / name@version
        let (ty, after) = rest.split_once('/').ok_or(PurlError::EmptyName)?;
        let mut segs: Vec<&str> = after.split('/').filter(|s| !s.is_empty()).collect();
        let last = segs.pop().ok_or(PurlError::EmptyName)?;
        let (name_raw, version_raw) = match last.split_once('@') {
            Some((n, v)) => (n, Some(v)),
            None => (last, None),
        };

        let namespace = if segs.is_empty() {
            None
        } else {
            let decoded: Result<Vec<String>, _> = segs.iter().map(|s| decode(s)).collect();
            Some(decoded?.join("/"))
        };
        let name = decode(name_raw)?;
        let version = version_raw.map(decode).transpose()?;

        let mut quals = BTreeMap::new();
        if let Some(qstr) = qstr {
            for pair in qstr.split('&').filter(|p| !p.is_empty()) {
                let (k, v) = split1(pair, '=');
                let v = decode(v.unwrap_or(""))?;
                quals.insert(k.to_string(), v);
            }
        }
        let subpath = subpath.map(decode).transpose()?;

        Purl::from_components(
            ty,
            namespace.as_deref(),
            &name,
            version.as_deref(),
            quals,
            subpath.as_deref(),
        )
    }
}

impl fmt::Display for Purl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "pkg:{}/", self.ty)?;
        if let Some(ns) = &self.namespace {
            for seg in ns.split('/') {
                write!(f, "{}/", encode(seg))?;
            }
        }
        f.write_str(&encode(&self.name))?;
        if let Some(v) = &self.version {
            write!(f, "@{}", encode(v))?;
        }
        for (i, (k, v)) in self.qualifiers.iter().enumerate() {
            f.write_str(if i == 0 { "?" } else { "&" })?;
            write!(f, "{k}={}", encode(v))?;
        }
        if let Some(sp) = &self.subpath {
            f.write_str("#")?;
            for (i, seg) in sp.split('/').enumerate() {
                if i > 0 {
                    f.write_str("/")?;
                }
                f.write_str(&encode(seg))?;
            }
        }
        Ok(())
    }
}

/// Split on the first `sep`: `(before, Some(after))`, or `(whole, None)`.
fn split1(s: &str, sep: char) -> (&str, Option<&str>) {
    match s.split_once(sep) {
        Some((a, b)) => (a, Some(b)),
        None => (s, None),
    }
}

fn non_empty(s: &str) -> Option<&str> {
    (!s.is_empty()).then_some(s)
}

/// Normalise a namespace into its non-empty segments, lowercased when the type
/// is case-insensitive.
fn segments(ns: &str, lower: bool) -> Vec<String> {
    ns.split('/')
        .filter(|s| !s.is_empty())
        .map(|s| {
            if lower {
                s.to_ascii_lowercase()
            } else {
                s.to_string()
            }
        })
        .collect()
}

/// Normalise a subpath: drop empty and `.`/`..` segments (§ purl canonical).
fn subpath_segments(sp: &str) -> Vec<String> {
    sp.split('/')
        .filter(|s| !s.is_empty() && *s != "." && *s != "..")
        .map(str::to_string)
        .collect()
}

/// A valid purl type: starts with an ASCII letter, then letters/digits/`.+-`.
fn valid_type(ty: &str) -> bool {
    let mut chars = ty.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_alphabetic())
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '+' | '-'))
}

/// A valid qualifier key: non-empty, ASCII letters/digits/`.-_`.
fn valid_qualifier_key(k: &str) -> bool {
    !k.is_empty()
        && k.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
}

/// Types whose namespace and name are case-insensitive (lowercased in the
/// canonical form). M1's `github` and `deb` are here; `cargo` and `generic`
/// are case-sensitive, so they are not.
fn type_lowercases(ty: &str) -> bool {
    matches!(ty, "github" | "deb")
}

/// Percent-decode a component once, validating each `%HH` escape.
fn decode(s: &str) -> Result<String, PurlError> {
    let bad = || PurlError::BadPercent(s.to_string());
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '%' {
            let (h1, h2) = (chars.next().ok_or_else(bad)?, chars.next().ok_or_else(bad)?);
            let hex: String = [h1, h2].iter().collect();
            out.push(u8::from_str_radix(&hex, 16).map_err(|_| bad())? as char);
        } else {
            out.push(c);
        }
    }
    Ok(out)
}

/// Percent-encode a component value, keeping the unreserved set and `:`
/// (RFC 3986 unreserved plus the colon purl keeps literal in versions and
/// qualifier values), encoding everything else as uppercase `%HH`.
fn encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~' | b':') {
            out.push(b as char);
        } else {
            out.push('%');
            out.push_str(&format!("{b:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(s: &str) -> Purl {
        s.parse().unwrap()
    }

    #[test]
    fn native_id_derivation() {
        // github uses namespace/name; the others use the bare name.
        assert_eq!(
            parse("pkg:github/package-url/purl-spec@1").native_id(),
            "package-url/purl-spec"
        );
        assert_eq!(parse("pkg:deb/debian/curl@7.50.3-1").native_id(), "curl");
        assert_eq!(parse("pkg:cargo/rand@0.7.2").native_id(), "rand");
        assert_eq!(parse("pkg:generic/openssl@1.1.10g").native_id(), "openssl");
    }

    #[test]
    fn unversioned_clears_the_version() {
        let p = parse("pkg:deb/debian/curl@7.50.3-1?arch=amd64");
        let u = p.unversioned();
        assert_eq!(u.version(), None);
        assert_eq!(u.to_string(), "pkg:deb/debian/curl?arch=amd64");
        // Everything but the version is preserved.
        assert_eq!(u.name(), "curl");
        assert_eq!(
            u.qualifiers().get("arch").map(String::as_str),
            Some("amd64")
        );
    }

    #[test]
    fn accessors_report_components() {
        let p = parse("pkg:deb/debian/curl@7.50.3-1?arch=i386&distro=jessie");
        assert_eq!(p.ty(), "deb");
        assert_eq!(p.namespace(), Some("debian"));
        assert_eq!(p.name(), "curl");
        assert_eq!(p.version(), Some("7.50.3-1"));
        assert_eq!(p.subpath(), None);
        assert_eq!(p.qualifiers().len(), 2);
    }

    #[test]
    fn subpath_drops_dot_dotdot_and_empty_segments() {
        let p = parse("pkg:github/o/r@1#/a/./b/../c/");
        assert_eq!(p.subpath(), Some("a/b/c"));
    }

    #[test]
    fn unregistered_type_is_allowed() {
        // winget/scoop/snap/flatpak have no registered purl type (§5.6).
        let p = parse("pkg:winget/sharkdp.fd");
        assert_eq!(p.ty(), "winget");
        assert_eq!(p.native_id(), "sharkdp.fd");
    }

    #[test]
    fn github_is_case_folded_but_cargo_is_not() {
        assert_eq!(parse("pkg:github/A/B@1").to_string(), "pkg:github/a/b@1");
        // cargo is case-sensitive: casing is preserved.
        assert_eq!(parse("pkg:cargo/RaNd@1").name(), "RaNd");
    }

    #[test]
    fn invalid_purls_are_rejected() {
        assert!(matches!(
            "deb/x".parse::<Purl>(),
            Err(PurlError::NoScheme(_))
        ));
        assert!(matches!(
            "pkg:3bad/x".parse::<Purl>(),
            Err(PurlError::InvalidType(_))
        ));
        assert!(matches!(
            "pkg:deb/".parse::<Purl>(),
            Err(PurlError::EmptyName)
        ));
        assert!(matches!(
            "pkg:deb/d/c@1?in bad=1".parse::<Purl>(),
            Err(PurlError::InvalidQualifier(_))
        ));
        assert!(matches!(
            "pkg:deb/c%2".parse::<Purl>(),
            Err(PurlError::BadPercent(_))
        ));
    }

    #[test]
    fn from_components_rejects_empty_name_and_type() {
        assert!(matches!(
            Purl::from_components("deb", None, "", None, BTreeMap::new(), None),
            Err(PurlError::EmptyName)
        ));
        assert!(matches!(
            Purl::from_components("", None, "x", None, BTreeMap::new(), None),
            Err(PurlError::InvalidType(_))
        ));
    }
}
