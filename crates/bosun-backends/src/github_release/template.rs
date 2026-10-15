//! Asset-name templates: the `asset` qualifier (from the manifest's
//! `ext.github-release.asset`) is a minijinja template rendered against a
//! fixed set of precomputed variables, so one manifest line names the right
//! archive for each host.
//!
//! Variable names follow goreleaser's (lower-cased) where they overlap, so a
//! template can be copied from a project's own `.goreleaser.yaml`. Undefined
//! is strict: an unknown variable is an error naming it, never an empty string
//! or a literal. Only minijinja's built-in filters (`title`, `upper`, `lower`,
//! `replace`, …) are available.

use std::collections::BTreeMap;

use bosun_core::{Arch, Os};
use bosun_ports::PortError;
use minijinja::{Environment, UndefinedBehavior};

use crate::errmsg;

/// goreleaser's default archive name, used when no `asset` is given.
pub(super) const DEFAULT_TEMPLATE: &str = "{{ binary }}_{{ version }}_{{ os }}_{{ arch }}.tar.gz";

/// Variables documented but not yet available: each needs host libc detection.
const NOT_YET: &[&str] = &["libc", "rust_target"];

/// What a template is rendered against: the package and the host it installs to.
pub(super) struct AssetVars<'a> {
    /// The binary name (the `binary` qualifier, else the package name).
    pub binary: &'a str,
    /// The resolved version, without a leading `v`.
    pub version: &'a str,
    /// The release's actual tag (`v1.2.3`, `1.2.3`, …).
    pub tag: &'a str,
    /// The target OS.
    pub os: Os,
    /// The target architecture.
    pub arch: Arch,
}

impl AssetVars<'_> {
    /// The precomputed variables, by name.
    fn context(&self) -> BTreeMap<&'static str, String> {
        let arch = self.arch.to_string();
        BTreeMap::from([
            ("binary", self.binary.to_string()),
            ("version", self.version.to_string()),
            ("tag", self.tag.to_string()),
            // bosun's `Os` display is already Go's GOOS (`darwin` for macOS).
            ("os", self.os.to_string()),
            ("arch", arch.clone()),
            ("arch_uname", arch_uname(self.arch).to_string()),
            ("arch_goreleaser", arch_goreleaser(self.arch, &arch)),
        ])
    }
}

/// The `uname -m` spelling, also used by Rust target triples, rpm and apk.
fn arch_uname(arch: Arch) -> &'static str {
    match arch {
        Arch::Amd64 => "x86_64",
        Arch::Arm64 => "aarch64",
    }
}

/// The `goreleaser init` default spelling: `x86_64` for amd64, otherwise the
/// Go arch (so `arm64`, not `aarch64`).
fn arch_goreleaser(arch: Arch, go_arch: &str) -> String {
    match arch {
        Arch::Amd64 => "x86_64".to_string(),
        Arch::Arm64 => go_arch.to_string(),
    }
}

fn template_error(template: &str, detail: impl std::fmt::Display) -> PortError {
    PortError::Backend {
        detail: format!("{} {template:?}: {detail}", errmsg::ASSET_TEMPLATE),
    }
}

