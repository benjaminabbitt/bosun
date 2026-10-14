# Conformance fixtures (vendored)

Official JSON conformance suites for the standards bosun adopts (§5.6). Vendored
verbatim as test fixtures; bosun's `vers` and `purl` code must pass them.

| dir | source repo | rev | license |
|---|---|---|---|
| `purl/` | https://github.com/package-url/purl-spec | `ebbeb6fd` | MIT |
| `vers/` | https://github.com/package-url/vers-spec | `7d0cc191` | MIT |

- `purl/specification-test.json` ← `tests/spec/specification-test.json`
- `purl/<type>-test.json` ← `tests/types/<type>-test.json` for the 12 types bosun
  registers: deb, cargo, github, generic, rpm, npm, pypi, gem, golang, apk, alpm, brew.
- `vers/*.json` ← the full `tests/` directory.

## What bosun exercises vs. skips

bosun maps vers types onto its §6 version schemes: `deb`→Debian;
`cargo`/`npm`/`golang`/`semver`→SemVer; everything else→generic chunk ordering.

**Exercised (M1):** `vers/vers_canonical_parse_test.json` (type-agnostic grammar),
`vers/npm_range_containment_test.json` (SemVer ordering). The purl suites are exercised
by the purl module (4b part 2).

**Skipped (reported), out of M1 scope — ecosystems with no bosun backend and version
grammars outside §6:** the vers `*_version_cmp` suites (alpine, alpm, conan, datetime,
gentoo, intdot, maven, nuget, openssl, lexicographic) and the `*_range_from_native`
suites (conan, gem, nginx, npm, nuget, openssl, pypi — native→vers conversion, which
bosun does not do: authors write vers directly). `pypi_*` needs PEP 440 ordering, which
bosun approximates with generic chunk ordering and does not claim to pass.
