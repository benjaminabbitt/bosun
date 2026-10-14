# github-release fixtures

Real data from the public `ctxloom/ctxloom` GitHub repo, used by the
github-release backend's tests (no network in tests).

- `ctxloom-releases-page1.json` — trimmed `GET /repos/ctxloom/ctxloom/releases`
  response. Only `ctxloom` ships (no ltk/taskloom/harp assets); no pre-releases.
- `ctxloom-v0.6.4-checksums.txt` — the v0.6.4 `checksums.txt` asset.
- `ctxloom-v0.6.4-linux-amd64.tar.listing` — the member list of
  `ctxloom_0.6.4_linux_amd64.tar.gz` (the binary `ctxloom` sits at the root).

Each release carries both `<name>_<ver>_<os>_<arch>.tar.gz` and a `_full`
variant, plus Windows `.zip`, `checksums.txt`, `install.sh`, `install.ps1`.
The picker matches the goreleaser default name exactly, so it selects the plain
archive; the `_full` variant is reached only via an explicit `asset` override.