/// Render `template` against `vars`. A syntax error, an unknown variable, or a
/// variable that is not supported yet is an error naming the template.
pub(super) fn render(template: &str, vars: &AssetVars<'_>) -> Result<String, PortError> {
    let mut env = Environment::new();
    env.set_undefined_behavior(UndefinedBehavior::Strict);
    let tmpl = env
        .template_from_str(template)
        .map_err(|e| template_error(template, e))?;
    let context = vars.context();

    // Name the offending variable up front: strict undefined alone reports
    // only "undefined value", and a not-yet variable deserves its own reason.
    let mut used: Vec<String> = tmpl.undeclared_variables(false).into_iter().collect();
    used.sort();
    if let Some(var) = used.iter().find(|v| NOT_YET.contains(&v.as_str())) {
        return Err(template_error(
            template,
            format!(
                "{} `{var}`: {}",
                errmsg::ASSET_NOT_YET,
                errmsg::ASSET_NOT_YET_WHY
            ),
        ));
    }
    if let Some(var) = used.iter().find(|v| !context.contains_key(v.as_str())) {
        let known: Vec<&str> = context.keys().copied().collect();
        return Err(template_error(
            template,
            format!(
                "{} `{var}` (available: {})",
                errmsg::ASSET_UNKNOWN_VARIABLE,
                known.join(", ")
            ),
        ));
    }

    let name = tmpl
        .render(&context)
        .map_err(|e| template_error(template, e))?;
    if name.contains(['{', '}']) {
        return Err(template_error(template, errmsg::ASSET_BRACE));
    }
    Ok(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    const AMD64: Arch = Arch::Amd64;
    const ARM64: Arch = Arch::Arm64;

    fn vars<'a>(binary: &'a str, version: &'a str, tag: &'a str, arch: Arch) -> AssetVars<'a> {
        AssetVars {
            binary,
            version,
            tag,
            os: Os::Linux,
            arch,
        }
    }

    /// Render `template` for linux/amd64 and linux/arm64.
    fn both(template: &str, binary: &str, version: &str, tag: &str) -> (String, String) {
        (
            render(template, &vars(binary, version, tag, AMD64)).unwrap(),
            render(template, &vars(binary, version, tag, ARM64)).unwrap(),
        )
    }

    fn pair(amd64: &str, arm64: &str) -> (String, String) {
        (amd64.to_string(), arm64.to_string())
    }

    fn err_text(template: &str) -> String {
        match render(template, &vars("x", "1.0.0", "v1.0.0", AMD64)).unwrap_err() {
            PortError::Backend { detail } => detail,
            other => panic!("unexpected error {other:?}"),
        }
    }

    // --- goreleaser default ---------------------------------------------------

    #[test]
    fn goreleaser_default_is_the_default_template() {
        assert_eq!(
            both(DEFAULT_TEMPLATE, "ctxloom", "0.6.4", "v0.6.4"),
            pair(
                "ctxloom_0.6.4_linux_amd64.tar.gz",
                "ctxloom_0.6.4_linux_arm64.tar.gz"
            )
        );
        assert_eq!(
            both(DEFAULT_TEMPLATE, "stern", "1.33.0", "v1.33.0"),
            pair(
                "stern_1.33.0_linux_amd64.tar.gz",
                "stern_1.33.0_linux_arm64.tar.gz"
            )
        );
    }

    #[test]
    fn goreleaser_default_with_the_tag_keeps_its_v() {
        // traefik writes the tag, `v` included, where the version goes.
        assert_eq!(
            both(
                "{{ binary }}_{{ tag }}_{{ os }}_{{ arch }}.tar.gz",
                "traefik",
                "3.7.14",
                "v3.7.14"
            ),
            pair(
                "traefik_v3.7.14_linux_amd64.tar.gz",
                "traefik_v3.7.14_linux_arm64.tar.gz"
            )
        );
    }

    // --- goreleaser init default ---------------------------------------------

    #[test]
    fn goreleaser_init_title_os_and_x86_64() {
        assert_eq!(
            both(
                "{{ binary }}_{{ version }}_{{ os | title }}_{{ arch_goreleaser }}.tar.gz",
                "gum",
                "2.0.2",
                "v2.0.2"
            ),
            pair(
                "gum_2.0.2_Linux_x86_64.tar.gz",
                "gum_2.0.2_Linux_arm64.tar.gz"
            )
        );
    }

    #[test]
    fn goreleaser_init_lower_case_os() {
        // duf and lazygit lower-case the OS but keep the init arch spelling.
        assert_eq!(
            both(
                "{{ binary }}_{{ version }}_{{ os }}_{{ arch_goreleaser }}.tar.gz",
                "duf",
                "0.8.1",
                "v0.8.1"
            ),
            pair(
                "duf_0.8.1_linux_x86_64.tar.gz",
                "duf_0.8.1_linux_arm64.tar.gz"
            )
        );
    }

    #[test]
    fn goreleaser_init_port_with_an_if_expression() {
        // The survey's minijinja port of goreleaser's own init template.
        let t = "{{ binary }}_{{ os | title }}_{% if arch == \"amd64\" %}x86_64{% else %}{{ arch }}{% endif %}.tar.gz";
        assert_eq!(
            both(t, "k9s", "0.50.0", "v0.50.0"),
            pair("k9s_Linux_x86_64.tar.gz", "k9s_Linux_arm64.tar.gz")
        );
    }

    // --- Rust target triple ---------------------------------------------------

    #[test]
    fn rust_triple_with_version() {
        // The binary is `rg`; the asset is named after the repo, so it's literal.
        assert_eq!(
            both(
                "ripgrep-{{ version }}-{{ arch_uname }}-unknown-linux-musl.tar.gz",
                "rg",
                "15.2.0",
                "15.2.0"
            ),
            pair(
                "ripgrep-15.2.0-x86_64-unknown-linux-musl.tar.gz",
                "ripgrep-15.2.0-aarch64-unknown-linux-musl.tar.gz"
            )
        );
    }

    #[test]
    fn rust_triple_with_a_v_tag() {
        assert_eq!(
            both(
                "{{ binary }}-{{ tag }}-{{ arch_uname }}-unknown-linux-musl.tar.gz",
                "fd",
                "10.3.0",
                "v10.3.0"
            ),
            pair(
                "fd-v10.3.0-x86_64-unknown-linux-musl.tar.gz",
                "fd-v10.3.0-aarch64-unknown-linux-musl.tar.gz"
            )
        );
    }

    #[test]
    fn rust_triple_without_a_version() {
        assert_eq!(
            both(
                "{{ binary }}-{{ arch_uname }}-unknown-linux-gnu.tar.gz",
                "uv",
                "0.9.2",
                "0.9.2"
            ),
            pair(
                "uv-x86_64-unknown-linux-gnu.tar.gz",
                "uv-aarch64-unknown-linux-gnu.tar.gz"
            )
        );
        // eza and bottom use `_` and no version.
        assert_eq!(
            both(
                "{{ binary }}_{{ arch_uname }}-unknown-linux-musl.tar.gz",
                "eza",
                "0.23.4",
                "v0.23.4"
            ),
            pair(
                "eza_x86_64-unknown-linux-musl.tar.gz",
                "eza_aarch64-unknown-linux-musl.tar.gz"
            )
        );
    }

    // --- dash-separated Go ----------------------------------------------------

    #[test]
    fn dash_separated_go() {
        assert_eq!(
            both(
                "{{ binary }}-{{ version }}-{{ os }}-{{ arch }}.tar.gz",
                "golangci-lint",
                "2.5.0",
                "v2.5.0"
            ),
            pair(
                "golangci-lint-2.5.0-linux-amd64.tar.gz",
                "golangci-lint-2.5.0-linux-arm64.tar.gz"
            )
        );
        // hugo mixes separators.
        assert_eq!(
            both(
                "{{ binary }}_{{ version }}_{{ os }}-{{ arch }}.tar.gz",
                "hugo",
                "0.151.0",
                "v0.151.0"
            ),
            pair(
                "hugo_0.151.0_linux-amd64.tar.gz",
                "hugo_0.151.0_linux-arm64.tar.gz"
            )
        );
        // syncthing puts the tag last.
        assert_eq!(
            both(
                "{{ binary }}-{{ os }}-{{ arch }}-{{ tag }}.tar.gz",
                "syncthing",
                "2.0.10",
                "v2.0.10"
            ),
            pair(
                "syncthing-linux-amd64-v2.0.10.tar.gz",
                "syncthing-linux-arm64-v2.0.10.tar.gz"
            )
        );
    }

    // --- raw binary, no archive -----------------------------------------------

    #[test]
    fn raw_binary_names() {
        assert_eq!(
            both(
                "{{ binary }}-{{ os }}-{{ arch }}",
                "kind",
                "0.30.0",
                "v0.30.0"
            ),
            pair("kind-linux-amd64", "kind-linux-arm64")
        );
        assert_eq!(
            both(
                "{{ binary }}_{{ tag }}_{{ os }}_{{ arch }}",
                "shfmt",
                "3.12.0",
                "v3.12.0"
            ),
            pair("shfmt_v3.12.0_linux_amd64", "shfmt_v3.12.0_linux_arm64")
        );
        assert_eq!(
            both(
                "docker-compose-{{ os }}-{{ arch_uname }}",
                "docker-compose",
                "2.40.0",
                "v2.40.0"
            ),
            pair(
                "docker-compose-linux-x86_64",
                "docker-compose-linux-aarch64"
            )
        );
    }

    // --- one-offs -------------------------------------------------------------

    #[test]
    fn trivy_spells_its_arch_itself() {
        let t = "{{ binary }}_{{ version }}_{{ os | title }}-{% if arch == \"amd64\" %}64bit{% else %}{{ arch | upper }}{% endif %}.tar.gz";
        assert_eq!(
            both(t, "trivy", "0.67.2", "v0.67.2"),
            pair(
                "trivy_0.67.2_Linux-64bit.tar.gz",
                "trivy_0.67.2_Linux-ARM64.tar.gz"
            )
        );
    }

    #[test]
    fn bun_and_nvim_and_shellcheck() {
        let bun = "{{ binary }}-{{ os }}-{{ arch_uname | replace(\"x86_64\", \"x64\") }}.zip";
        assert_eq!(
            both(bun, "bun", "1.4.3", "bun-v1.4.3"),
            pair("bun-linux-x64.zip", "bun-linux-aarch64.zip")
        );
        assert_eq!(
            both(
                "{{ binary }}-{{ os }}-{{ arch_goreleaser }}.tar.gz",
                "nvim",
                "0.11.4",
                "v0.11.4"
            ),
            pair("nvim-linux-x86_64.tar.gz", "nvim-linux-arm64.tar.gz")
        );
        assert_eq!(
            both(
                "{{ binary }}-{{ tag }}.{{ os }}.{{ arch_uname }}.tar.xz",
                "shellcheck",
                "0.11.0",
                "v0.11.0"
            ),
            pair(
                "shellcheck-v0.11.0.linux.x86_64.tar.xz",
                "shellcheck-v0.11.0.linux.aarch64.tar.xz"
            )
        );
    }

    // --- other hosts ----------------------------------------------------------

    #[test]
    fn mac_and_windows_use_go_names() {
        let t = "{{ os }}/{{ os | title }}";
        let on = |os| {
            render(
                t,
                &AssetVars {
                    binary: "x",
                    version: "1.0.0",
                    tag: "v1.0.0",
                    os,
                    arch: AMD64,
                },
            )
            .unwrap()
        };
        assert_eq!(on(Os::Macos), "darwin/Darwin");
        assert_eq!(on(Os::Windows), "windows/Windows");
        assert_eq!(on(Os::Linux), "linux/Linux");
    }

    // --- errors ---------------------------------------------------------------

    #[test]
    fn an_unknown_variable_is_an_error_naming_it() {
        let e = err_text("{{ binary }}_{{ platform }}.tar.gz");
        assert!(e.contains(errmsg::ASSET_TEMPLATE), "{e}");
        assert!(e.contains(errmsg::ASSET_UNKNOWN_VARIABLE), "{e}");
        assert!(e.contains("platform"), "{e}");
        // The message lists what is available.
        assert!(e.contains("arch_uname"), "{e}");
    }

    #[test]
    fn an_unknown_variable_is_an_error_even_inside_a_condition() {
        let e = err_text("{% if nope %}a{% endif %}x.tar.gz");
        assert!(e.contains("nope"), "{e}");
    }

    #[test]
    fn a_single_brace_placeholder_is_not_passed_through() {
        // A single `{` is literal text to minijinja, so `{binary}` would render
        // as itself; a rendered name holding a brace is refused instead.
        let e = err_text("{binary}_{{ version }}.tar.gz");
        assert!(e.contains(errmsg::ASSET_BRACE), "{e}");
        assert!(e.contains("{{ binary }}"), "{e}");
    }

    #[test]
    fn variables_are_case_sensitive() {
        let e = err_text("{{ Os }}.tar.gz");
        assert!(e.contains(errmsg::ASSET_UNKNOWN_VARIABLE), "{e}");
        assert!(e.contains("Os"), "{e}");
    }

    #[test]
    fn libc_is_not_supported_yet() {
        for var in ["libc", "rust_target"] {
            let e = err_text(&format!("{{{{ binary }}}}-{{{{ {var} }}}}.tar.gz"));
            assert!(e.contains(errmsg::ASSET_NOT_YET), "{e}");
            assert!(e.contains(var), "{e}");
            assert!(!e.contains("undefined"), "{e}");
        }
    }

    #[test]
    fn a_syntax_error_is_an_error() {
        let e = err_text("{{ binary _{{ version }}.tar.gz");
        assert!(e.contains(errmsg::ASSET_TEMPLATE), "{e}");
    }

    #[test]
    fn attribute_access_on_a_variable_is_strict() {
        let e = err_text("{{ binary.nope }}.tar.gz");
        assert!(e.contains(errmsg::ASSET_TEMPLATE), "{e}");
    }
}
