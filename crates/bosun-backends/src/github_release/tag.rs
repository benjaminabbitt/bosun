//! Release tags that aren't `v<version>` or `<version>`: the `tag` qualifier
//! (the manifest's `backends.github-release.tag`) is the tag with `{{ version }}`
//! where the version goes, such as `jq-{{ version }}`, `bun-v{{ version }}` or
//! `kustomize/v{{ version }}`. It is read both ways: a version names its tag,
//! and a tag that fits the pattern gives its version, so a repository that
//! releases several tools (kustomize's `api/…`, `kyaml/…`) lists only one.

use crate::errmsg;

/// A tag pattern: the literal text before and after `{{ version }}`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct TagPattern {
    prefix: String,
    suffix: String,
}

impl TagPattern {
    /// Read a tag template. It must hold `{{ version }}` (inner spaces
    /// optional) exactly once, and no other `{` or `}`: the rest is literal,
    /// so the pattern can be read back from a tag.
    pub(super) fn parse(template: &str) -> Result<TagPattern, String> {
        let bad = |why: &str| format!("{} {template:?}: {why}", errmsg::TAG_TEMPLATE);
        let open = template
            .find("{{")
            .ok_or_else(|| bad("it has no {{ version }}"))?;
        let close = template[open..]
            .find("}}")
            .map(|i| open + i)
            .ok_or_else(|| bad("an unclosed {{"))?;
        if template[open + 2..close].trim() != "version" {
            return Err(bad("the only placeholder is {{ version }}"));
        }
        let (prefix, suffix) = (&template[..open], &template[close + 2..]);
        if prefix.contains(['{', '}']) || suffix.contains(['{', '}']) {
            return Err(bad(
                "{{ version }} goes in once, and nothing else is a placeholder",
            ));
        }
        Ok(TagPattern {
            prefix: prefix.to_string(),
            suffix: suffix.to_string(),
        })
    }

    /// The tag for `version`.
    pub(super) fn tag_for(&self, version: &str) -> String {
        format!("{}{version}{}", self.prefix, self.suffix)
    }

    /// The version in `tag`, when the tag fits the pattern.
    pub(super) fn version_in<'t>(&self, tag: &'t str) -> Option<&'t str> {
        tag.strip_prefix(self.prefix.as_str())?
            .strip_suffix(self.suffix.as_str())
            .filter(|v| !v.is_empty())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pattern(t: &str) -> TagPattern {
        TagPattern::parse(t).unwrap()
    }

    #[test]
    fn a_prefixed_tag_reads_both_ways() {
        let jq = pattern("jq-{{ version }}");
        assert_eq!(jq.tag_for("1.8.2"), "jq-1.8.2");
        assert_eq!(jq.version_in("jq-1.8.2"), Some("1.8.2"));
        let bun = pattern("bun-v{{version}}");
        assert_eq!(bun.tag_for("1.4.3"), "bun-v1.4.3");
        assert_eq!(bun.version_in("bun-v1.4.3"), Some("1.4.3"));
    }

    #[test]
    fn only_tags_that_fit_give_a_version() {
        let kustomize = pattern("kustomize/v{{ version }}");
        assert_eq!(kustomize.version_in("kustomize/v5.8.3"), Some("5.8.3"));
        assert_eq!(kustomize.version_in("kyaml/v0.21.3"), None);
        assert_eq!(kustomize.version_in("api/v0.21.3"), None);
        assert_eq!(kustomize.version_in("kustomize/v"), None);
    }

    #[test]
    fn a_suffix_is_literal_too() {
        let p = pattern("v{{ version }}-release");
        assert_eq!(p.tag_for("2.0.0"), "v2.0.0-release");
        assert_eq!(p.version_in("v2.0.0-release"), Some("2.0.0"));
        assert_eq!(p.version_in("v2.0.0"), None);
    }

    #[test]
    fn a_template_without_exactly_one_version_is_an_error() {
        for bad in [
            "jq-1.8.2",
            "{{ tag }}",
            "{{ version }}-{{ version }}",
            "jq-{{ version",
            "{{ binary }}-{{ version }}",
            "x}{{ version }}",
        ] {
            let err = TagPattern::parse(bad).unwrap_err();
            assert!(err.contains(errmsg::TAG_TEMPLATE), "{bad}: {err}");
            assert!(err.contains(bad), "{bad}: {err}");
        }
    }
}
