# github-release asset templates

A `github-release` package installs one archive from a GitHub release. bosun finds that
archive by **exact name**, so it never matches by prefix and a `_full` variant can't be
mistaken for the plain archive. The name comes from a [minijinja](https://docs.rs/minijinja)
template: `{{ variable }}` placeholders are replaced with values for the package being
installed and the host it's going to.

## Default

With no `asset` setting, the template is goreleaser's default archive name:

```text
{{ binary }}_{{ version }}_{{ os }}_{{ arch }}.tar.gz
```

For ctxloom 0.6.4 on linux/amd64 that gives `ctxloom_0.6.4_linux_amd64.tar.gz`.

## Overriding the template

Set the `asset` option under the block's `github-release` backend when a release uses a
different layout:

```yaml
packages:
  - name: gum
    spec: ">=2.0.0|<3.0.0"
    install:
      linux:
        backends:
          github-release:
            package: charmbracelet/gum
            asset: "{{ binary }}_{{ version }}_{{ os | title }}_{{ arch_goreleaser }}.tar.gz"
```

The template must name the asset exactly as it appears on the release page, extension
included. Quote it in YAML: a value that starts with `{` would otherwise be read as a mapping.

`backends` maps each backend that may install the package to its options. `package` is the
one option every backend takes: that backend's own package id. The others are the backend's
own, and an option it doesn't declare is an error that lists the ones it does (`unknown key
"aset" in backends.github-release (allowed: package, asset, tag)`), as is a backend bosun
doesn't have. With one backend and no options but `package`, the shorthand
`{backend: github-release, package: owner/repo}` says the same thing.
`defaults.install` can set options too; an entry that names the same backend inherits them
key by key, and a key the entry sets wins.

## Variables

| Variable          | Value                                                              | linux/amd64 | linux/arm64 |
|-------------------|--------------------------------------------------------------------|-------------|-------------|
| `binary`          | The binary name: the entry's `name` (the `binary` purl qualifier)   | `gum`       | `gum`       |
| `version`         | The resolved version, without a leading `v`                        | `2.0.2`     | `2.0.2`     |
| `tag`             | The release's actual tag                                           | `v2.0.2`    | `v2.0.2`    |
| `os`              | Go's `GOOS`                                                        | `linux`     | `linux`     |
| `arch`            | Go's `GOARCH` (also Debian's spelling for these two)               | `amd64`     | `arm64`     |
| `arch_uname`      | The `uname -m` spelling; also Rust triples, rpm and apk            | `x86_64`    | `aarch64`   |
| `arch_goreleaser` | The `goreleaser init` spelling: `x86_64` for amd64, otherwise `arch` | `x86_64`  | `arm64`     |

All OS values:

| bosun OS | `os`      | `os \| title` |
|----------|-----------|---------------|
| linux    | `linux`   | `Linux`       |
| macos    | `darwin`  | `Darwin`      |
| windows  | `windows` | `Windows`     |

Names follow goreleaser's template fields (lower-cased) where they overlap, so a template can
often be copied from the project's own `.goreleaser.yaml`.

`arch_uname` and `arch_goreleaser` are both needed because release authors mix spellings.
gum, lazygit and duf publish `x86_64` next to `arm64` (that's `arch_goreleaser`). Projects that
follow `uname -m` or Rust target triples use `aarch64` (that's `arch_uname`).

minijinja's built-in filters change case or spelling: `{{ os | title }}` gives `Linux`,
`{{ arch | upper }}` gives `ARM64`, `{{ arch_uname | replace("x86_64", "x64") }}` gives `x64`.
`{% if %}` works for one-off spellings.

Not available yet:

| Variable      | Would be                                              | linux/amd64                |
|---------------|-------------------------------------------------------|----------------------------|
| `libc`        | The host's C library family (`gnu` or `musl`)          | `gnu`                      |
| `rust_target` | `{{ arch_uname }}-unknown-{{ os }}-{{ libc }}`         | `x86_64-unknown-linux-gnu` |

Both need host libc detection, which bosun doesn't have yet. A template that uses either is
an error that says so. Many Rust projects publish only a musl build for some architectures,
so writing `musl` into the template is usually right anyway.

## Examples from real releases

| Project       | Release asset (linux/amd64)                          | Template |
|---------------|------------------------------------------------------|----------|
| ctxloom       | `ctxloom_0.6.4_linux_amd64.tar.gz`                   | default |
| ctxloom       | `ctxloom_0.6.4_linux_amd64_full.tar.gz`              | `{{ binary }}_{{ version }}_{{ os }}_{{ arch }}_full.tar.gz` |
| traefik       | `traefik_v3.7.14_linux_amd64.tar.gz`                 | `{{ binary }}_{{ tag }}_{{ os }}_{{ arch }}.tar.gz` |
| duf           | `duf_0.8.1_linux_x86_64.tar.gz`                      | `{{ binary }}_{{ version }}_{{ os }}_{{ arch_goreleaser }}.tar.gz` |
| gum           | `gum_2.0.2_Linux_x86_64.tar.gz`                      | `{{ binary }}_{{ version }}_{{ os \| title }}_{{ arch_goreleaser }}.tar.gz` |
| ripgrep       | `ripgrep-15.2.0-x86_64-unknown-linux-musl.tar.gz`    | `ripgrep-{{ version }}-{{ arch_uname }}-unknown-linux-musl.tar.gz` |
| fd            | `fd-v10.3.0-x86_64-unknown-linux-musl.tar.gz`        | `{{ binary }}-{{ tag }}-{{ arch_uname }}-unknown-linux-musl.tar.gz` |
| uv            | `uv-x86_64-unknown-linux-gnu.tar.gz`                 | `{{ binary }}-{{ arch_uname }}-unknown-linux-gnu.tar.gz` |
| golangci-lint | `golangci-lint-2.5.0-linux-amd64.tar.gz`             | `{{ binary }}-{{ version }}-{{ os }}-{{ arch }}.tar.gz` |
| trivy         | `trivy_0.67.2_Linux-64bit.tar.gz`                    | `{{ binary }}_{{ version }}_{{ os \| title }}-{% if arch == "amd64" %}64bit{% else %}{{ arch \| upper }}{% endif %}.tar.gz` |

ripgrep's binary is `rg`, so the template spells the archive's `ripgrep` literally.
## Windows

On Windows the binary is installed as `<binary>.exe` in `%USERPROFILE%\.local\bin` (bosun
warns when that isn't on `PATH`). `{{ binary }}` in a template stays the bare name, as
release assets spell it: `{{ binary }}_{{ version }}_{{ os }}_{{ arch }}.zip` finds
`tool_1.2.0_windows_amd64.zip`, and bosun looks for `tool.exe` inside it. A release that
ships the `.exe` itself is installed as it is.

## Windows installers: github-installer

`github-installer` runs the `.msi` or `.exe` installer a release attaches, machine-wide (from
an Administrator terminal). It needs `asset` and `display_name`, the program's name in
Windows' installed-programs list (end it with `*` to match a name that includes its
version). An `.exe` also needs `silent_args`, since installers disagree on them (NSIS `/S`,
Inno Setup `/VERYSILENT /SUPPRESSMSGBOXES`); an uninstaller without a quiet form takes
`uninstall_args`.

```yaml
- name: app
  spec: ">=2.0.0|<3.0.0"
  install:
    windows:
      backends:
        github-installer:
          package: owner/app
          asset: "{{ binary }}-setup-{{ version }}.exe"
          display_name: "App*"
          silent_args: "/VERYSILENT /SUPPRESSMSGBOXES"
```

## OS packages from a release: github-deb and github-rpm

`github-deb` and `github-rpm` install the `.deb` or `.rpm` a release attaches, through
`apt-get` or `dnf`, machine-wide (run with `sudo`). They find the release and the asset the
same way, with the same variables, but have no default name: `asset` is required. The
installed package's name is the entry's name unless `package_name` says otherwise:

```yaml
- name: bat
  spec: ">=0.26.0|<0.27.0"
  install:
    linux:debian:
      backends:
        github-deb:
          package: sharkdp/bat
          asset: "{{ binary }}_{{ version }}_{{ arch }}.deb"
    linux:fedora:
      backends:
        github-rpm:
          package: sharkdp/bat
          asset: "{{ binary }}-{{ version }}-1.{{ arch_uname }}.rpm"
          package_name: bat
```

apt holds the package (`apt-mark hold`); github-rpm doesn't hold, since dnf needs its
versionlock plugin for that, and the plan says so.

## Release tags

bosun looks a version's release up by its tag: `v<version>`, then `<version>`. A project that
tags differently sets `tag`, the tag with `{{ version }}` where the version goes:

```yaml
backends:
  github-release:
    tag: "jq-{{ version }}"            # jq-1.8.2
    asset: "jq-{{ os }}-{{ arch }}"
```

`bun-v{{ version }}` and `kustomize/v{{ version }}` work the same way. Only tags that fit the
pattern are versions, so in a repository that releases several tools (kustomize also tags
`kyaml/…` and `api/…`) bosun lists just the one named. The rest of the pattern is literal:
`{{ version }}` must appear exactly once, and nothing else may be a placeholder. In the asset
template, `{{ tag }}` is the whole tag (`kustomize/v5.8.3`); `{{ tag | replace('kustomize/', '') }}`
gives `v5.8.3`.

## Formats

bosun reads how the asset is packed from its name:

| Asset name ends in | What bosun does |
|---|---|
| `.tar.gz` `.tgz` `.tar.xz` `.txz` `.tar.zst` `.tzst` `.tar.bz2` `.tbz2` `.tbz` `.tar` `.zip` | finds the binary inside by name |
| `.gz` `.xz` `.zst` `.bz2` (not after `.tar`) | decompresses the file; that is the binary (restic) |
| anything else, including no suffix, `.exe` and `.appimage` | installs the file as the binary (kind, jq, cosign) |
| `.deb` `.rpm` `.apk` `.msi` `.dmg` `.pkg`, signatures, checksums, `.json`, `.txt`, `.sh` | refused when bosun resolves: not a binary bosun can install |

Upper and lower case are the same (`RCLONE.ZIP` is a zip).

## Rules

- Variables are case-sensitive: `{{ os }}` exists, `{{ Os }}` doesn't.
- An unknown variable such as `{{ platform }}` is an error that names it and lists the
  variables available. It is never passed through literally or rendered empty.
- A single-brace placeholder like `{binary}` is an error too. To minijinja it's literal text,
  so bosun refuses any rendered name that still contains `{` or `}`.
- Templates are checked when bosun resolves the package (`bosun lock`, `bosun install`), once
  it has found the release. A broken template stops resolution, and nothing is installed.
- Matching is exact. If no asset has the rendered name, the error lists the release's assets
  so you can correct the template.
- bosun finds the binary inside the archive by name, at any depth: at the root (duf), in a
  wrapping directory (`gum_2.0.2_Linux_x86_64/gum`), or under `bin/` (`gh_…/bin/gh`). Exactly
  one file in the archive may have that name. If there are none, or more than one, the error
  lists what it found.
- **Checksums:** when the release publishes a sha256 for the asset, bosun verifies the
  download against it and records it in the lock. A mismatch is an error. bosun looks for:
  - a per-asset file, `<asset>.sha256` or `<asset>.sha256sum`;
  - otherwise a combined sums file: `checksums.txt`, `<name>_checksums.txt`,
    `<name>-checksums.txt`, `SHA256SUMS`, `sha256sum.txt`, `SHASUMS256.txt`, `sha256.sum` or
    `checksums.sha256`.

  Signature files (`.asc`, `.sig`, `.minisig`, `.sigstore.json`, `.pem`) are never read as
  sums. If the release publishes no checksum, or its sums file doesn't list the asset, bosun
  installs anyway, prints a warning naming the repo and asset, and records no checksum.

## Open

- **Binary path override** for archives where locating by name doesn't work (two files with
  the binary's name, or a binary named differently from the package): a second template naming
  the path inside the archive, using the same variables.
