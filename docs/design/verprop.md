# bosun — design (working name; formerly "verprop")

> Status: pre-implementation design. Captures the build-vs-adopt decision, architecture,
> and the capability/scope audit. Date: 2026-10-13.

## 1. What bosun is

A cross-platform CLI that maintains a **manifest of pinned versions** of applications/binaries
and **propagates/installs** them by delegating to **whatever package managers are available** on
the host — native OS managers (apt, dnf, pacman, zypper, apk, brew, scoop, winget) and
language-ecosystem installers (`go install`, `cargo install`, npm, uv/pipx, gem).

**Licence: EUPL-1.2**, the same as annals, whose `annals-yaml` reads and edits bosun's
manifest (§4.4).

## 2. Build vs adopt — decided to BUILD

No existing tool combines all three of:

1. a pinned-version manifest,
2. version-level propagation/reconcile (drive the host *to* the pinned versions), and
3. delegation to native OS package managers.

The field splits into two families that don't overlap:

- **Pinning tools** (mise, aqua, proto, asdf) — real lockfiles, but install only over language
  ecosystems / release binaries, never apt/dnf/brew/pacman.
- **Native-PM meta-managers** (metapac, nyw) — delegate to native managers, but track package
  *presence*, not *version* (no pinning).

**metapac — evaluated, DECIDED: don't extend; reference only.** metapac (Rust) is the closest
existing tool: it delegates to native PMs and reconciles. It is the wrong base for bosun because:

- **Its backend contract has no version.** The `Backend` trait installs, lists and uninstalls by
  package *name*; a version exists only as an optional per-backend option (cargo, mise).
  Pinning, ordering, policy, holds and downgrades would replace that contract, not add to it.
- **Its prune semantics are the inverse of bosun's.** `metapac clean` uninstalls everything
  installed but not listed; it keeps no record of what it installed. bosun prunes only what it
  installed (§9), from a machine state file metapac has no equivalent of (§4.6).
- **Upstream doesn't want it.** On metapac issue #217 ("Support for a lock file") the
  maintainer judged versioning at the backend-trait level not worth the complexity, since
  backends such as pacman can't support it.
- **License:** GPL-3.0-or-later. Its code can't be copied into bosun unless bosun is GPL too,
  so it serves as a reference for ideas and a record of known problems (Appendix B), not as a
  code source.

Its extra backends are tracked in [FUTURE.md](FUTURE.md).

A custom apt/brew backend for `mise` (which pins well, but only for language ecosystems) was
the last alternative considered; bosun was built greenfield instead.

## 3. Language: Rust

Chosen over Go on 2026-10-13. The core problem is *modeling heterogeneity correctly*: a
per-backend interface with ~8–10 implementations, per-backend version grammars that don't
normalize, and a taxonomy of failure states. That maps directly onto Rust enums + `trait`
backends + `Result`, compiler-enforced exhaustively. It mutates system package state
(correctness-sensitive), the CLI/serde/version-compare crate ecosystem is excellent, and
the closest prior art (`metapac`, §2) is Rust. The cost vs Go — more cross-compilation friction —
becomes a *feature* here (see container-inject), so budget musl static builds early.

### 3.1 Dependencies and distribution

- **`annals-yaml` and `annals-diagnostic`** (from annals, EUPL-1.2, the author's own project)
  read and edit the manifest and carry bosun's error codes. annals is shared by git only.
- **Not on crates.io:** crates.io refuses packages with git dependencies, so bosun ships as
  release binaries: GitHub release archives (static musl on Linux) and a Homebrew tap. These
  are the same channels bosun's `github-release` and brew backends install the ctxloom family
  from (§12), so bosun can install itself.

## 4. Core architecture

```
manifest  ──►  resolver  ──►  lock  ──►  drivers
(vers spec    (ordering +    (resolved   (host-apply | container-inject | …)
 + purl per    the package    versions,
 entry)        manager's      per target)
               choice in set)
```

- **manifest** — declarative; per-entry vers spec and per-backend install selectors (§5.6).
- **resolver** — asks each backend's package manager for its choice within each spec's set
  (apt's and dnf's priorities, npm's and uv's own resolution, else the highest available
  version, §5.6).
- **lock** — the resolved versions, the single source of truth both drivers
  consume. Deterministic/sorted output (good for diffing and Docker layer caching).
- **drivers** — apply the lock to a target. "Apply to host" and "inject into a container build"
  are *peers* over the same lock, not special cases.

### 4.1 Workspace layout (ports and adapters)

A Cargo workspace, so the boundaries are compiler-enforced (and give the arch-lint something
real to check later):

| crate | role |
|---|---|
| `bosun-core` | domain: `Version`, `Segment` + `Ord`, `Spec` (vers), `Purl`, `Entry`, `Resolution`. Pure, no I/O. |
| `bosun-ports` | traits: `Index`, `Installer`, `Driver`, `StateStore` |
| `bosun-backends` | adapters: apt, dnf, pacman, zypper, apk, snap, flatpak, brew, scoop, winget, cargo, go, npm, uv, pipx, gem, github-release (§12) |
| `bosun-resolve` | manifest + `Index` queries → lock (ordering; the package manager's choice in the set) |
| `bosun-io` | manifest and user config (YAML via `annals-yaml`, comment-preserving edits) and lock/state (JSON) |
| `bosun-apply` | drivers: host applier, container renderer |
| `bosun-host` | probe OS/arch/distro family; run backend probes; build the registry of present backends (§5.3) |
| `bosun-cli` | clap, output formatting, orchestration |

### 4.2 Commands

| command | does |
|---|---|
| `add <name> --backend … [--package] [--spec] [--on <selector>] [--opt k=v]` / `remove <name> [--plan] [--yes]` | edit one manifest entry, keeping every other byte, then run `lock` on the edited manifest. Both refuse a manifest older than the current version (run `migrate`). `add` writes under `any` when every backend is platform-independent, else this machine's OS, unless `--on` says; it checks the entry first (backend options, then a resolve on this machine) and writes nothing on error; without `--spec` it writes a default from the newest version: cargo and npm the caret range up to the next breaking one (`>=2.0.2\|<3.0.0`, below 1.0 the next minor), PyPI `>=X` as `uv add` writes, apt, dnf and apk `*`, following whatever the release's repositories carry (the distro's stable-update policy is the compatibility promise, and the selector already scopes the entry); it never installs. `remove` first releases the package (drops this manifest's claim, uninstalls only if bosun installed it and no other manifest claims it, behind the usual confirm), then edits and locks |
| `refresh` | explicit index refresh, per backend |
| `install [path] [--plan] [--scope system\|user] [--yes]` | plan → privilege check → confirm → apply: resolve each entry against this machine's lock (its pins that still fit, the newest in spec for the rest), install, record in the machine state, then write the lock (only after a successful apply; never with `--plan`) (§4.8, §5.5, §7). **Never uninstalls** (except the old name in a `formerly` migration, §5.4) |
| `uninstall [path] [--plan] [--scope system\|user] [--yes]` | the inverse of `install` for one manifest: drop all of its claims, then uninstall each package bosun installed that no other manifest still claims; same plan → confirm → apply flow (§9) |
| `prune [--plan] [--scope system\|user] [--yes]` | machine-wide cleanup: uninstall what bosun installed that no manifest claims any more, and drop claims whose manifest no longer exists (§9) |
| `upgrade [pkg…] [--manifest]` | let go of the named pins (all, when none are named), re-resolve those packages to the newest version their spec allows, write this machine's lock, and show old → new; installs nothing |
| `export [path] [--plan] [--yes]` | seed a manifest from what this machine has installed by hand, by each package manager's own record (§9): apt's manual marks less the distro's base system (priority required, important or standard), apk's world file less the Alpine base packages, cargo's crates.io installs (not path or git ones), and what bosun's state records it installed from GitHub releases (github-release, github-deb, github-rpm, github-installer), with the options its identity carries; a backend with no such record (winget, scoop, choco for now) gives nothing and is named. Each package becomes an entry as `add` writes it: the default spec for its installed version (`*` for an OS package), under `any` or this machine's OS. A name the manifest already has is left as written, as is a second backend's package of the same name; one whose range nothing available meets is left out. Creates the manifest when there is none; shows what it adds and confirms (§4.8), then writes and runs `lock`. Never installs |
| `status [path]` | with a manifest: each of its packages, installed against wanted, and every manifest that claims it (path, spec, user; a claim whose manifest is gone is marked, `prune` drops it). With no path: everything bosun manages on this machine, both scopes, with origin and claims; no manifest needed. Drift is `install --plan`'s job |
| `adopt <name>` | explicitly take ownership of an existing install: `accepted` → `installed` (§4.6) |
| `lock` | resolve the manifest for this machine (its platform, plus platform-independent `any` entries) and write this machine's lock for it, keeping each version it already pins while that still fits (§4.5, §7). Warns when an older bosun's `bosun.lock` sits beside the manifest: it is no longer read |
| `migrate [path] [--plan] [--yes]` | rewrite an older-version manifest as the current version (loading migrates in memory anyway; this writes it, so `add` / `remove` can edit it). annals edits only the entries the migration changes (`Loader::upgrade_keeping`), so comments elsewhere stay; when it can't edit in place it writes the whole file and says comments are not kept. It shows the result and confirms first (§4.8), and keeps the original as `<file>.v<N>` |
| `install --frozen` | install exactly what this machine's lock records; never re-resolves, never writes the lock; errors when there is no lock for this manifest, or the manifest changed since it was locked (§7) |
| `render args\|env [path] [--target <platform>]` / `render json [path] [--targets a,b]`, with `[--distro <id>@<release>] [--libc <family>]` | versions for a target platform as build args, env lines, or a JSON versions file (§8.3): this machine's from its lock, another machine's resolved through remote indexes (§4.7) |
| `backends` | registered backends, their versions, and why others were skipped (§5.3) |

### 4.3 Output format

Same convention as ctxloom: a global `--format` flag (`text | json | yaml`). The default is
human-readable `text` when stdout is a terminal and `json` when it is piped or redirected, so
scripts and CI get machine-readable output without asking for it. Every command's output is a
typed value serialized by one formatter, never hand-built strings, so `json` and `yaml` carry
the same fields.

### 4.4 Files

| file | format | written by | committed | purpose |
|---|---|---|---|---|
| `bosun.yaml` | YAML (annals-yaml subset) | people (and `add`/`remove`/`export`) | yes | the manifest: intent |
| lock | JSON | bosun only (`lock`, `upgrade`; `add`/`remove` run `lock`) | **no** (one per manifest, in the user state dir) | what this machine resolved for one manifest (§4.5) |
| machine state | JSON | bosun only | **no** (one per user, in `$HOME`) | what bosun installed for this user, and which manifests claim it |
| user config | YAML (annals-yaml subset) | people | no (`<config dir>/bosun/config.yaml`) | preferences, e.g. installer order |
| user manifest | YAML (annals-yaml subset) | people | no (`<config dir>/bosun/bosun.yaml`) | personal tools; one more claimant |

- **Config dir — DECIDED: XDG Base Directory, with the Windows convention on Windows.**
  Linux and macOS: `$XDG_CONFIG_HOME`, default `~/.config` (what git, gh and most developer
  CLIs use on macOS too, rather than `~/Library/Application Support`). Windows: `%APPDATA%`.
  User state follows the same rule (§4.6).
- **Manifest (YAML, read and written by `annals-yaml`):** a strict YAML subset. Anchors,
  aliases, tags, multi-line strings and multiple documents are refused with an error, never
  misread. `#` comments are allowed (for "why is this pinned").
  - **No type coercion:** every scalar is kept as its text, so `1.10` stays `1.10` and `no`
    stays `no`. Versions never need quoting and can't be read as numbers or booleans.
  - **Edits keep comments:** `add`, `remove` and `export` go through annals-yaml's
    comment-preserving editor, which splices only the entry it changes and leaves every
    other byte, comments included, as the person wrote it.
  - **Format versions:** every file carries a monotonic format version: `version` in the
    manifest (2) and user config (1), `lockfileVersion` and `stateVersion` in the JSON files.
    The YAML files load through `annals_yaml::versioned`: a file with no `version` is version
    0, and the registered steps migrate an older file in memory on every load. A newer version
    than the build reads is refused, never misread. Manifest 1 → 2 drops `targets` and folds
    the backend list, shared `package` and `ext` into the `backends` map; config 0 → 1 only
    stamps the version.
  - **Editor validation:** a published JSON Schema, referenced by a
    `# yaml-language-server: $schema=…` comment.
  - **Errors** are `annals-diagnostic` values: a stable code and named args, matching bosun's
    structured errors (§5.5).
- **Lockfile (JSON):** generated, never hand-edited, readable by `jq`. Pretty-printed, keys in
  fixed order, packages sorted by name, target keys sorted.
- **No bosun version is recorded anywhere** — `bosun --version` answers that. Only each file's
  *format* version is stored.

### 4.5 Lockfile format

> **DECIDED: the lock is the machine's own.** The manifest is about the packages and is
> shared; the user config holds this machine's preferences; the lock records what *this
> machine* resolved and is never shared or committed. `lock` and `upgrade` write it (`add` /
> `remove` run `lock`), and `install` writes it after a successful apply; `install --frozen`
> installs it verbatim. Earlier designs
> committed one `bosun.lock` covering every declared platform (format version 1); that file
> is no longer read, and `lock` warns when it finds one beside the manifest.

**Where:** one lock per manifest, in the user state directory beside the machine state
(§4.6): `$XDG_STATE_HOME/bosun/locks/<id>.json` (default `~/.local/state`),
`%LOCALAPPDATA%\bosun\locks\<id>.json` on Windows. `<id>` is the first 16 hex digits of the
sha256 of the manifest's canonical path, so every project gets its own lock without needing a
filesystem-safe name; the lock records the path too, and a lock whose `manifest` isn't this
one is treated as absent. Under `sudo` the user state directory is root's, so an elevated
`install --frozen` needs a lock written elevated (`sudo bosun lock`). Moving a project leaves
its old lock behind (harmless; a future `prune` can drop locks whose manifest is gone).

Each package is locked under this machine's platform, or under `any` for platform-independent
backends (cargo, go, npm, …). Because the backend and package ID can differ per platform
(§5.4), they live inside the target entry.

```json
{
  "lockfileVersion": 2,
  "manifest": "/home/me/proj/bosun.yaml",
  "manifestHash": "sha256:9f2c…",
  "packages": [
    {
      "name": "jq",
      "spec": "vers:deb/>=1.7.1|<1.7.2~",
      "targets": {
        "linux/amd64": {
          "backend": "apt",
          "purl": "pkg:deb/ubuntu/jq?arch=amd64&distro=noble",
          "resolved": "1.7.1-3ubuntu0.1",
          "indexSnapshot": "2026-10-13T19:00:00Z"
        }
      }
    },
    {
      "name": "ripgrep",
      "spec": "vers:cargo/>=14.1.1|<15.0.0",
      "targets": {
        "any": { "backend": "cargo", "purl": "pkg:cargo/ripgrep", "resolved": "14.1.1" }
      }
    }
  ]
}
```

| field | meaning |
|---|---|
| `lockfileVersion` | format version; bosun reads only the version it writes. A lock is regenerable, so another version is refused and `bosun lock` writes a fresh one; there is no migration |
| `manifest` | the canonical path of the manifest this lock is for |
| `manifestHash` | hash of the *parsed* manifest (comments stripped) it was resolved against, so editing a comment doesn't invalidate the lock; `install --frozen` refuses when it doesn't match |
| `name` | bosun's logical name for the tool (§5.4); never has to match any backend's naming |
| `spec` | the intent in canonical vers form (§5.6), copied from the manifest so the lock reads on its own; a target may carry its own `spec` where its version scheme differs |
| `backend` / `purl` | which package manager, and the package's canonical purl (unversioned, with qualifiers such as `repository_url`, `arch`, `distro`) (§5.6) |
| `resolved` | the exact version selected |
| `indexSnapshot` | for repo-bound backends: when the index it resolved against was fetched |
| `checksum` | the sha256 bosun verifies a release download against: the one the release publishes, else (trust on first use) the download's own when it was first locked. A kept pin keeps it, so a replaced asset fails to install |

### 4.6 Machine state (what bosun installed, and who wants it)

**Versions travel per project; installs land per user or system-wide.** Manifests are
committed in each project, but `apt`, `brew`, `cargo install` and friends install into a scope
shared by every project on the machine. So two projects can claim the same installed package,
and the state must record that, or prune in one project would uninstall a tool another project
still needs.

State is kept **per install scope, not per project**. Each backend reports which scope an
install landed in (install scope itself stays delegated to the package manager), and bosun
records the claim in the matching file:

| scope | backends (typical) | state file |
|---|---|---|
| **system** | apt, dnf, pacman, zypper, apk, snap, flatpak `--system` (its default), winget `--scope machine`, scoop `--global` | Linux `/var/lib/bosun/state.json` (beside `/var/lib/dpkg`, `/var/lib/rpm`); macOS `/Library/Application Support/bosun/state.json`; Windows `%ProgramData%\bosun\state.json`. Owned by root/admin, readable by all users. |
| **user** | cargo, go install, pipx / uv tool, npm under its own global prefix, flatpak `--user`, scoop and winget defaults, **Homebrew** | Linux and macOS `$XDG_STATE_HOME/bosun/state.json` (default `~/.local/state`); Windows `%LOCALAPPDATA%\bosun\state.json` |

- Not `/root` or another superuser home: those are private home directories (other users
  couldn't read system claims) and aren't portable (macOS root is `/var/root`; Windows has
  none). The system locations above are where each OS's own package managers keep state.
- Homebrew is user scope: it refuses to run as root and its prefix is owned by one user, even
  though its binaries are visible to everyone.

**Same shape as the lockfile**, deliberately: the same package entries keyed per target, so
one Rust type serves both and a state entry can be compared with a lock entry field by field.
Only the header and the per-target leaf fields differ:

| | lockfile | machine state |
|---|---|---|
| header | `lockfileVersion`, `manifest`, `manifestHash` | `stateVersion` |
| package-level `spec` | present (one manifest) | absent: each claim carries its own |
| per-target leaf | `resolved`, `indexSnapshot`, `checksum` | `installed`, `origin`, `held`, `claimedBy` |
| targets present | this machine's target and `any` | this machine's target and `any` |

```json
{
  "stateVersion": 1,
  "packages": [
    {
      "name": "jq",
      "targets": {
        "linux/amd64": {
          "backend": "apt", "purl": "pkg:deb/ubuntu/jq?arch=amd64&distro=noble",
          "installed": "1.7.1-3ubuntu0.1", "origin": "installed", "held": false,
          "claimedBy": [
            { "manifest": "/home/babbitt/workspace/verprop/bosun.yaml", "spec": "vers:deb/>=1.7|<2" },
            { "manifest": "/home/babbitt/.config/bosun/bosun.yaml",     "spec": "vers:deb/>=1.7.1|<1.7.2~" }
          ]
        }
      }
    },
    {
      "name": "git",
      "targets": {
        "linux/amd64": {
          "backend": "apt", "purl": "pkg:deb/ubuntu/git?arch=amd64&distro=noble",
          "installed": "2.43.0", "origin": "accepted", "held": false,
          "claimedBy": [
            { "manifest": "/home/babbitt/workspace/verprop/bosun.yaml", "spec": "vers:deb/>=2.40" }
          ]
        }
      }
    }
  ]
}
```

- `origin: installed`: bosun put it there. Prune may uninstall it, **but only once no manifest
  claims it**.
- `origin: accepted` (the **default** for anything already present): it was installed by
  the user or the OS and satisfies the claim. bosun records that the claim is met and does
  nothing else — no hold, no version change, no removal. If it later stops satisfying a claim
  (an upstream upgrade, or the manifest raises its spec), the install plan reports a diagnostic and
  changes nothing: "git 2.43.0 is installed outside bosun and does not satisfy >=2.45 —
  upgrade it yourself, or `bosun adopt git`".
- **Adoption is the explicit move from `accepted` to `installed`** (`bosun adopt <name>`, or
  `"adopt": true` on the entry). Once adopted, the package *is* `installed`: bosun owns its
  full lifecycle — hold, version changes, and prune once no manifest claims it — exactly as
  after `terraform import`. Ownership is never taken implicitly. Removal stays safe through
  the usual gates, which apply to every removal alike: claims (no prune while any manifest
  wants it), the backend's dry-run in the plan showing any cascade (e.g. dependents
  `apt-get -s remove` would also take), and confirmation. No separate "pre-existing" marker
  is kept.
- `installed` follows the package manager (DECIDED, built). Under a range bosun holds nothing,
  so apt, dnf and apk move a package between bosun runs. Planning (and `status`) reads each
  recorded package's version from its backend's installed-version query (`verify_installed`,
  as verification does) and takes that as current; the recorded one stands where the backend
  reports none (github-release) or the package is gone. Every state write records it (the
  applier reads it again at the start of its transaction), so state catches up on the next
  `install`, `uninstall` or `prune` that writes. A move within every claim's spec is never
  taken back: the end version is the package manager's wherever it is newer than what the lock
  or the claims' selection would give, and the lock follows it; a lock newer still is upgraded
  to. A move outside the spec is warned about (`INSTALLED_OUTSIDE_SPEC`) and planned like any
  installed version outside it. `install --frozen` still applies its lock's version.
- `held`: whether bosun holds the package at `installed` (§5.2: only while some claim's spec is
  exact). State written before bosun recorded this has no `held`; a package bosun installed then
  was held, so it reads as held, and the next `install` releases it if no spec is exact.
- `claimedBy`: every manifest that wants this package, with the spec it asked for. `install`
  in a project adds or updates that project's claims, and drops its claims for entries no longer
  in its manifest — a state-only change; the package stays installed until `prune`. `status`
  reports claims whose manifest no longer exists (repo moved or deleted); `prune` drops them.
- `purl` records the package **as actually installed** (§5.6), so prune removes the
  right thing even if the manifest's name mapping changes later (§5.4).
- **User-level manifest:** `<config dir>/bosun/bosun.yaml` holds personal tools. It is just
  another claimant.

**Conflicts across claimants.** One install scope holds one version, so `install` resolves against
*all* claims for a package, not just the current project's:

- Specs whose vers sets intersect: take the package manager's choice in the intersection (§5.6;
  the highest available version where it has none). This is the main reason to prefer ranges
  for widely shared tools.
- Claims that can't all be satisfied (empty intersection, e.g. two different exact pins): **refuse (DECIDED)**,
  naming the manifests and specs that disagree. Never last-writer-wins, which would flip the
  installed version every time you switch projects.
- **Locks advise; specs decide (DECIDED).** Plain `install` picks a version satisfying every
  claim's *spec*, preferring, in order: this project's pin; else what bosun already installed
  (for any project), when every spec admits it; else the newest every spec allows. A project
  resolving a shared tool for the first time therefore locks the installed version rather than
  bumping it, and `bosun upgrade` is how a project moves it. Only `install --frozen` holds a
  claim to its lock's exact version, and refuses on a clash with another claim.
- **The lock follows what was installed (DECIDED, built).** When other claims make `install`
  end a package at a version this project's lock does not pin (the pin is outside their
  intersection), the lock pins the version installed, written once the install succeeds, so
  `install --frozen` afterwards changes nothing. A declined or failed install leaves the lock
  as it was.

**Multiple OS users.** Claims in the *system* state file also record the claiming user
(`"user": "babbitt"` alongside `manifest`), so prune and conflict checks in one user's project
see every other user's claims on the same system package. User-scope state stays private to
its user.

**Privilege — DECIDED: the user elevates bosun; bosun never elevates itself.** System scope
runs under `sudo bosun …` (Linux/macOS) or from an Administrator terminal (Windows). bosun
checks privilege against the computed plan and fails fast before changing anything (§5.5).

**Reading the system state without elevation (DECIDED, built).** An elevated run leaves the
system state file and its lock root's: the state readable by every user, the lock writable only
by root. A run without elevation reads it under a shared lock taken through the lock file opened
read-only, so `status`, `prune` and `install` see every claim. When the user may not read it
(a state file or directory root's alone), the run leaves the system state out: its packages are
skipped in `install` and `status`, `uninstall` and `prune` plan nothing at system scope, it is
warned about once (`SYSTEM_STATE_HIDDEN`), and the file is never locked or written. An elevated
run that cannot read it fails, as before.

### 4.7 Resolution is per machine

`bosun lock` resolves:

- **the current machine's platform**, using its local package managers, and
- **platform-independent backends** (cargo, go, npm, uv, pipx, gem) under `any`, from their
  public registries over HTTP.

and writes this machine's lock, replacing the last one: the lock is the machine's own, so
there is no merge and nothing for another machine to fill in. Two machines sharing a manifest
agree on the *specs*, not on exact versions; a team that needs one exact version everywhere
writes an exact spec.

**Freshness:** the lock records the `manifestHash` it was resolved against. Editing the
manifest (not just its comments) makes it stale, and `install --frozen` refuses a stale or
missing lock.

**Remote indexes (apt built; the rest backend by backend)**, so one machine can resolve for
another (`render --target`, §8.3, and container builds). Resolution is already split from
installation for this:

- **`Index`** (query side): answers "which versions exist for this target" from each source's
  index over HTTP: Homebrew's formula API, the winget-pkgs repo, scoop bucket repos, apt
  `Packages` / dnf repodata files, crates.io, the npm registry, PyPI. Runs anywhere.
- **`Installer`** (mutation side): runs the local package manager. Runs only on a matching host.

A `BackendFactory` offers its remote index through `remote_index()`, built for the platform the
factory was made for, with no probe and no local program (`None`: it can only ask a local
package manager). cargo (crates.io's sparse index), npm (the npm registry), uv (PyPI's JSON
API), github-release, github-deb and github-rpm (GitHub's API) return the index they always
use, so none of them needs to be on this machine; apt returns its remote index. Resolving for
another machine runs every factory with a command runner that runs nothing, so it cannot touch
this machine's package managers; the one program it may run is this machine's apt, in a scratch
root of its own (below), which reads and writes nothing of this machine's apt state.

**apt's remote index (DECIDED, built).** The release comes from the identity: the deb purl's
namespace is the distro, its `distro` qualifier the codename, its `arch` the architecture.

- **Sources: the target's own, else a stock image's.** When `render` was given the target's
  root filesystem (`--image` / `--rootfs`, §8.3) and the release is the root's (its os-release
  `ID` and `VERSION_CODENAME`), the sources are the ones its apt reads: `etc/apt/sources.list`,
  then each `etc/apt/sources.list.d` file ending in `.list` (one-line format) or `.sources`
  (deb822), in name order. Only `deb` entries are read (`deb-src` and deb822 `Enabled: no` are
  dropped); a deb822 stanza is one entry per URI and suite; `arch=` / `Architectures:` limit an
  entry to those architectures; flat repositories (`deb <uri> ./`) are read; every other option
  is kept for apt. So a repository the Dockerfile added (a vendor's, NodeSource, a PPA,
  backports) is read with the rest, and a derived distro with no stock mirror (Pop!_OS) works.
  A root with no entry for the arch is an error naming it. Otherwise, and only then, the
  sources are what a stock container image of the release lists. Debian: `main` of
  `<codename>` and `<codename>-updates` from `deb.debian.org/debian` and `<codename>-security`
  from `deb.debian.org/debian-security` (checked against `debian:12` and `debian:13`). Ubuntu:
  `main restricted universe multiverse` of `<codename>`, `-updates` and `-security`, from
  `archive.ubuntu.com/ubuntu` and `security.ubuntu.com/ubuntu` on amd64 and
  `ports.ubuntu.com/ubuntu-ports` on arm64. Backports are left out (apt never picks them unless
  asked). The release's own suite missing is an error naming it; a missing `-updates` or
  `-security` suite (sid has neither) is skipped. Any other distro is an error: bosun knows no
  mirror for it. Using the stock sources is logged (info).
- **apt resolves, when it is on this machine.** The release's sources are written, as deb822,
  into a scratch root under the cache (`apt/scratch/<digest of its files>`): an `apt.conf` with
  `Dir` the scratch root, `Dir::State::status` an empty file, `APT::Architecture` and
  `APT::Architectures` the target's, `Acquire::Languages "none"` and `Acquire::GzipIndexes`.
  apt is run with `APT_CONFIG` naming it (apt reads that before any other configuration, so this
  machine's `apt.conf.d` and sources are not read) and `LC_ALL=C`: `apt-get -q update`, then
  `apt-cache policy <package>`, whose version table gives each version and its priority. The
  choice within the spec is the one local apt makes (`Index::select`, the same `apt-cache
  policy` reading): the highest priority the spec admits, then the newest, a lock's version
  first while offered, a priority of 0 or less only when named exactly; so apt's own rules
  choose: pins, `NotAutomatic` / `ButAutomaticUpgrades` suites, priorities, phasing as this
  machine's apt applies it. From the target's root the scratch root takes its `preferences` and
  `preferences.d`, its `trusted.gpg` and `trusted.gpg.d`, and every keyring a `Signed-By` names
  (copied in, the entry pointed at the copy); not its `apt.conf.d`, which may hold hooks
  (`APT::Update::Post-Invoke`) that would run here. The update follows `RELEASE_TTL` (a stamp
  file); `Index::refresh` runs it again. A failed update is an error carrying apt's stderr,
  unless one succeeded before, whose lists are then used with a warning. Foreign architectures
  work as non-root (arm64 from amd64, checked). No `apt-get` (or no `apt-cache`) on this machine
  means bosun reads the mirrors itself, as below; so does apt rejecting a stock image's sources
  (a release without an `-updates` suite), which bosun then reads skipping what is missing.
- **bosun reads the mirrors itself, without apt.** Each suite is fetched over HTTPS (an
  `http://` URI from the target's sources is read as `https://`, logged at debug): its
  `InRelease`, then each component's package index (`Packages.gz`, else `Packages.xz`: Ubuntu
  writes its xz indexes as two streams, which the pure-Rust xz decoder doesn't read, and
  Debian's security suite lists only xz; a flat repository's at its top), fetched by hash when
  the release offers `Acquire-By-Hash`, and refused unless its size and SHA256 are the ones the
  `InRelease` lists. A release file must name the suite asked for (its `Suite` or `Codename`;
  not for a flat repository). Each version gets the priority apt gives a release
  with no pins (apt_preferences(5)): 500, or 100 for a `NotAutomatic` release that is also
  `ButAutomaticUpgrades`, or 1 for `NotAutomatic` alone, a version listed twice taking its
  highest; the choice within the spec is then made as apt's is, above. So backports listed by
  an image (Ubuntu's do) are chosen only when the spec admits nothing else. The target's pins
  are not applied on this path, and a root with `preferences` or `preferences.d` is warned of.
- **Integrity: the hash chain, and a signature only where apt has the target's key.** bosun
  verifies no OpenPGP signature itself and warns of none (decided, §11): HTTPS and the hash
  chain are enough to choose versions, and the image's apt verifies the release against its
  keyring when it installs what was chosen. When apt resolves, it checks each release file
  against the target's own keys copied into the scratch root, as the target's apt would. An entry
  with no key to check it against there (stock sources, a keyring path the root lacks, a
  fingerprint with no trusted keys) is written `Trusted: yes` in the scratch configuration only,
  with its URI read over HTTPS, logged at debug; apt still checks the package indexes against
  the release file's hashes. `Allow-Insecure` and the like are never set; the target's own
  trust is never changed. `Valid-Until` is checked by apt, and not when bosun reads the mirrors.
- **Cache and freshness:** under the user cache directory (`$XDG_CACHE_HOME/bosun/apt`,
  default `~/.cache/bosun/apt`; `%LOCALAPPDATA%\bosun\cache\apt` on Windows), one directory per
  mirror and suite, and the scratch roots under `scratch/`. An `InRelease` is reused for an hour
  (`RELEASE_TTL`) and then fetched again; apt has no time-to-live (it fetches on `apt-get
  update`, with If-Modified-Since), but bosun's HTTP port carries no response headers, and an
  hour keeps a render close to what a fresh `apt-get update` sees while sparing the mirror a
  fetch per run. A package index is reused for as long as its hash is the one the current
  `InRelease` lists, so a new index is downloaded only when the release changes. When the mirror
  can't be reached, the `InRelease` fetched before is used, with a warning. `Index::refresh`
  fetches every `InRelease` again.
- **Scratch roots are pruned by age (DECIDED, built):** a root's digest changes with its sources,
  pins and keys, so old ones pile up (validating M3-8 alone left 1.5 GB). Each root stamps its
  last successful `apt-get update` (`updated`) and each use (`used`); once a run, when the
  remote index first reads through apt (whether or not apt is here), every root neither updated
  nor used for 30 days (`SCRATCH_MAX_AGE`) is removed, judged by its directory's time when it
  has no stamp. A root that cannot be removed is warned about and left. A root removed and then
  needed is written again and its lists fetched afresh.
- **Checked** against `apt-cache policy` after `apt-get update` in `debian:12`, `debian:13` and
  `ubuntu:24.04` containers (2026-10-16): 14, 15 and 14 packages, every candidate the same.
  Rendered from each image with `--image` through apt (2026-10-17), 30 packages each in
  `debian:12`, `debian:13`, `ubuntu:22.04` and `ubuntu:24.04`, every candidate the one
  `apt-cache policy` gives inside; arm64 from amd64, the same as the image's apt with arm64 added
  (Debian; arch-`all` packages aside, which `pkg:arm64` doesn't name) and as bosun's own reading
  (Ubuntu, whose arm64 sources are the ports mirror); a `debian:12` image adding Docker's
  repository with a `Signed-By` keyring and a pin (`docker-ce-cli` to `5:27.*`, priority 1001):
  through apt the pinned version, as inside; without apt the newest, with the pins warned of.

### 4.8 Plan and apply

Modelled on terraform's plan/apply split. The plan is the read-only step of every run
(probe → load state → **plan** → privilege check → apply, §5.5). There is no top-level `plan`
command: each command that changes the machine (`install`, `uninstall`, `prune`) takes
`--plan`, which stops after planning and prints the plan; without it, the command executes
the same value. `--plan` changes nothing and needs no root.

```text
$ bosun install --plan
  + jq          apt    install    1.7.1-3ubuntu0.1   (system)  claimed by verprop
  ~ ripgrep     cargo  upgrade    14.0.3 → 14.1.1    (user)    ^14.0, verprop
  > exa → eza   cargo  migrate    0.10.1 → 0.20.0    (user)    formerly: exa
  = git         apt    accept     2.43.0             (system)  already satisfies >=2.40

Plan: 1 to install, 1 to upgrade, 1 to migrate, 1 accepted as-is.
Needs root for: jq (system). Run: sudo bosun install --scope system
```

```text
$ bosun prune --plan
  - bat         cargo  remove     0.24.0             (user)    no remaining claims
  x claim       ~/old-checkout/bosun.yaml                     manifest no longer exists

Plan: 1 to remove, 1 stale claim to drop.
```

**Actions:** `ensure-source`, `install`, `upgrade`, `downgrade`, `migrate` (`formerly`),
`accept` (record only, no change), `claim` (record this project's claim on a package bosun
already installed at the resolved version, e.g. for another project; version and origin
unchanged), `adopt` (explicit only: `accepted` → `installed`),
`hold` / `unhold` (§5.2) — the `install` plan. `remove` (`installed` origin only, no remaining
claims) and `drop-claim` — the `uninstall` and `prune` plans, and `remove <name>`. Each carries scope,
backend, package ID, from → to version, the claim(s) that drove it, and the privilege it needs.

- **Ordering is fixed:** sources first, then migrations, then unholds, then installs, upgrades
  and downgrades, then holds, so a renamed package's old version is gone before the new one is
  installed, and a hold bosun releases is gone before the version moves.
- **The current version is the package manager's** (§4.6): each recorded package's version is
  read from its backend, so a plan compares against what is installed now, not what bosun last
  recorded, and never takes a distro's update within the spec back down.
- **Diagnostics live in the plan:** conflicting exact pins, entries not applicable on this
  machine, missing backends. Errors block apply.
- **System records wait for elevation:** without elevation, a system-scope `accept`, `claim`
  or `drop-claim` changes only the system state, which only an elevated run writes (§4.6). It
  is left out of the plan as a note (`jq: accept recorded on the next elevated run`; JSON
  `deferred`), reported as such by apply, and planned again by the next elevated run; the
  user-scope actions beside it still apply. `install`, `uninstall` and `prune` defer;
  `remove <name>` does not, since its manifest entry is gone afterwards.
- **Index age** is shown for repo-bound backends, since planning uses whatever index is current
  (refresh is explicit, §9).
- **Exit codes** (like `terraform plan -detailed-exitcode`): `0` no changes, `2` changes
  pending, `1` error — so `bosun install --plan` doubles as a CI drift check.
- **`--format json`** emits the plan as typed actions: the same value `install` executes.

**Confirmation:** on a terminal `install`, `uninstall`, `prune`, `remove` and `migrate` show
what they will change and ask before applying; `--yes` skips the prompt. With no terminal (CI,
a container build, a pipe) nobody is there to answer, so they apply without asking, still
printing what they apply to stderr for the log. `--plan` is how a non-interactive run looks
first.

**Persisted plans: supported by design, not implemented in v1.** The plan is a serializable
value that already carries what a saved plan needs to be checked before apply:

- manifest hash (parsed, comments stripped)
- state fingerprint (the state files it planned against)
- machine fingerprint (OS, arch, distro, privilege)
- each action's expected *before* version

Later, `bosun install --plan -o plan.json` writes it and `bosun install --from plan.json`
executes it only if every precondition still holds, refusing a stale plan (terraform's
behaviour); likewise for `uninstall` and `prune`. v1 only prints plans.

## 5. Backends

### 5.1 The backend trait (the whole design)

Every backend implements the same two traits; adding a backend is one more impl, never a core
change. Lookups are split from installation so the lockfile can be resolved for targets other
than the current machine (§4.7):

```rust
trait Index {                       // query side: may run remotely, for any target
    fn refresh(&self) -> Result<()>;                                   // explicit
    fn query_available(&self, pkg: &Ref, target: &Target) -> Result<Vec<Version>>;
}

trait Installer {                   // mutation side: local machine only
    fn capabilities(&self) -> Capabilities;                            // exact? hold? downgrade?
    fn ensure_source(&self, src: &Source) -> Result<()>;               // registry management
    fn install_exact(&self, pkg: &Ref, v: &Version) -> Result<Installed>;
    fn verify_installed(&self, pkg: &Ref) -> Result<Option<Version>>;
    fn hold(&self, pkg: &Ref, v: &Version) -> Result<()>;              // exact specs only (§5.2)
    fn unhold(&self, pkg: &Ref) -> Result<()>;                         // release bosun's hold
    fn downgrade(&self, pkg: &Ref, v: &Version) -> Result<Installed>;  // explicit flag on apt
    fn uninstall(&self, pkg: &Ref) -> Result<()>;                      // prune; managed set only
}
```

Backends declare **capabilities** (`supports_exact_version`, `supports_hold`,
`supports_downgrade`), because they are wildly unequal (see spectrum below). bosun
degrades-with-a-warning when a backend can't honor a pin.

**Mutations are described, then executed (DECIDED).** The mutating `Installer` methods
(`ensure_source`, `install_exact`, `hold`, `unhold`, `downgrade`, `uninstall`) don't act
directly. `install_exact` leaves the package unheld (apt and dnf release a hold first; apk adds
`pkg=ver`, then the bare name, so the world names it unpinned at that version). Each
returns a **step list**, a typed value saying what to do, and a separate executor carries it
out. One description drives every driver (§8): the host driver executes the steps, and the
container renderer prints them as Dockerfile instructions. The plan (§4.8) can show the exact
commands too.

| step | host executor | container renderer |
|---|---|---|
| `Run { argv, env, scope, ok_exits }` | `CommandRunner` (privilege per scope, §5.5); an exit listed in `ok_exits` (msiexec's 3010, "done, reboot to finish") is success and is reported | `RUN` with shell-quoted argv, accepting the `ok_exits` codes (`… \|\| [ $? -eq 3010 ]`) |
| `Fetch { url, sha256?, dest }` | bosun downloads and verifies the checksum when there is one | `ADD [--checksum=sha256:…] <url> <dest>` |
| `Extract { format, archive, member, dest, mode }` | by `format` (read from the asset name): from a tar (plain, gz, xz, zstd, bzip2) or zip, the one file named `member` at any depth; a single compressed file, decompressed; a raw binary, copied | `RUN tar …` / `unzip …` / a decompressor / `COPY` |

- `Fetch` and `Extract` exist because for `github-release` bosun *is* the installer (§12).
- Queries (`verify_installed`, `query_available`) run directly; they're reads, not part of
  any rendered output.
- Per-architecture steps (different URLs and checksums) are rendered under a `TARGETARCH`
  switch, or as one file per platform.

### 5.2 Three families and the capability spectrum

| Family | install-exact | hold/pin | version availability |
|---|---|---|---|
| **Language** (go/cargo/npm/pipx/gem/uv-tool) | strong (any version) | **weak** (re-run overwrites; no durable hold) | registry — ~everything |
| **OS, versioned** (apt/dnf/apk/winget) | good, repo-bound | strong (`apt-mark hold` / `dnf versionlock` / apk's world `pkg=ver` / `winget pin`) | repo — latest 1–2 (apk: one per branch) |
| **OS, rolling** (brew/scoop) | weak | freeze-current only | weak — old versions scarce |
| **Universal app** (snap/flatpak) | weak: snap selects a channel (track/risk), not a version; flatpak can check out a past commit | strong (`snap refresh --hold`, `flatpak mask`) | snap: current release per channel; flatpak: the remote's commit history |

Behavior differs per tier: language → just re-assert exact every run; versioned-OS → install,
and hold only an exact spec; rolling-OS → warn pin is best-effort + fall back to a downloaded
artifact; universal-app → install the channel or commit, then hold.

**Hold only exact specs (DECIDED).** A package is held (`apt-mark hold`, `dnf versionlock`,
apk's world `pkg=ver`, `winget pin`, `choco pin`, `scoop hold`) exactly when some claim's spec
is one bare version. Under a range or `*` it is installed unheld, so the distro's security and
stable updates reach it between bosun runs; bosun's state keeps the version it installed, and
`bosun upgrade` moves the lock on. bosun records each hold in state (§4.6). When a package bosun
holds no longer has an exact spec, the next `install` plans an `unhold` (logged, and shown in
the plan) before any version change; one an exact spec newly pins gets a `hold` though its
version stays. A hold the backend cannot make is a warning only for an exact spec.

snap and flatpak are heavily used on desktop Linux (snap ships by default on Ubuntu, flatpak
with Flathub on Fedora Workstation, Linux Mint and SteamOS), but they wait until wanted, as do
pacman, zypper, `go install` and gem; dnf, apk, npm and uv are built, and brew is next
(§13). The mechanics in the snap/flatpak row are unverified.

Properties worth designing around:

- **Language backends are platform-orthogonal** — `go install` / `cargo install` behave the same
  on all OSes, collapsing the per-platform columns. So the manifest axis is really
  **per-package × per-backend**, and each backend carries its own platform scope (apt = Linux
  only; cargo = universal). A host resolves to "backends present ∧ platform matches".
- **Build-from-source backends** (go/cargo) need a toolchain present (another host-detection
  probe) and verify differently (`go version -m $(which tool)`; cargo's `.crates.toml`).

### 5.3 Backend registration and installer preference

**Registration: backends register only if present.** Every backend is compiled in. At startup
each one runs a cheap probe (binary on PATH, its version, for build-from-source backends the
toolchain); only backends whose probe succeeds are registered for the run. Probes run in
parallel, once per invocation, in `bosun-host`, which also detects OS, architecture (§5.5) and
distro family (for `linux:<family>` selectors, §5.4).

```rust
trait BackendFactory {
    fn id(&self) -> BackendId;                                   // "apt", "brew", ...
    fn probe(&self, host: &HostInfo) -> Result<Option<Probe>>;   // None = not present
    fn build(&self, probe: Probe) -> Box<dyn Installer>;         // only called when present
}
// registry = factories.filter_map(probe).map(build)
```

- An entry whose only allowed backend isn't registered is **not applicable here**: `install` and
  `status` report it ("apt not present"); it is not a crash.
- `bosun backends` lists registered backends, their versions, and why the others were skipped
  (respects `--format`).

**Selection: the project decides what's allowed, the user's preference picks among it.**

- **The manifest is about the package; the preference is the machine's.** A manifest `install`
  selector names the **set** of acceptable backends, each with its options
  (`backends: {scoop: {}, winget: {package: sharkdp.fd}}`, §5.4). It says nothing about
  order.
- If it names exactly one, that backend is used: a preference never overrides a single
  explicit backend, because version grammar and pin durability differ per backend
  (`ripgrep` via apt = distro's old Debian version; via cargo = crates.io semver, any version).
- If it names several, bosun takes the registered one highest in the **preferred-installer
  order**, resolved with this precedence:

| source | example |
|---|---|
| command-line flag | `--installer brew,cargo` |
| environment | `BOSUN_INSTALLER=brew,cargo` |
| user config | `<config dir>/bosun/config.yaml`: `installer: [brew, cargo]` |
| built in | every registered backend, user-scope ones before system-scope ones (so nothing needs root or Administrator when a user-scope backend can do it), each group in the order `bosun backends` lists them |

  Each level is an ordered list; backends it doesn't mention fall back to the next level. A user
  config that doesn't load (unknown key, a newer `version`) is an error, never a silently
  dropped preference.

### 5.4 Package identity and name mapping

The same tool has different names in different places, and names change over time:

- **Per backend:** `fd` is `fd-find` on apt, dnf and crates.io, `fd` on brew and scoop,
  `sharkdp.fd` on winget. Windows IDs are often vendor-qualified (`BurntSushi.ripgrep.MSVC`).
- **Per distro:** the same backend can differ between distro families.
- **Over time:** packages are renamed or succeeded (`exa` → `eza`; Debian's `python-*` →
  `python3-*`).
- **Version strings too:** apt may carry `9.0.0-1` where brew carries `10.2.0`.

So every manifest entry has a **logical `name`**: bosun's own identity for the tool, used as
the key in the lockfile and machine state, never required to match any backend. A
per-platform **`install`** block says which backend to use and, where it differs, the package
ID and version string. The author still names the backend explicitly; it is just per platform.

```yaml
# yaml-language-server: $schema=https://…/bosun.schema.json
version: 2

# reuse: most entries only need to state where they differ from these
defaults:
  install:
    darwin: {backend: brew}
    windows: {backend: winget}

packages:
  # brew on macOS (from defaults); release archives elsewhere
  - name: bosun
    spec: 0.3.0
    install:
      linux: {backend: github-release, package: <owner>/bosun}
      windows: {backend: github-release, package: <owner>/bosun}

  # Debian-family and Fedora call it fd-find (Debian's binary is `fdfind`)
  - name: fd
    spec: ">=10.2.0|<10.3"
    install:
      linux:debian: {backend: apt, package: fd-find, spec: 9.0.0-1}
      linux:fedora: {backend: dnf, package: fd-find}
      # either is fine; this machine's preference picks. Each names its own package
      windows:
        backends:
          scoop: {}
          winget: {package: sharkdp.fd}

  - name: eza
    spec: 0.20.0
    formerly: [exa]
    install: {backend: cargo}    # a bare block is `any`: cargo isn't tied to an OS
```

(Versions above are illustrative.)

Rules (draft syntax):

- **Selectors:** `install` keys are `any`, `<os>`, `<os>:<distro>`,
  `<os>:<distro>@<release>`, or the libc selectors `linux:<libc>[@<range>]` (below);
  the most specific match for the current machine wins
  (`<distro>@<release>` > exact distro `ID` > `ID_LIKE` match > libc > plain `<os>` > `any`), then
  `defaults.install`. A bare block under `install` (its keys are block keys, not selectors) is
  `any`; mixing the two is an error. No match = the entry doesn't apply on this platform
  (reported by `status`, not an error). Distro and libc detection are below.
- **`backends`: the acceptable backends and their options.** `backends: {<backend id>:
  {<option>: <scalar>}}`; `backend: <id>` with an optional `package:` is the shorthand for one
  backend whose only option is its package. **`package`** is bosun's option, which every
  backend takes: the backend's own package id, the purl's name, never a qualifier. The rest
  are the backend's own (github-release's `asset` template, …): bosun-core carries them
  verbatim, each backend declares them in `BackendMeta::option_keys`, and the manifest is
  checked against every compiled-in backend at load, so an unknown backend or option is an
  error. The chosen backend's options become qualifiers on its identity purl, which it
  validates when it reads them. A block-level `package` beside a `backends` map is an error
  (name it per backend).
- **Defaults keep it sparse:** `package` defaults to `name`; a per-platform `spec` overrides the
  entry's `spec`; `defaults` supplies backends and their options, so entries only state
  exceptions. An entry that names no backend inherits the defaults' set, and its `package`
  (`windows: {package: sharkdp.fd}`) is that backend's id; one that names a backend inherits
  that backend's defaults options per key. `defaults` can't name a `package`: it would be
  every entry's.
- **`formerly`:** previous names (logical or backend package IDs). On `install`, if a package
  under a former name is present: bosun-installed → migrate (uninstall old, install new,
  update state); accepted or unmanaged → report it, don't touch it (migration uninstalls the
  old package, which bosun only does for what it owns).
- **What's recorded:** the lockfile and machine state store the backend package ID actually
  resolved/installed, so prune and migration act on the real package even after the mapping
  changes.
- **Verification is by package ID through the package manager, never by binary name**, so
  binary-name differences (`fdfind` vs `fd`) don't affect bosun (PATH is delegated anyway).
- **Lookup aid (later):** `bosun add` could suggest per-backend IDs from Repology, which tracks
  package names across hundreds of repositories. A suggestion source only, never a runtime
  dependency (its API is rate-limited).

#### Distro detection

Source: `/etc/os-release` (systemd/freedesktop standard; fallback `/usr/lib/os-release`),
present on essentially every current distro and container base image. It is plain
`KEY=value` lines, so a small hand-written parser suffices. Fields used, and the release
token each distro gets (below):

| distro | `ID` | `ID_LIKE` | `VERSION_ID` | `VERSION_CODENAME` | `UBUNTU_` / `DEBIAN_CODENAME` | release token |
|---|---|---|---|---|---|---|
| Ubuntu 24.04 | `ubuntu` | `debian` | `24.04` | `noble` | `noble` / — | `noble` |
| Debian 12 | `debian` | — | `12` | `bookworm` | — | `bookworm` |
| Linux Mint 22 | `linuxmint` | `ubuntu debian` | `22` | `wilma` | `noble` / — | `noble` |
| LMDE 6 | `linuxmint` | `debian` | `6` | `faye` | — / `bookworm` | `bookworm` |
| Pop!_OS 24.04 | `pop` | `ubuntu debian` | `24.04` | `noble` | `noble` / — | `noble` |
| elementary OS 7 / 8 | `elementary` | `ubuntu debian` | `7` / `8` | `horus` / `circe` | `jammy` / `noble`, — | `jammy` / `noble` |
| Kali | `kali` | `debian` | `2026.3` | `kali-rolling` | — | `kali-rolling` |
| Fedora 41 | `fedora` | — | `41` | — | — | `fedora-41` |
| RHEL / Rocky | `rhel` / `rocky` | `fedora` / `rhel centos fedora` | `9.4` | — | — | `rhel-9.4` / `rocky-9.4` |
| Amazon Linux 2023 | `amzn` | `fedora` | `2023` | — | — | `amzn-2023` |
| openSUSE Tumbleweed | `opensuse-tumbleweed` | `opensuse suse` | — | — | — | — |
| Arch / Manjaro | `arch` / `manjaro` | — / `arch` | — | — | — | — |
| Alpine | `alpine` | — | `3.20.3` | — | — | — |

(Typical values. The Debian, Ubuntu and Alpine rows have been checked in containers, as have
LMDE 6 (`linuxmintd/lmde6-amd64`), elementary OS 7 (`elementary/docker:stable`) and Kali
(2026-10-17); Linux Mint 22, Pop!_OS 24.04 and elementary OS 8 are their installed files'
known contents: `linuxmintd/mint22-amd64` keeps Ubuntu's base-files, so it reads as Ubuntu
noble. Alpine's `VERSION_ID` is a point release.)

- **`ID_LIKE` (DECIDED, built):** `linux:<distro>` matches when `ID` equals it **or**
  `ID_LIKE` contains it, so Ubuntu, Mint and Pop!_OS all match `linux:debian` without being
  listed. An exact `ID` match outranks an `ID_LIKE` one.
- **Release selectors (DECIDED, built):** `linux:<distro>@<release>`, for a package
  that differs between releases of one distro. apt-only manifests need them: `just` is absent
  from Debian 12 and Ubuntu 22.04, 1.21 on Ubuntu 24.04 and 1.40 on Debian 13; `jq` is 1.6 on
  bookworm and jammy, 1.7.1 on trixie and noble. Without them, a manifest written on one
  release fails to resolve on another.
  - **The release token is the purl `distro` qualifier value** (§5.6), matched exactly, so a
    selector and the identity bosun records use one vocabulary: deb distros (`ID` or `ID_LIKE`
    `debian`) use the codename their apt reads: `UBUNTU_CODENAME`, else `DEBIAN_CODENAME`, else
    `VERSION_CODENAME` (`linux:debian@bookworm`, `linux:ubuntu@noble`, `linux:linuxmint@noble`
    on Mint 22, whose apt reads Ubuntu's `noble` suites beside Mint's own `wilma`); rpm
    distros (`ID` or `ID_LIKE` `fedora`, `rhel` or `suse`) use `<ID>-<VERSION_ID>`
    (`linux:fedora@fedora-41`), as in the official purl suite. No prefix or numeric rule on
    `VERSION_ID` (`ubuntu@24` names nothing).
  - The `<distro>` of a release selector matches `ID` only: a release is a release of that
    distro, and `ID_LIKE` carries none.
  - **A derivative's own codename names nothing (DECIDED):** `linux:linuxmint@wilma` does not
    match Mint 22; `linux:linuxmint@noble` does, as it does Mint 22.1 (`xia`) and 22.2
    (`zara`), which read the same Ubuntu suites and so offer the same versions. One token per
    machine keeps the selector and the recorded purl in one vocabulary.
  - A distro whose package type has no `distro` rule (Arch, Alpine) gets no release
    selectors. This holds for Alpine after apk lands too: bosun doesn't invent a branch rule
    (`alpine@3.20`) ahead of purl; Alpine entries use `linux:alpine` and libc keys.
  - The selector matches when some registered backend of its block (or, for a block naming
    none, of the `defaults` block it inherits) spells this machine's release that way; which
    of the block's backends installs is still the machine's preference (§5.3). So a block
    naming only github-release never matches a release: name the release where the repo has
    the package (`linux:debian@trixie: {backend: apt}`) and let the others fall back to
    `linux`.
- **Missing facts are never an error by themselves.** os-release is optional, and so is
  every field after `ID`:
  - No os-release file (e.g. a `scratch` image): no distro, so only `any` and plain `linux`
    selectors apply. A backend whose identity needs the distro (apt's `pkg:deb/<ID>/…`) can't
    resolve there, and says so. Legacy files (`/etc/redhat-release`, `lsb_release`) are not
    supported.
  - No release token (rolling distros such as Arch and Tumbleweed; a deb distro without
    `VERSION_CODENAME`; an rpm distro without `VERSION_ID`): no `@<release>` selector
    matches, the exact-`ID`, `ID_LIKE` and plain selectors still do, and the purl carries no
    `distro` qualifier. Debian testing and sid have a codename but no `VERSION_ID`, so deb
    release selectors still work there.
- **`render` needs the distro supplied:** a Docker platform string (`linux/amd64`) doesn't say
  which distro the base image is, so `render` takes it in selector form (`--distro
  ubuntu@noble`), or reads the image's own os-release (§8.2).

#### libc (DECIDED, built)

On Linux the C library is a machine fact beside OS, arch and distro. A dynamically linked
release binary depends on libc, not on the distro: a glibc build doesn't run on Alpine (musl),
and one linked against a newer glibc fails on an older one. Neither purl nor the OCI platform
carries libc, so bosun reuses existing vocabularies rather than inventing one:

- **Family:** `gnu` | `musl`, the environment field of a Rust target triple. Selectors
  `linux:gnu` and `linux:musl`; github-release's (and github-deb's and github-rpm's)
  `{{ libc }}` and `{{ rust_target }}` (`{{ arch_uname }}-unknown-linux-{{ libc }}`) asset
  template variables ([asset-templates.md](../asset-templates.md)).
- **Version:** a glibc floor in the manylinux sense (PEP 600: `manylinux_2_31` runs on glibc
  2.31 and later) is a vers range on the selector: `linux:gnu@>=2.31`. Any vers `generic`
  range is accepted, with or without the prefix (`linux:gnu@vers:generic/>=2.31|<3`); a range
  of another vers type, or one vers refuses (unsorted, whitespace), is a manifest error that
  says why.
- **The names are reserved.** After `linux:`, `gnu` and `musl` are always a libc, so `@`
  after them is a range, never a release: `linux:gnu@>=2.31` is a libc range,
  `linux:debian@bookworm` a release. No distro in use has `ID=gnu` or `ID=musl`; one that did
  could only be selected by its `ID_LIKE` family or plain `linux`.
- **Linux only:** `windows:gnu` or `darwin:musl` is a manifest error, not a selector that
  never matches.
- **Detection** (`bosun_host::libc`): the family from the `PT_INTERP` of `/bin/sh`
  (`ld-musl-*` is musl, `ld-linux-*` glibc), read by a small ELF reader (32- and 64-bit,
  either byte order; no ELF crate); the version by running that loader through the
  `CommandRunner` port with `--version` (glibc's prints `… stable release version 2.36.` on
  stdout; musl's prints its banner, `Version 1.2.5` included, to stderr and exits 1, which is
  read anyway). Never from bosun's own libc (`gnu_get_libc_version`): bosun ships as a static
  musl binary, so its own libc says nothing about the machine; a static musl bosun reports
  `gnu 2.36` on Debian 12. The unit tests use the real `/bin/sh` headers and loader output of
  Debian 12 and 13, Ubuntu 22.04 and 24.04 (glibc 2.36, 2.41, 2.35, 2.39) and Alpine 3.20
  (musl 1.2.5), captured in containers.
- **Missing facts are not errors.** No `/bin/sh`, a statically linked one, or a loader that
  is neither (bionic's `linker64`): libc unknown, so libc selectors don't match and a
  template that uses `{{ libc }}` or `{{ rust_target }}` is an error saying the libc is
  unknown. A loader that won't run or prints no version: the family is known, the version
  isn't, so a bare `linux:gnu` matches and a ranged one doesn't.
- **Shown** by `bosun backends` and `bosun status <manifest>`: a `libc: gnu 2.36` line (or
  `libc: unknown`) on Linux, and a `libc` member (`{family, version}` or `null`) in JSON.
- **Rank (DECIDED, built):** libc is its own rank, between `ID_LIKE` and plain `<os>`: a
  distro selector names a package source, libc only a binary's ABI, so any matching distro
  key wins and a distro with no key of its own (Alpine with no `linux:alpine`) falls through
  to `linux:musl`. A ranged `linux:gnu@>=2.31` outranks a bare `linux:gnu`. libc and distro
  are not combined in one key.
- **Version scheme (DECIDED, built):** the range is a vers `generic` range
  (`vers:generic/>=2.31`), compared with bosun's generic chunk ordering (§5.6); glibc and musl
  versions are plain dotted numbers.
- **Test seam:** the acceptance fixture takes `"libc": {"family": "musl", "version": "1.2.5"}`
  ("the machine's C library is musl 1.2.5"); without it the libc is unknown.

### 5.5 Machine interface and privilege

All OS-specific behaviour sits behind one polymorphic interface (trait in `bosun-ports`,
implementations in `bosun-host`): `LinuxMachine`, `MacMachine`, `WindowsMachine`, plus a
`FakeMachine` for tests, so the core never branches on OS.

```rust
trait Machine {
    fn os(&self) -> Os;
    fn arch(&self) -> Arch;
    fn distro(&self) -> Option<&Distro>;            // Linux: from os-release (§5.4)
    fn privilege(&self) -> Privilege;                // Elevated | Unelevated
    fn invoking_user(&self) -> User;                 // the human, even under sudo
    fn state_path(&self, scope: Scope) -> PathBuf;   // §4.6 locations
    fn elevation(&self) -> Elevation;                // how to elevate; drives error text
}
```

| | Linux | macOS | Windows |
|---|---|---|---|
| elevated when | effective UID is 0 | effective UID is 0 | process token is elevated |
| invoking user under elevation | `SUDO_USER` / `SUDO_UID` | `SUDO_USER` / `SUDO_UID` | the token's user |
| how to elevate (error text) | re-run with `sudo` | re-run with `sudo` | open a terminal with "Run as administrator" and re-run (Windows 11's opt-in `sudo` may work, but is not relied on) |

**Architecture is the machine's, not bosun's build (DECIDED).** An x86_64 bosun can run
translated or emulated on arm64 (Rosetta, qemu-user), so its compiled target says nothing
about the machine, as its libc doesn't (§5.4). On Linux and macOS bosun runs `uname -m`
through the `CommandRunner` port, a native program answering for the machine: `x86_64`
(`amd64` on the BSDs) is amd64, `aarch64` (`arm64` on macOS) is arm64. On Windows it reads
`PROCESSOR_ARCHITEW6432`, set only for a 32-bit process under WOW64 and naming the native
processor, else `PROCESSOR_ARCHITECTURE` (`AMD64`, `ARM64`). Any other architecture (`armv7l`,
`i686`, `x86`) is an error naming it, as is a `uname` that fails.

**`Machine` is orthogonal to backends.** It knows nothing about package managers. Which
backends exist on a machine is decided only by each backend's own probe (§5.3) against the
machine's facts, and each backend reports the install scope of what it installs. Homebrew
registers on macOS *and* on Linux wherever it is installed, at user scope on both; cargo, npm,
uv and go register on every OS. The only coupling is through privilege: whenever an action's
scope is `system`, the `Machine` decides whether the current process may perform it and how
to word the remedy.

**Run order: probe → load state → plan → privilege check → apply.** Planning is read-only and
needs no root (`dpkg-query`, `rpm -q` and friends work unprivileged), so the check is against
what the plan will *change*, not against what the manifest *mentions*: a manifest full of
already-satisfied apt entries needs no sudo. Exception: `refresh` for apt/dnf always needs
root, because updating their index does.

**Fail fast, both directions, before anything changes:**

- **Unelevated, plan changes system scope** → `ElevationRequired`. A system-scope action that
  changes only state (`accept`, `claim`, `drop-claim`) is no change: it waits for the next
  elevated run (§4.8), and the rest of the plan applies.
- **Elevated, plan changes user scope** → `MustNotBeElevated`. Under sudo, user-scope installs
  would land in root's home (and `$HOME` under sudo varies by distro), and Homebrew refuses to
  run as root. `--scope system|user` splits a mixed run:
  `sudo bosun install --scope system`, then `bosun install --scope user`.
- **Mixed plan, no `--scope` — DECIDED: fail**, printing the two commands to run. Never apply
  whichever half the current privilege allows, which would leave a half-applied manifest.
- Under elevation, claims in system state are recorded for the **invoking user**, not root.

**Errors are structured values with a remedy**, rendered by the platform's `Elevation`:

```text
error: 2 system-scope packages need root (apt: jq, fd-find). Nothing was changed.
  re-run: sudo bosun install --scope system
```

```text
error: 1 machine-scope package needs Administrator (winget: sharkdp.fd). Nothing was changed.
  open a terminal with "Run as administrator" and re-run: bosun install --scope system
```

```json
{ "error": "elevation_required", "scope": "system",
  "packages": [ { "name": "jq", "backend": "apt", "package": "jq" } ],
  "changed": false,
  "remedy": "sudo bosun install --scope system" }
```

### 5.6 Standards: purl for identity, vers for ranges (DECIDED)

bosun does not invent a package-ID or version-range syntax. It uses the package-url family:

- **purl** (ECMA-427, Package-URL): `pkg:type/namespace/name@version?qualifiers#subpath`.
  Registered types cover most bosun backends: `deb`, `rpm`, `alpm`, `apk`, `brew`, `cargo`,
  `golang`, `npm`, `pypi`, `gem`, `github`, `generic`. Spec and conformance tests:
  github.com/package-url/purl-spec (`types/`, `tests/spec/`, `tests/types/`).
- **vers** (Package-URL version range specifier, being standardized by Ecma TC54-TG2):
  `vers:<type>/<constraint>|<constraint>…`. Used by CycloneDX and CSAF. Spec and tests:
  github.com/package-url/vers-spec (`docs/specification/standard/`, `tests/`).

**purl: the recorded identity of a package on a target.**

- The manifest stays sparse. `install.<selector>.package` is the backend's native ID
  (defaulting to `name`), or a full purl where that's clearer. bosun builds the canonical
  purl from the backend's purl type plus host facts. For deb/rpm the namespace is the distro
  `ID`, with `arch` and `distro` qualifiers; `distro` is the release token (§5.4: deb
  the codename apt reads, rpm `<ID>-<VERSION_ID>`, the latter waiting on an rpm-typed backend) and
  is left out when the machine has none. A source becomes the standard `repository_url`
  qualifier.
- The lockfile and machine state record that purl (unversioned) per target, alongside
  `backend` and `resolved`/`installed`. It replaces the separate `package` and `source`
  fields. The backend's native ID is derived from the purl (e.g. `owner/repo` for `github`).
- The logical `name` (§5.4) stays: purl identifies a package *within* one ecosystem; bosun's
  `name` ties one tool's purls across ecosystems together.
- Backends with no registered purl type (winget, scoop, snap, flatpak) use their backend id
  as an unregistered type (`pkg:winget/sharkdp.fd`). The spec permits this, with a warning.
- `backend` is still recorded: a purl type doesn't always pick the installer (`pypi` → uv
  or pipx; `rpm` → dnf or zypper).

**vers: the meaning of every `spec`.**

- A `spec` is either a bare version (equality) or vers constraints, written either in full
  (`vers:deb/>=1.7.1|<1.7.2~`) or without the `vers:<type>/` prefix (`>=2.40|<3`). The type
  comes from the selected backend's purl type, so one entry-level `spec` is read in each
  target's own version scheme (§6).
- Containment follows the vers algorithm exactly: sorted unique constraints, alternating
  bounds, `!=` exclusions, `*` for all versions; a version is in the range if it lies in any
  interval. Each type's comparison rules come from bosun's version model (§6). Types with no
  vers definition yet (github-release tags, brew) use `semver` where the versions are SemVer,
  otherwise bosun's generic chunk ordering.
- **npm and PyPI ranges are evaluated as their ecosystems evaluate them (DECIDED).** vers
  stays the stored form; for containment bosun converts the range to the native grammar and
  asks the ecosystem's own rules (`bosun_core::native`):
  - `npm` → a node-semver range: each piece a comparator set (`>=1.2.0 <2.0.0`, a bare
    `1.2.0`, `*`), pieces joined by ` || `; node-semver has no `!=`, so an exclusion splits
    the interval holding it. Evaluated by the `nodejs-semver` crate, whose pre-release rule
    is node-semver's: a pre-release satisfies a set only when a bound of the same
    `major.minor.patch` is one.
  - `pypi` → PEP 440 specifier sets, one per interval (PEP 440 has no union), each carrying
    the exclusions as `!=`. Evaluated by `pep440_rs`: `<V` admits no pre-release of `V`, `>V`
    no post-release of `V`, `==V` ignores a local label.
  - Several claims together are the conjunction of each combination of pieces. The same
    native forms are what bosun hands npm and uv to resolve (below). A version the native
    grammar rejects falls back to the vers walk.
- **No other range grammar in M1:** no `^`, `~` or `1.7.*`. They can come back later as
  documented sugar that expands to vers.
- The lockfile stores each spec in canonical vers form (`vers:<type>/…`), so a reader needs
  only the vers spec to know what a lock entry allowed.

**Selection: the package manager's choice within the set, no policies (DECIDED).** A spec
is a vers set, and bosun owns the set: its grammar, its storage and the intersection of every
claim's (§4.6). The version *within* it is the package manager's view, so bosun keeps no
ranking of its own that could drift from the installer's (`Index::select`):

- **apt** chooses by pin priority, read from `apt-cache policy <pkg>`, as apt chooses its
  candidate: the highest priority, the newest among equals, never behind the installed
  version below priority 1000. The installed version, when admitted, is a candidate even once
  no source offers it (dpkg's status file gives it 100), so bosun keeps it as apt does. A version at priority 100 (a backports suite) loses to the
  release's 500 unless the set admits only it. A version at priority 0 or below is never
  apt's candidate; bosun takes it only when the spec names it exactly, as `apt-get install
  pkg=version` installs it despite the pin (and warns). Phased updates do not change the
  choice: apt defers them only in `upgrade`, and `apt-get install pkg` takes them.
- **dnf** chooses by repository priority, then version, as `dnf install` does; a
  lower-priority repository's build only when the set admits nothing better (an explicit
  `name-version` installs it). Like apt below priority 1000, it never goes back past the
  installed build while the set admits another: the installed build (`rpm -q`), when admitted,
  stays over an older one of better priority, and is kept even when no repository offers it.
  Excludes and module filtering are already `repoquery`'s.
- **npm and uv** resolve the set themselves on this machine: npm with its own picker (`npm
  pack --dry-run --json name@<range>`: the `latest` dist-tag when it satisfies, then
  non-deprecated, then engine-compatible versions), uv with `uv pip compile` for the
  interpreter its tools use (requires-python, yanked releases (PEP 592), wheels, its own
  index), with only the configuration `uv tool install` reads (run from the filesystem root:
  no project's `uv.toml`, `[tool.uv]` or `.python-version`). bosun checks the answer is in
  the set and records it.
- **Everything else** (cargo, apk, the github backends, the Windows backends) and every
  remote index (`render --target`, no package manager to ask) takes the **highest available
  version in the set**.

A locked version stays while the package manager still offers it and the set admits it
(lockfiles advise, specs decide). When nothing fits, resolution fails naming the spec and what
was available. A bare version is a one-version set, so an exact pin needs no separate mode.
There is no `policy` field in the manifest, lockfile or state, and no `deviation`: what was
resolved is always inside what was written.

- "Prefer 1.7.1, but accept a later revision" is a range: `vers:deb/>=1.7.1|<1.7.2~`.
  Because selection takes the highest, write an upper bound to stay close to a version. An OS
  package's default spec is `*` (§4.2): the release's repositories, not a bosun ceiling, bound
  what it takes.
- **Pre-releases (language schemes only):** they are *in* a vers set by plain ordering, but
  selection never picks one unless some constraint version is itself a pre-release (the SemVer
  convention, §12 item 3), or the spec is a bare pre-release version. SemVer, PEP 440 and the
  generic scheme each classify pre-releases by their own ecosystem's rule.
- **OS versions have no pre-releases (DECIDED).** Debian, RPM and Alpine versions never
  are: apt, dnf and apk install whatever the configured repositories carry, and repository
  configuration (suites, pin priority, enabled repos, branches) is how a distro keeps
  experimental builds away. A `~` or `_rc` only orders the build (`1.0~rc1 < 1.0`); bosun
  does not skip it, so `*` selects what the repositories make newest, stable updates
  (`3.0.22-1~deb12u1`) and backports (`~bpo12+1`) included.
- Reproducibility comes from the lockfile and `install --frozen` (§7), not from a policy.

**Conformance:** bosun's purl and vers code must pass the official JSON suites from both repos,
vendored as test fixtures (MIT). Existing crates (`purl`, `vers-rs`, `unive_rs` on crates.io)
are used only if they pass those suites and are maintained; otherwise bosun-core implements
both (the grammars are small).

## 6. Version model

- Version strings are **not normalizable** across ecosystems (Debian `epoch:upstream-rev`, RPM
  NEVRA, PEP 440, SemVer — different grammars *and* ordering rules). Stored per-backend.
- **Ordering = regex-decompose into typed chunks, then compare the chunk sequence** — exactly how
  `dpkg --compare-versions`, `rpmvercmp`, and SemVer work internally.
  - A per-platform regex (named capture groups, with per-package override) extracts chunks; a
    chunk is typed `Numeric(u64) | Alpha(String) | Tilde`, parsed into `Vec<Segment>` with an
    `impl Ord`.
  - Three non-obvious rules the `Ord` must encode: (1) numeric chunks compare numerically
    (`1.10 > 1.9`), (2) a pre-release chunk *lowers* precedence (`1.0rc1 < 1.0`; Debian `~` sorts
    before empty), (3) arity/missing chunks need a defined rule (pad-zero vs rank-lowest, absent
    epoch = 0).
  - Reference impls to depend on where possible: `deb-version`, `semver`, `pep440_rs` (PEP 440,
    vers type `pypi`).
    RPM (`rpmvercmp`) and Alpine (apk-tools) ordering are bosun's own, tested against each
    tool's reference cases. Reserve the custom regex-chunk path for the long tail.
  - **Ordering stays bosun's, held to the tools (DECIDED).** `render --target` and claim
    intersection need it where the tool is absent, so bosun keeps its ports, and the real tier
    checks them against the tools themselves: every version a Debian 12/13, Fedora 41, Rocky 9
    or Alpine 3.20/3.23 index offers, sorted by bosun, must be in order by `dpkg
    --compare-versions`, rpm's `rpm.vercmp` or `apk version -t` pair by adjacent pair (so the
    tool orders the whole corpus as bosun does), as must each tool's conformance pairs.
  - **apk ordering follows the host's apk-tools major.** apk-tools 2 (Alpine 3.22 and earlier)
    and 3 disagree on a few versions: under 2 each leading zero of a `.`-number after the first
    lowers it (`1.0 > 1.00`), a letter may take a number (`1.0b2`), and `~` is outside the
    grammar, so what follows it is not ordered (`1.0~ab = 1.0~b`). The apk backend reads the
    major from `apk --version` when it probes and every Alpine version on the machine follows
    that major's rules, ordering and validity both; apk-tools 3's apply until one is probed.
  - Go pseudo-versions (`v0.0.0-TIMESTAMP-hash`) validate the model — they order by the embedded
    timestamp chunk, no special-casing.

## 7. Resolution (orthogonal to backend)

Every spec is a vers set, and resolution takes the backend's package manager's choice in it:
apt's and dnf's priorities, npm's and uv's own resolution, else the highest available version
(§5.6). There are no per-entry policies: an exact pin is a one-version set and a range is a
wider one.

Consequences:

- Containment and `highest-in-set` are impossible without correct ordering (§6). That is the
  payoff for getting ordering right.
- A pin that the configured sources no longer carry is unsatisfiable. The remedy is to widen
  the range (e.g. `>=1.7.1|<1.7.2~` for "any Debian revision of 1.7.1"), not a fallback mode.
- Ranges **break reproducibility by design**, so bosun inherits uv/npm's frozen-vs-update split.
  **A locked version stays until it's upgraded (DECIDED):** resolving keeps each pin that
  still fits — the same backend and package identity, still published, still in the spec —
  and gives the package manager's in-spec choice only to new entries and pins that no longer fit. A new
  entry first takes the version bosun already installed, when it fits (§4.6).
  - `bosun lock` — resolve and record versions for this machine (§4.7), keeping pins.
  - `bosun install --frozen` — apply this machine's lock verbatim, no re-resolve, no lock
    write. A container build or CI job runs `bosun lock` then `install --frozen` in the same
    environment, or relies on exact specs.
  - `bosun upgrade [pkg…]` — let go of pins, re-resolve, rewrite the lock, show old → new.
  - plain `bosun install` — resolves like `lock`, prefers the pinned version across claims
    (§4.6), and writes the lock once the install succeeds.
- The plan shows "spec → resolved" for every entry, so a range resolving somewhere new is
  visible before applying.

## 8. Propagation drivers

### 8.1 host-apply

Imperative reconcile of the current machine to the lock. Needs a **dry-run/preview** on OS
managers (they can cascade package erasures) before applying.

### 8.2 container-inject

Feed pinned versions into an image build. **The primary near-term consumer is dev container
builds** — a `.devcontainer` / reproducible dev-environment image that gets exactly the
manifest's pinned toolchain so every developer (and CI) shares one versioned toolset. Production
image builds are the same mechanism. Both of the first two shapes are supported (DECIDED):

- **build-args / env** — the Dockerfile takes versions as `ARG`s (or `ENV`) and bosun supplies
  them with `bosun render` (§8.3). Simplest; stable build args give clean layer-cache
  behaviour. The author writes the `RUN` lines.
- **in-build execution (DECIDED: the M3 container path)** — `COPY bosun + bosun.yaml` then
  `RUN bosun install --yes`. `render` and `Containerfile.bosun` wait for remote indexes (§4.7),
  starting with the backends a dev container needs. The lock
  is the machine's own (§4.5), so the image resolves against its own repositories; exact
  versions across builds come from exact specs plus a pinned base-image digest. Single source
  of truth; needs the
  bosun binary in the build stage — this is where the **static musl binary pays off**
  (`COPY --from` into scratch/distroless, no runtime deps).
- **generated bosun layer (`Containerfile.bosun`)** — bosun renders a whole file it owns,
  which layers the tools onto the user's image:
  `ARG BASE` / `FROM ${BASE}`, then each package's step list (§5.1) as `RUN` / `ADD
  --checksum` instructions, with names and versions as literals. The user's Dockerfile is never
  edited. Chain it with `docker build --build-arg BASE=<image>` or a bake file that uses the
  base as a named build context. A tool bump rebuilds only the tool layer, not the base.
  Rendering needs the base image's distro (for selectors and purl namespace) and, for apt, its
  sources: read from the image itself (`render --image <ref>`, which copies its os-release and
  `/etc/apt` out without running it, §8.3), or declared per target (`--distro debian@bookworm
  --like …`, §5.4, with a stock image's sources). Versions for a platform other than the
  rendering machine's come from remote indexes (§4.7; apt so far), as `render --target`
  resolves them (§8.3); a backend without one is rendered inside the base image, or from exact
  specs.
- **in-build execution inside that layer** is the alternative when the distro can't be known
  ahead of time: the generated layer `COPY`s the static bosun binary and runs
  `bosun install --yes`, which reads the real base's os-release.
- **Not a Dev Container Feature (DECIDED).** Features are consumed only by devcontainer-aware
  tooling (the devcontainer CLI, VS Code, Codespaces), not by plain `docker build` / `podman
  build` / `buildah` or CI; ctxloom, for one, builds `.devcontainer/Dockerfile` directly.
  `Containerfile.bosun` works with any OCI build tool, so it is the one container path. A
  Feature could later wrap it, if wanted.
- Standard Dockerfiles have no `INCLUDE` directive (third-party BuildKit frontends add one),
  so "include a bosun fragment into the main Dockerfile" is done by layering with `FROM`
  instead.

**Hazard — repo drift makes the container case *harder*, not easier.** `RUN apt-get install -y
pkg=1.24.0-2ubuntu7.3` rots: base-image repos rotate and drop old versions, so a pin that built
last month fails later. So the container mode needs a first-class story for "unsatisfiable":
pin the base-image digest, point at a snapshot repo (snapshot.debian.org / archived AppStream),
or bake a downloaded artifact into the build context. Language backends (cargo/go/npm) sidestep
this (registry-bound).

### 8.3 `bosun render`: versions as build args and environment variables

Dockerfiles take versions as `ARG` (build time) or `ENV` (persisted into the image); bosun's
job is to supply the values cleanly for one target platform. `render` uses subcommands for its
output styles so they don't collide with the global `--format` flag:

| command | output | use with |
|---|---|---|
| `bosun render args --target linux/amd64` | `--build-arg FD_PACKAGE=fd-find --build-arg FD_VERSION=9.0.0-1 …` | `docker build $(bosun render args --target linux/amd64) .` |
| `bosun render env --target linux/amd64` | `FD_PACKAGE=fd-find` / `FD_VERSION=9.0.0-1` lines | compose `.env`, `devcontainer.json` `build.args` via `${localEnv:FD_VERSION}`, `set -a; . ./versions.env` |
| `bosun render json --targets linux/amd64,linux/arm64` | plain-JSON versions file, lockfile-shaped, per target | multi-arch builds: the Dockerfile reads it with `jq` and `TARGETPLATFORM` (needs remote indexes for the platforms that aren't this machine's, §4.7) |

```dockerfile
ARG FD_PACKAGE
ARG FD_VERSION
RUN apt-get install -y "${FD_PACKAGE}=${FD_VERSION}"
```

- **Variable names (DECIDED, built)** derive from the logical name: uppercase, every
  character but an ASCII letter or digit → `_`, suffixes `_PACKAGE` and `_VERSION` (in that
  order, entries in manifest order). The package ID is the backend's own (`fd-find`, a
  GitHub `owner/repo`), emitted because it differs per platform, so the Dockerfile never
  hardcodes it. An entry's `var: FD` names the stem instead; it must be a variable name
  (ASCII letters, digits, `_`, not led by a digit), checked when the manifest loads, and it
  is part of the manifest hash (left out of the hash when absent, so older locks still
  match). Two entries with one stem are an error naming both, and so is a name whose stem
  starts with a digit (`7zip`: "set `var`"); both are checked over every entry, so a
  manifest renders, or fails to, the same on every machine.
- **Output:** `render args` is one line of `--build-arg NAME=value` flags; `render env` one
  `NAME=value` per line; `render json` the versions file, lock-shaped:
  `{"packages": [{"name", "var", "targets": {"linux/amd64": {"backend", "package", "purl",
  "version"}}}]}`, a platform-independent (`any`) entry listed under each platform. The
  style is the subcommand's whatever `--format` says (`$(bosun render args)` is piped, which
  would otherwise mean JSON); errors still follow `--format`. An entry with no block for the
  target is left out.
- **Target selection** applies the same `install` selectors and installer preference as the
  `install` command (§5.3, §5.4), for the requested target rather than the current machine.
- **`render json` covers multi-arch:** build args are the same for every platform in a
  multi-platform build, so where versions or package IDs differ per platform the Dockerfile
  looks them up by `TARGETPLATFORM` instead.
- **Versions come from this machine's lock** for this machine's platform (§4.5); a stale or
  missing lock is an error naming `bosun lock`. Another platform's versions are resolved
  through remote indexes when the backend has one (§4.7), else rendering for it is an error.
- **Another machine (DECIDED, built):** a target is another machine when its platform isn't
  this machine's, or when `--image`, `--rootfs`, `--distro` or `--libc` describes it (even on
  this platform: the flags say the machine isn't this one). It is resolved fresh, the newest
  version each spec allows, through each backend's remote index (§4.7); this machine's
  installed packages, package sources and lock are never read, and nothing of them is written
  (apt's index may run this machine's apt in a scratch root of its own, §4.7).
- **The target's facts, from its image (DECIDED, built):** `--image <ref>` copies
  `/etc/os-release`, `/usr/lib/os-release`, `/etc/apt` and `/usr/share/keyrings` out of the
  image for each target platform (`podman`, else `docker`: `create --pull=never --platform
  <target> <ref>`, `cp`, `rm`; the container is never started; a path the image lacks is left
  out; logged with the engine) into a temporary directory removed afterwards. The local image
  store is never changed: an image not held locally for the target platform is an error naming
  the pull that fetches it (`podman pull --platform linux/arm64 debian:12`); `--rootfs <dir>` reads the same
  files from a directory. The distro is its os-release's `ID`, `ID_LIKE`, `VERSION_ID` and
  `VERSION_CODENAME`, and apt reads its sources, pins and keys (§4.7). Either excludes
  `--distro` (and each other).
- **Or from the flags:** `--distro <id>[@<release>]` in selector form, the release spelled as
  the purl `distro` qualifier (`debian@bookworm`, `ubuntu@noble`, `fedora@fedora-41`), like
  only the families `--like <id>,…` names (its `ID_LIKE`: `--distro ubuntu@noble --like
  debian`; there is no built-in table, so without `--like` only the distro's own selectors
  match); apt then reads a stock image's sources. `--libc <gnu|musl>[@<version>]`.
  Without `--libc`, libc selectors don't match, and without a distro distro selectors don't;
  each is a warning when the manifest has such selectors, and an apt entry with no distro is an
  error saying to pass `--distro`. An entry whose acceptable backends have no remote index, or
  with nothing in its spec, is an error naming them. The installer preference is this machine's
  flag, `BOSUN_INSTALLER` and config, then the backends with a remote index in registration
  order (a backend's scope is known only from a probe).

Future driver: CI.

## 9. Capability / scope audit

### In scope — decided

- **Removal / prune / reconcile-to-absence — DECIDED: prune only what bosun installed.** A
  declarative manifest implies the inverse: install what's listed, and decide about what's *not*.
  bosun maintains a **managed set** (what it installed) and prunes/reconciles **only within that
  set** — it never removes "unlisted" packages it didn't install (that would uninstall the OS on
  apt/dnf).
- **Installing and pruning are separate commands — DECIDED.** `install` only moves the
  machine *toward* the manifest (install, upgrade, downgrade, hold); it never uninstalls.
  Removal happens only through `uninstall` (one manifest), `prune` (machine-wide orphans), or
  `remove <name>` (one entry). Two reasons:
  - The name: "sync" implies removing whatever doesn't conform, which bosun deliberately
    doesn't do; "install" says what the command does.
  - There is no single source of truth to sync to. Many manifests (every project, plus the
    user manifest) claim packages in the same install scope (§4.6), and one project's run sees
    only its own manifest. Whether a package is still wanted is a question about *all* claims,
    so it belongs to `prune`, which reads every claim in the machine state, drops claims whose
    manifest no longer exists, and uninstalls only `installed`-origin packages left with none.
- **`uninstall` — DECIDED: explicit removal of one manifest's contents.** `bosun uninstall`
  takes a directory (its `bosun.yaml`) or a path to the file,
  defaulting to the current directory, the same way `install` does.
  - It works from the machine state, not the file: it drops every claim recorded under that
    manifest's path, so it still works after the manifest has been edited, or deleted with its
    repo.
  - A package is uninstalled only if bosun installed it (`origin: installed`) and **no other
    manifest still claims it**. Packages other manifests still want keep running at their
    current version. `accepted` packages lose the claim and are never touched.
  - `uninstall` is the targeted counterpart to `prune`: run it before deleting a project; `prune`
    catches projects that disappeared without it.
- **Source / repo / tap / bucket / registry management — DECIDED: in scope.** Most managers need a
  source added before a pinned package exists (apt PPAs, dnf copr, brew tap, scoop bucket, winget
  sources, cargo alt-registries, npm scoped registries, go GOPROXY, Python extra-index-url).
  bosun gets its own **per-backend sources section** in the manifest, resolved *before* install.
- **Integrity / signature / trust — DECIDED: delegate to the underlying tooling.** bosun adds
  **no integrity/signature layer of its own**; it relies on each backend's native verification
  (apt signed repos/keyrings, dnf `gpgcheck`, brew/scoop manifest hashes, npm integrity, `go.sum`,
  Cargo.lock checksums). bosun drives the managers; the managers verify what they fetch.

### Settled (lower-priority, now decided)

- **Index refresh — DECIDED: in scope, EXPLICIT.** bosun exposes index refresh as an explicit
  operation (e.g. `bosun refresh`), **not** an implicit pre-resolve step run silently every
  time. `query_available` reads whatever index is current (`apt-get update`, `dnf makecache`,
  `brew update`, `winget source update`); the user refreshes deliberately. Keeps runs fast and
  predictable and makes "stale index" a visible, user-controlled state rather than hidden cost.
- **Install scope + PATH — DECIDED: delegate placement; warn where bosun picks the bin dir.**
  bosun does not manage install scope or location itself: it delegates to the installer /
  package manager's own default placement (`npm -g` prefix, `~/.cargo/bin`, pipx, system
  prefix, etc.). It checks PATH in two places, as warnings that change nothing: github-release,
  where bosun is the installer and chose `~/.local/bin`, warns when that dir is not on `PATH`
  (§12 item 1), and npm warns when the global prefix's bin dir is not on `PATH` (Appendix A).
  Other backends' "installed but not on PATH" stays the user's or the manager's concern.
- **State export — DECIDED: in scope.** bosun can seed a manifest from an existing host.
  High-value onboarding. Following the principle below, it reads each package manager's *own*
  record of what the user explicitly installed (e.g. `apt-mark showmanual`,
  `dnf repoquery --userinstalled`, `pacman -Qe`, `brew leaves --installed-on-request`,
  `flatpak list --app`), never the full installed set, and never infers which packages are
  dependencies itself. A backend with no such record gets no export.
- **Architecture / multi-arch — DECIDED: in scope.** Arch is a first-class resolution axis
  alongside version: multiarch (apt `:arm64`), rpm `.arch`, brew bottles per-arch, os/arch for
  binary fetchers — needed for cross-platform + arm64 container builds.

### Out of scope / shelved (named deliberately)

- **Transitive dependency closure — DECIDED: bosun deals only in what it was asked for.**
  bosun manages **top-level tools/binaries only**; dependencies are the package manager's job.
  (uv/npm/cargo/go lock full trees; apt/dnf/brew don't.) Concretely, bosun never enumerates a
  backend's full installed set: it queries the package IDs named in manifests and machine
  state, and nothing else. Dependencies, flatpak runtimes, and base snaps (`core*`, `snapd`)
  never appear in bosun's plan, state or prune. This avoids a whole class of metapac bugs
  (Appendix B) where a listing of "installed" packages included dependencies.
- **Reinstall / file-integrity verify** (`rpm -V`, `dpkg --verify`, `--reinstall`) — self-healing;
  skip unless wanted.
- **Offline / cache / mirror & transaction locks** — partly overlaps the fallback-artifact path;
  the dpkg/dnf transaction lock just means bosun serializes OS-PM ops.

## 10. Architecture linting (dev process — deferred)

"When we get there", once real named layers exist:

- **CI gate:** a Rust architecture/dependency-boundary linter (TBD — pin down the exact tool;
  there is a Rust equivalent of go-arch-lint). Baseline before that: Rust module visibility +
  `cargo-deny`/clippy dependency lints.
- **Optional ctxloom complement:** a structural-review agent (LLM design-judgment — connascence,
  boundary fit, over-engineering) that complements, not overlaps, the static gate.

## 11. Open questions

None open. Settled here last:

- **Name:** `bosun` stays. The crates.io name is taken, which doesn't matter while bosun ships
  only as release binaries (§3.1).
- **Remote indexes (§4.7):** built right after Linux breadth (release selectors, libc, dnf,
  apk), apt `Packages` first (built: §4.7, `render --target`, §8.3). `Containerfile.bosun` with
  versions decided ahead of the build and the ctxloom dev container dogfood (§12) build on it.
- **Signatures of apt release files (§4.7):** not verified by the remote index, and not
  warned about. HTTPS and the `InRelease` → package index hash chain are enough for choosing
  versions; the image's apt verifies the release against its keyring when it installs.
- **The rest of the ctxloom family (§12):** ltk, taskloom, harp and reprise stay out of the
  dogfood manifest, published or not.

## 12. Dogfood

The first real manifest manages bosun's own toolbelt: **just, jq and ctxloom**, on Debian 13
first, then macOS and Windows. The rest of the ctxloom family (ltk, taskloom, harp, reprise)
stays out (§11).

| tool | Linux (Debian) | macOS | Windows |
|---|---|---|---|
| `just` | apt `just` | brew `just` | winget / scoop (ID to verify) |
| `jq` | apt `jq` | brew `jq` | winget `jqlang.jq` (to verify) |
| ctxloom | GitHub release archive | brew tap `ctxloom/tap` | GitHub release archive |

**Verified 2026-10-14 against the public `ctxloom/ctxloom` releases (latest `v0.6.4`):** only
the `ctxloom` binary is published. Each OS/arch has `ctxloom_<ver>_<os>_<arch>.tar.gz` and a
`…_full.tar.gz` variant (Windows: `.zip`), plus `checksums.txt`, `install.sh` and
`install.ps1`. The archive holds `ctxloom` at its root, with LICENSE, README.md and man pages.
No pre-releases are published (`0.7.0-pre1` exists only as local dev builds). ltk, taskloom
and harp are not release assets; they stay out of the dogfood (§11).
(Earlier assumption, now known wrong: per-binary archives for all four.) The project's own Linux/Windows install path is a `curl | bash` script
over those archives. (`go install` from the module is possible but needs a Go toolchain and
builds rather than installs a release.)

What dogfooding forces into v1 (proposed):

1. **A `github-release` backend.** Previously only the rolling-backend fallback (§5.2, FUTURE.md);
   it is the main Linux/Windows path for the ctxloom family. Lists versions from the GitHub
   releases API (token for rate limits), picks the archive for the target OS/arch, verifies it
   against the checksum the release publishes (if any), extracts the binary, and records what it
   installed in machine state (user scope).
   - **Integrity exception to §9:** here bosun *is* the installer, so it cannot delegate
     verification. When the release publishes a sha256 for the asset (a per-asset
     `<asset>.sha256`/`.sha256sum`, else a combined sums file such as `checksums.txt`,
     `<name>_checksums.txt` or `SHA256SUMS`), bosun verifies the download against it and records
     it in the lock; a mismatch is an error. bosun doesn't audit releases, but it holds them
     still (DECIDED): when none is published, or the sums file doesn't list the asset, `lock`
     records the sha256 of the asset as it downloads then (trust on first use) and warns, and
     every later install of that version must match it. The backends that install a release's
     file as root or Administrator (github-deb, github-rpm, github-installer) refuse a release
     with no published checksum unless the entry sets `unverified: allow`.
   - **Install location exception to §9:** with no package manager to choose a prefix, bosun
     needs a default bin directory (e.g. `~/.local/bin`, `%LOCALAPPDATA%\bosun\bin`) and must
     say so when it isn't on PATH.
2. **Sources:** `ctxloom/tap` is a brew tap the §9 sources section must add before install.
3. **Pre-release versions:** the family is at `0.7.0-pre1`; ordering (§6) must place it below
   `0.7.0`, and selection never picks a pre-release unless the spec names one (§5.6).
4. **Families sharing one release:** four entries that must move together. Either the manifest
   gets a way to share one `spec` across entries, or `lock` warns when they diverge (open, §11).
5. **Shadowing installs:** this machine already has the family as local dev builds in
   `~/go/bin` (version strings like `v0.7.0-…-dirty`, matching no release) and just/jq from apt.
   apt's are `accepted` (§4.6). A bosun-installed release binary that a dev build shadows on
   PATH is outside bosun's checks today (PATH is delegated); dogfood will show whether `status`
   needs to report it.

**Next dogfood candidate (proposed): ctxloom's `.devcontainer/tool-versions.env`.** It is a
hand-built bosun: one file pinning ~20 build tools (Go, just, buf, golangci-lint, goreleaser,
gitleaks, node, …) with per-arch sha256 pins, consumed as devcontainer `--build-arg`s, loaded
into `$GITHUB_ENV` by workflows, used by a Windows CI job and a host install, and guarded by
drift tests. Replacing it with `bosun.yaml` + `bosun render` exercises the container driver
(§8), multi-arch checksums, and CI consumers on a real project. Its per-arch pins are for
platforms other than the machine rendering them, so with the lock per machine (§4.5) this
waits on remote indexes (§4.7).

## 13. Implementation status (recovery log)

Kept current by the orchestrating session so a crashed run can be resumed. The M1 slice plan
lives in the implementing agent's plan file (`bosun-m1.plan.md`, slices 0–13); this section is
the authoritative record of what has landed on `main` and what was decided along the way.

**Goal (M1):** enough of bosun to dogfood on a Debian host (§12): apt, cargo and
github-release backends; `backends`, `install`, `uninstall`, `prune`, `status`, `lock`.
Out of M1: other backends, `render`, `export`, `upgrade`, `adopt`, macOS/Windows machines,
remote indexes. Traits stay ready; nothing is stubbed.

**Decisions made during implementation:**

- annals is pinned by git rev `4f92f43`, the first with `annals_yaml::edit`, which `add`
  and `remove` use to splice one entry.
- `rust-toolchain.toml` pins **1.96.1**, matching the host and the agent image; the workspace
  `rust-version` is 1.96.1 too (cargo refuses to build under a lower toolchain).
- Mutation testing inside agent containers runs `--in-place` on a disposable copy of the tree
  with `TMPDIR` off `/tmp`: the shared `.git` is read-only there, so `git worktree add` fails.
- The dogfood `bosun.yaml` targets plain `linux:debian`, not a specific release, so it
  resolves only on Debian 13 (Debian 12 and Ubuntu 22.04 have no `just`); release
  selectors (§5.4) are the fix.
- ~~Range grammar is per version scheme (semver `VersionReq` / AND-ed comparators)~~ —
  superseded by vers (§5.6), slice 4b.
- **No policies (user decision):** tolerant, exact/range modes and `deviation` are removed;
  vers ranges cover them (§5.6, §7).
- **purl + vers (user decision, §5.6):** package identity is purl; every spec is vers;
  selection is "highest available in the vers set". This supersedes the per-scheme range grammar recorded below (no `^`, `~`,
  `1.2.*` in M1).
- **The tool's view (user decision, 2026-10-17):** no rule that duplicates, and could fall
  out of sync with, what apt, dnf, npm or uv decide. bosun keeps vers as the stored spec and
  the intersection across manifests; within a spec the package manager chooses (§5.6), npm
  and PyPI ranges are evaluated by their ecosystems' own rules, and each tool's configured
  registry is honoured on this machine (remote indexes keep the public ones).
- Error and log text lives in `errmsg` / `logmsg` constants; data-carrying thiserror
  variants use a named binding (`#[error("{kind}: {0}", kind = errmsg::X)]`).

- **Lock and state share one generic type** (§4.6): `Package<Meta, Leaf>`, with
  `LockPackage = Package<Spec, LockLeaf>` and `StatePackage = Package<(), StateLeaf>`; the
  common backend/package/source sits in a `Placement` embedded in both leaves. `PackageId`
  carries an optional opaque `kind` (brew cask, scoop bucket). The JSON wire format (field
  renames, key order) is pinned in bosun-io, not in bosun-core.
- `Driver` is defined in slice 9 with the plan type; `BackendFactory::build` returns both
  halves (`Backend { index, installer }`) because resolve needs the query side too. Process
  and HTTP ports (with fakes) live in bosun-ports; a non-zero exit is data, not an error.
- **purl and vers are implemented in bosun-core**, not taken from crates: `purl` 0.1.6 fails
  the official suites for deb/github/generic (unsupported types), and `vers-rs` / `unive_rs`
  carry their own version comparison rather than bosun's §6 ordering. The official suites are
  vendored under `crates/bosun-core/tests/conformance/` (pinned revs in its README). The vers
  suite covers only the registered types (npm, pypi), so deb containment is covered by
  bosun's own unit tests.
- Backends work in **native IDs** (`Ref`/`Installed` in bosun-ports carry the backend's own
  package ID); bosun builds the canonical purl only for what it records (`Placement =
  {backend, purl}` in lock and state). `native_id()`: github → `namespace/name`, else `name`.
- **State locking:** a mutating run holds an exclusive lock on the state file from load to
  save (the lock spans the plan and apply), using `std::fs::File::lock`; writes are atomic
  (temp file, fsync, rename). A save-only lock would lose claims when two runs interleave.
- **Backends describe, executors act (user decision, §5.1):** mutating `Installer` methods
  return step lists (`Run` / `Fetch` / `Extract`). The host executor runs them, and the
  container renderer turns them into `Containerfile.bosun` (§8.2). **Binding on slice 7**; the
  ports trait change goes in with it. Rendering itself (`render`, `Containerfile.bosun`) stays
  after M1; render args/env and the bosun layer are both supported shapes.
- The manifest model lives in bosun-core; its loader, `ManifestError` (an annals
  `Diagnostic`) and `manifest_hash` (sha256 of a canonical serialization of the parsed model)
  live in bosun-io. Specs stay raw until resolve, which parses them as vers using the
  selected backend's purl type. The M1 manifest schema has no `sources` or `adopt` keys (an
  unknown key is an error); both come back with the features that need them.
- `StateStore` is transactional: `begin` (exclusive lock + load, held across plan and apply) /
  `try_begin` / `read` (shared lock) and `StateTransaction::commit` (atomic write).
- Probing is parallel (`std::thread::scope`); `CommandRunner` and `BackendFactory` are
  `Send + Sync`, while built backends need not be. `LinuxMachine` takes its state paths
  injected (the CLI supplies them from bosun-io). geteuid comes via `rustix`; no `unsafe`.
- Every slice's gate is literally `cargo clippy --workspace --all-targets -- -D warnings`
  plus `cargo test --workspace`, re-run by the orchestrator before landing.
- Steps: `Run { argv, env, scope }` (argv is `Vec<String>`; `Command` carries `env`, e.g.
  `DEBIAN_FRONTEND=noninteractive`), `Fetch`, `Extract`. Mutating `Installer` methods return
  `Steps`; the installed version is read back with `verify_installed` after execution, and
  state is written only from that. The `StepExecutor` lives in bosun-host.
- `verify_installed` returns `Installed::{Absent, Present { version: Option<Version> }}`; there
  is never a placeholder version. github-release reports `Present { version: None }`, so apply
  records the resolved version and state is the authority for it.
- `Ref` carries the canonical purl. github-release identity uses purl qualifiers `binary`
  (default: the entry's logical name, set by resolve) and an optional `asset` template, from
  the github-release `asset` option (see `docs/asset-templates.md`). The asset template is a
  minijinja template with strict undefined over precomputed variables (`binary`, `version`,
  `tag`, `os`, `arch`, `arch_uname`, `arch_goreleaser`); the default is goreleaser's
  `{{ binary }}_{{ version }}_{{ os }}_{{ arch }}.tar.gz`, matched exactly. A bad template is
  an error at resolve, through `Index::checksum`. The binary is located in the archive by file
  name at any depth (exactly one match). A published checksum is verified; none published means
  install, warn and record none. An optional `tag` option (`backends.github-release.tag`) is a
  literal tag pattern with one `{{ version }}` (`jq-{{ version }}`): release lookup uses it
  instead of `v<version>`/`<version>`, and version listing keeps only tags that fit it.
- Resolve takes a narrow `Host { os, arch, distro }` and `ResolvableBackend { id, purl_type,
  vers_type, platform_independent, index }` values built by the CLI from the registry, and
  the manifest hash passed in, so bosun-resolve depends on neither bosun-host nor bosun-io.
  Install blocks hold an ordered `backends` list (the YAML accepts a scalar or a sequence);
  manifest version 2 replaced it with an unordered set (§5.3).
  The deb `distro` qualifier is os-release `VERSION_CODENAME` (since TV-10, the base
  distro's `UBUNTU_CODENAME` / `DEBIAN_CODENAME` where set).
- An empty claim intersection is one refusal (listing each manifest's spec and what's
  available) whether the specs are disjoint or nothing published fits; telling those apart
  needs vers interval algebra, deferred. Reporting lockfiles a plain `install` doesn't match is
  the command layer's job.
- **Plan exit code counts package changes only** (install / upgrade / downgrade / hold /
  remove). Claim bookkeeping (`accept`, `claim`, `drop-claim`) is shown and applied but exits 0, so
  `install --plan` stays a drift check on a fresh CI machine whose tools are already present.
- **Unsupported capability degrades** (§5.2): a hold the backend can't do is a warning and is
  skipped; a downgrade it can't do is an error diagnostic for that package, not a failure of
  the whole plan.
- Holds follow the spec (§5.2): a `hold` follows a version change under an exact spec, or pins
  a package an exact spec newly asks for; an `unhold` releases a hold state records once no
  spec is exact. A fully-applied manifest plans to nothing.
- **State is written after every confirmed action**, not only at the end of the run, with the
  lock held throughout; a crash mid-apply never leaves a bosun-installed package unrecorded
  (it would otherwise look `accepted` and never be pruned). The applier depends on a
  `StepRunner` port, implemented by bosun-host's `StepExecutor`.
- **Backend metadata lives with the backend**: each `BackendFactory` declares its purl type,
  vers type and platform independence, so adding a backend never touches the CLI (§5.1).
- The CLI's integration tests drive the real binary through a test seam (`BOSUN_FIXTURE` +
  `BOSUN_STATE_DIR`). The seam needs both variables, never touches real state paths, and
  warns on stderr on every run while it's active.
- Hold support is three-valued (§5.2): apt `Supported` (hold action), cargo and github-release
  `NotNeeded` (silent: cargo re-asserts each run; bosun owns github-release binaries),
  `Unsupported` warns.
- **A version's scheme comes from the backend's declared vers type** (`BackendFactory`
  metadata), never from the purl type: github-release's purl type is `github`, but its tags
  compare as SemVer. That applies to lock `resolved`, state `installed` and every spec.
  Crossing schemes was dogfood bug 1.
- `Index::checksum` (defaulted to none) lets the lock record a backend-reported checksum;
  github-release returns the release's published sha256 for the asset (none when it publishes
  none), the same value its Fetch step enforces.
- `Arch` is amd64/arm64 only; `Os` uses Docker's spelling (`darwin`).

**M1 status: COMPLETE (2026-10-14).** `bosun backends | status | lock [--check] | install |
uninstall | prune` with apt, cargo and github-release on Linux; purl identity and vers ranges;
plan/privilege/apply with per-action crash-safe state; 303 hermetic tests and 20 cucumber
scenarios, plus a real-backend tier (`just acceptance-real`, container only); coverage 90.6%
lines; every per-crate mutation target met. The dogfood `bosun.yaml` and `bosun.lock` are
committed; the real host install is the user's to run (`bosun install`, then
`sudo bosun install --scope system`).

**Since M1** (the M2 and M3 slices below): manifest version 2, the per-machine lock, `add` /
`remove` / `migrate` / `upgrade` / `export`, more github release kinds, Windows (hermetic only),
apply without a terminal, claims in `status`, Linux breadth: release selectors and libc
(§5.4), dnf and apk, npm and uv, and `render` (§8.3) with apt's remote index (§4.7). `just test` runs 845
tests and 192 scenarios.

**Next, in order.**

1. **Containers:** `Containerfile.bosun` (§8.2) on `render`'s resolution; dnf's and apk's
   remote indexes. (npm and uv are built, with their remote indexes: M3-7.)
2. **macOS, and Windows on a real host:** the macOS machine, brew (research first), and
   verifying scoop, winget, choco and github-installer on Windows.

Other backends (pacman, zypper, snap, flatpak, `go install`, gem) wait until wanted.

**Still deferred:** `adopt`, `sources`, `formerly` migration; persisted plans; distinct
messages for disjoint-vs-unpublished claim intersections (needs vers interval algebra); a cargo
scenario in the real-backend tier (skipped for compile time); answering "n" at the prompt runs
under a pty on Unix only.

**Slice status:**

| slice | what | state |
|---|---|---|
| 0 | workspace skeleton, justfile, bacon.toml, LICENSE | landed |
| 1 | bosun-core version model (§6) | landed; 24 tests, mutation 25/25 viable caught |
| 2 | bosun-core Spec/Policy, exact/range/tolerant (§7) | landed; 46 tests, mutation 17/17 viable caught |
| 3 | bosun-core platform/backend/record value types (§4.5, §4.6, §5.4) | landed; 64 tests, mutation 18/18 viable caught |
| 4 | bosun-ports traits (§5.1, §5.3, §5.5) + bosun-core Distro | landed; 71 tests, mutation 28/28 viable caught |
| 2b | tolerant-with-range on the pre-vers wildcard grammar | superseded; its commit sits on main beneath 4b-1, which removes it |
| 4b-1 | vers ranges + single-rule selection; Policy/Deviation/tolerant removed (§5.6) | landed (with 2b's commit beneath it); 71 tests incl. vers conformance (npm canonical-parse + containment), mutation 53/53 viable caught |
| 4b-2 | purl identity: conformant purl module; PackageId → unversioned purl in lock/state | landed; 87 tests; purl conformance 65/65 required (spec, deb, cargo, github, generic), 98/107 other types best-effort; mutation 72/72 viable caught |
| 5-1 | bosun-io: lockfile + state JSON, FileStateStore, user config | landed; golden + round-trip tests; mutation deferred to 13 |
| 5-2 | bosun-io: manifest + manifestHash; transactional StateStore on std file locks | landed; 106 workspace tests; hash proven comment-insensitive |
| 6 | bosun-host: LinuxMachine/FakeMachine, os-release, privilege, probe registry | landed; 115 workspace tests; mutation os_release+machine 21/22 (miss: host-I/O `detect()` wrapper) |
| 7-1 | step-list Installer contract, host StepExecutor, apt backend (§5.1) | landed; 136 tests; mutation apt+executor 33/33 viable caught |
| 7-2 | executor hardening (atomic temp→fsync→rename writes; regular-file members, `./name` ≡ `name`) + cargo backend (sparse-index listing) | landed; 147 tests; mutation cargo 22/24 (2 equivalent) |
| 7-3 | github-release backend (goreleaser picker, enforced checksums.txt, `~/.local/bin`) | landed together with 7-4 |
| 7-4 | fix-up: no sentinel version in `verify_installed` (Absent / Present{version?}); `binary` purl qualifier; real v0.6.4 fixtures, exact goreleaser asset match | landed; 163 tests; mutation github_release+model 36/38 (2 equivalent) |
| 8-1 | bosun-resolve: single-manifest resolve (selectors, installer preference, purl building, highest-in-set, `any` target) | landed; 182 tests; mutation 14/14 viable caught |
| 8-2 | multi-claim intersection, lock merge per target, `--frozen`, `lock --check` (pure functions) | landed; 193 tests; mutation 31/31 viable caught |
| 9-1 | bosun-apply: plan builder (§4.8) + privilege check (§5.5); JSON via one-way DTO views | landed; 216 tests; mutation 48/48 viable caught |
| 9-2 | Driver + host applier (one transaction, per-backend isolation, verify-before-record); exit code and capability fixes | landed; 226 tests; mutation 64/64 viable caught |
| 10-1 | per-action state checkpoints + ApplyReport JSON; real adapters (std::process, ureq/rustls); wiring; `backends` / `status` / `lock [--check]`; `--format` | landed; 245 tests |
| 10-2 | `install` / `uninstall` / `prune` + confirmation; backend metadata on the factory; fixture-seam guard | landed; 258 tests |
| 10-3 | blockers from the first real-host smoke run (below) + cross-claim selection in `install` | landed; 265 tests; host re-run: jq → `accept`, invalid spec → parse error + exit 1, ctxloom 0.6.4 planned |
| 11-1 | acceptance seam tier: cucumber-rs over the real binary, 4 features / 20 scenarios, under `cargo test --workspace`; hold support `Supported / Unsupported / NotNeeded` | landed; 266 tests + 20 scenarios |
| 11-2 | acceptance real-backend tier in a disposable container (`just acceptance-real`; guarded by `BOSUN_ACCEPTANCE_REAL=1` + a container marker) | landed; real run 2/2 scenarios (apt hello install → hold → prune; github-release ctxloom 0.6.4 checksum-verified); found 2 bugs (below) |
| 13 | quality: hermetic regressions for the 11-2 bugs, workspace mutation run, coverage ≥ 90% | landed; 284 tests; coverage 90.60% lines / 90.32% regions (re-measured by orchestrator); every mutation target met, survivors justified |
| 13b | mutation for bosun-io / bosun-ports | landed; 290 tests; io 31/31 and ports 11/11 viable caught (6 survivors killed with new tests) |
| 12-1 | fixes for the 5 dogfood bugs (below) | landed; 296 tests; host re-run clean; `bosun.lock` committed (ctxloom checksum recorded) |
| 12-2 | installed-version scheme from backend metadata (state + lock); idempotency test per backend | landed; 299 tests; github-release idempotency test confirmed red without the fix |
| 12-3 | scheme map from every compiled-in backend; unknown backend id in lock/state is an error, not Generic | landed; 303 tests + 20 scenarios |
| 12 | dogfood: `bosun.yaml` (just, jq, ctxloom) committed; plan-only host run (user decision) | manifest committed; host run found 5 bugs (below) → 12-1 |
| M2-0 | backend-specific settings under `ext:` (`BackendMeta::ext_keys`, checked at load); github-release `asset` moves there | landed; superseded by M2-4 |
| M2-1 | annals-yaml `edit` (annals `4f92f43`); `bosun add` / `bosun remove` (edit, then `lock`); default spec from the newest version; `lock` drops packages the manifest no longer lists on every target | landed; 50 scenarios; annals edit mutation 197/200 viable caught (3 justified) |
| M2-2 | github-release archive formats (tar gz/xz/zst/bz2, zip, single compressed, raw); `tag` template | landed |
| M2-3 | github-deb / github-rpm: a release's `.deb` / `.rpm` through apt-get / dnf, system scope, cache `/var/cache/bosun`; presence verified with dpkg-query / rpm -q; apt holds, dnf doesn't claim one | landed hermetically; real apt/dnf run not done (github-rpm unverified: no Fedora host) |
| M2-4 | manifest version 2: `backends` map (acceptable set + options, `package` per backend), no order and no `targets`; `any` selector and bare-block shorthand; 1 → 2 migration; config `version` (0 → 1); `bosun migrate`; `lock --check` removed | landed |
| M2-5 | preference is the machine's: flag > `BOSUN_INSTALLER` > config > built in (user scope before system, then registration order); a config that fails to load is an error | landed |
| M2-6 | the lock is the machine's own: version 2, one per manifest in the user state dir (`locks/<sha256 of path>.json`), records its manifest and hash; no cross-target merge; `install --frozen` refuses a missing or stale lock; an older `bosun.lock` beside the manifest is warned about and not read; the repo's committed `bosun.lock` deleted | landed |
| M2-7 | a locked version stays until it's upgraded: `lock` / `add` / `remove` / `install` keep pins that still fit; `bosun upgrade [pkg…]`; plain `install` prefers the pin across claims and writes the lock after a successful apply | landed |
| M2-8 | release checksums held on first use: no published checksum → `lock` records the download's sha256 and every later install verifies against the lock's checksum; github-deb/rpm/installer refuse an unpublished checksum without `unverified: allow` | landed |
| M2-9 | shared tools: a new entry's candidate pin is what bosun already installed (any project), kept when it fits, so `install` doesn't bump a tool another project installed; `upgrade` moves it | landed |
| M3-1 | WindowsMachine (Administrator = High/System integrity label in `whoami /groups`; state `%ProgramData%` / `%LOCALAPPDATA%`, config `%APPDATA%`); github-release on Windows (`<binary>.exe` into `%USERPROFILE%\.local\bin`, `;`-separated case-insensitive PATH check); `Step::Remove` replaces `rm -f`; `just check-win` (zig cc as MinGW) | landed hermetically; workspace type-checks for x86_64-pc-windows-gnu; **unverified on a real Windows host** |
| M3-2 | winget (user), scoop (user), choco (system) backends; probe only on Windows | landed hermetically (argv and output parsing unit-tested, fixture scenarios); **unverified on a real Windows host** |
| M3-3 | github-installer: a release's `.msi` (`msiexec /i … /qn /norestart`) or `.exe` (manifest `silent_args`, required); found in the Uninstall registry keys (`reg query`, three views) by `display_name` (`*` suffix = prefix); uninstall `msiexec /x {ProductCode}` or the entry's quiet/plain uninstaller; msiexec's 3010 (reboot required) is success, reported; system scope, no hold, no downgrade | landed (unit tests); **unverified on a real Windows host**; no fixture scenario (the seam has no registry) |
| M3-4 | `bosun render args\|env\|json [path]` for this machine, from its lock (missing or stale: an error naming `bosun lock`); variable stems from the logical name or the entry's `var` (manifest key, checked at load), clashes and digit-led stems refused; the build-args driver is `bosun_apply::render` | landed |
| M3-5 | apt's remote index (§4.7): a release's suites from its mirror over HTTPS (Debian and Ubuntu defaults, as stock images list them), `InRelease` → package index hash chain (by hash), signature not verified (decided: HTTPS and the hash chain; the image's apt verifies at install), cached under the user cache dir (`InRelease` an hour, indexes while their hash holds); `BackendFactory::remote_index` (apt, cargo, github-release, github-deb, github-rpm) | landed; unit tests over trimmed real Debian 12 captures; same candidates as `apt-cache policy` in `debian:12`, `debian:13` and `ubuntu:24.04` |
| M3-6 | `render args\|env --target <platform>`, `render json --targets a,b`, `--distro <id>[@<release>]`, `--libc <family>[@<version>]`: another machine is resolved fresh through remote indexes with a runner that runs nothing; a backend without one is an error naming it; missing distro or libc facts are warned about | landed |
| M3-7 | npm and uv backends (user scope, platform-independent, no hold): npm registry abbreviated metadata / `npm install -g --prefix` under npm's own global prefix (`npm prefix -g`; warned when not writable or its bin dir is off `PATH`) / `npm ls -g --json`; PyPI JSON API / `uv tool install name==ver` / `uv tool list`; both offer their registry as their remote index (`render --target`); export from each; PEP 440 ordering (`pep440_rs`, vers `pypi`, conformance suites run); `npm` and `pypi` purls must-pass | landed; real tier in `node:22-slim` with uv 0.13.1 (semver 7.6.0 → 7.5.0 → pruned under `~/.npm-global`; the system prefix fails, warned; ruff 0.6.9 → 0.6.8 → pruned) |
| TV-1 | `Index::select` (the package manager chooses within a `Want`, every claim's vers set; lock preferred while still offered); apt by `apt-cache policy` priorities (`LC_ALL=C`), replacing `madison` | landed; real tier in `debian:13` (a local repo pinned at 100 and at -1); fixtures from `debian:12` + bookworm-backports, pin files, `ubuntu:24.04` phased updates |
| TV-2 | dnf by repository priority: `repoquery` with `%{repoid}`, priorities from `dnf --dump-repo-config='*'` (dnf5) or the dnf Python API (dnf4) | landed; real tier in `fedora:41` (fedora at priority 1: wget2 2.1.0, then 2.2.0 for `>=2.2`) and `rockylinux:9` (the 9.5 vault at priority 1: unzip 6.0-58.el9_5, then 6.0-60.el9_8) |
| TV-3 | npm and PyPI ranges evaluated natively (`nodejs-semver` 7.0.0, `pep440_rs`); npm resolves locally with `npm pack --dry-run --json`, uv with `uv pip compile -` (stdin, new on `Command`) for `uv python find --system`; uv's if-necessary pre-release rule | landed; real tier in `node:22-bookworm-slim` (yarn `*` → 1.22.22, the `latest` tag, though 2.4.3 exists) and `python:3.9-slim` with uv 0.13.1 (black `*` → 25.11.0, the newest for Python 3.9) |
| TV-4 | configured registries on this machine: npm `npm config get registry` / `@scope:registry`; cargo source replacement from its config files (cwd ancestors, `$CARGO_HOME`; `toml` 1.1.6), a git, local or vendored source an error; uv's index left to uv (no PyPI listing locally) | landed (unit tests) |
| TV-5 | npm prefix writability by `access(2)` (no probe file), warned once when an install or uninstall step is planned | landed; real tier: root-installed semver accepted with no warning |
| M3-8 | render reads the target from its image: `--image <ref>` (podman, else docker: create / cp / rm from the local image, never pulled or run; missing for the platform, an error naming the pull) or `--rootfs <dir>` give os-release (`ID`, `ID_LIKE`, release) and apt sources (one-line and deb822, `deb` only, `Enabled: no` dropped, `arch=` honoured, flat repositories, vendor repositories included); stock sources only without them, logged; apt resolves with this machine's apt in a scratch root (`APT_CONFIG`, empty status, the target's arch, its pins and keys; no key: `Trusted: yes` over HTTPS in the scratch config only), bosun's own reading (apt's default priorities, chosen within the spec as local apt chooses, pins warned of) without it; the `--distro` `ID_LIKE` table dropped for `--like` | landed; `--image` renders match `apt-cache policy` inside `debian:12`, `debian:13`, `ubuntu:22.04`, `ubuntu:24.04` (30 packages each), arm64 from amd64, and a derived `debian:12` with Docker's repository and a pin |
| M2-10 | with no terminal, every confirm (install, uninstall, prune, remove, migrate) applies, still printing what it applies to stderr; `--yes` only skips a terminal's prompt | landed |
| M2-11 | `status [path]` lists every claim on each package (manifest, spec, user, whether the manifest still exists); with no path, everything bosun manages on the machine; `install` records a `claim` on a package bosun installed for another project | landed |
| M2-12 | release selectors `linux:<distro>@<release>`: the release is the purl `distro` value a block's backend gives this machine (deb codename, rpm `<ID>-<VERSION_ID>`); exact `ID` only | landed; checked in Debian 12/13 and Ubuntu 22.04/24.04 containers |
| M2-13 | libc: family from `/bin/sh`'s ELF interpreter, version from running it; `linux:gnu` / `linux:musl[@<vers generic range>]` ranked between `ID_LIKE` and `<os>`; `{{ libc }}` / `{{ rust_target }}` templates; shown in `backends` / `status` | landed; checked in Debian, Ubuntu and Alpine containers |
| M2-14 | dnf backend (dnf4 and dnf5): repoquery / `rpm -q`, versionlock hold (unheld with a warning when dnf4 lacks the plugin); RPM ordering (rpmvercmp) | landed; real tier in Fedora 41 and Rocky 9 |
| M2-15 | apk backend: `apk policy` index, installs pinned in the world file, `pkg:apk` identity with no `distro`; Alpine ordering (apk-tools 3) | landed; real tier in Alpine 3.20/3.21/3.23 (`just acceptance-real-musl`) |
| M2-16 | `bosun export`: per backend, what the user installed by hand (apt manual minus base priorities, `cargo install --list`, apk world, bosun-installed releases), merged into the manifest, then `lock` | landed |
| M2-17 | acceptance seam: absent backends, github-release planning into `~/.local/bin`, the confirmation prompt under a pty; a backend whose program is missing is "not present" | landed |
| V-1 | OS versions have no pre-releases: Debian, RPM and Alpine `is_prerelease` is false (the tagged-`~` rule is gone); SemVer, PEP 440 and generic keep their own (§5.6) | landed |
| V-2 | `add` / `export` write `*` for apt, dnf and apk (the `>=U\|<U'~` and `<U'_alpha` ceilings are gone); cargo/npm caret, PyPI `>=X`; github-deb/github-rpm (SemVer tags) caret | landed |
| V-3 | hold only exact specs: `Installer::unhold`; state records `held` (absent: installed reads held); the plan unholds bosun's hold once no spec is exact (before any version change) and holds a package an exact spec newly pins; apk installs `pkg=ver` then the bare name, so a range leaves the world unpinned | landed; real tier: apt hold → `*` → unheld → `apt-get --only-upgrade` → bosun leaves 2.0-1 (Debian 12/13); dnf `*` unlocked → exact locked → range unlocked (Rocky 9, Fedora 41); apk world unpinned → pinned → unpinned (Alpine 3.20, 3.23) |
| V-4 | machine architecture from `uname -m` through the runner (Linux/macOS) or `PROCESSOR_ARCHITEW6432` / `PROCESSOR_ARCHITECTURE` (Windows), not bosun's build target (§5.5) | landed; unit tests on `x86_64`, `aarch64`, `arm64`, `armv7l`, `i686` |
| V-5 | apk ordering and validity by the probed apk-tools major (2 vs 3); real-tier ordering oracle: each index's versions sorted by bosun, every adjacent pair confirmed by `dpkg --compare-versions` / `rpm.vercmp` / `apk version -t`, plus conformance pairs | landed; 0 disagreements: Debian 13 23,276 pairs, Debian 12 21,410, Fedora 41 22,394, Rocky 9 2,800, Alpine 3.20 (apk-tools 2.14.4) 4,921, Alpine 3.23 (3.0.8) 5,282 |
| TV-6 | system state without elevation: `read` takes its shared lock through the lock file opened read-only; state the user may not read (`PortError::StateDenied`) is left out of a run without elevation, its packages skipped, warned once, never locked or written; elevated runs fail on it as before | landed; real tier in `debian:13`: elevated install, then a non-root `prune` / `status` read it; with the state 0600 a non-root `prune` warns and leaves it untouched |
| TV-7 | dnf never goes back past the installed build: `select` asks `rpm -q`, ranks builds behind it last, and keeps it, admitted, when no repository offers it | landed; real tier in `fedora:41` (wget2 2.2.0 kept with fedora's 2.1.0 at priority 1 after `upgrade`) and `rockylinux:9` (unzip 6.0-60.el9_8 kept over the 9.5 vault's 6.0-58.el9_5 at priority 1); red without the installed build: a downgrade was planned |
| TV-8 | uv resolves with the configuration `uv tool install` reads: `uv python find` and `uv pip compile` run as `uv --directory / --project /`, so a project's `uv.toml`, `[tool.uv]`, `.python-version` or `UV_PROJECT` where bosun runs is not read | landed; real tier in `python:3.12-slim` with uv 0.13.1: a `uv.toml` in the project naming an unreachable index, ruff `>=0.6.0\|<0.7.0` → 0.6.9; red without it (`Connection refused`) |
| TV-9 | state follows the package manager: planning and `status` take each recorded package's version from `verify_installed`; the applier records it at the start of every state transaction; a move within the spec ends there (the lock follows), never back; a move outside it is warned and planned as any | landed; real tier in `debian:13`: bosun-probe 1.0-1 under `*`, `apt-get --only-upgrade install` → 2.0-1, `status` shows `2.0-1 (up to date)`, `install --plan` plans nothing |
| TV-10 | a deb release is the codename apt reads: `UBUNTU_CODENAME`, else `DEBIAN_CODENAME`, else `VERSION_CODENAME` (`Distro::base_codename`); `linux:linuxmint@wilma` names nothing, `@noble` matches Mint 22 | landed; os-release of LMDE 6, elementary OS 7, Debian 13, Kali captured from images; Mint 22, Pop!_OS 24.04, elementary OS 8 from known contents (no faithful image) |
| TV-11 | apt scratch roots pruned by age: `used` stamped on each write, `updated` on each successful update; `prune_scratch` removes roots idle for `SCRATCH_MAX_AGE` (30 days) once a run, when the remote index first reads through apt | landed (unit tests with file times set back) |
| TV-12 | `render --image` never changes the local image store: `create --pull=never --platform <target>`; an image missing locally or held for another platform is an error naming `<engine> pull --platform <target> <ref>` | landed; podman 5.4 with amd64-only `debian:12`: `--target linux/arm64` errors with the pull, amd64 renders, the store unchanged; docker 29 refuses a platform mismatch under `--pull=never` the same way |
| TV-13 | without elevation, system-scope `accept`, `claim` and `drop-claim` are left out of `install`, `uninstall` and `prune` as `deferred` notes (`defer_system_records`), reported by apply, so user-scope actions in the same plan apply; `remove <name>` keeps needing elevation for them | landed; scenarios: a mixed apt/cargo manifest with root's system state installs ripgrep, defers claim jq and accept tree, and leaves the system state unchanged; a non-elevated prune defers its drop-claim |
| TV-14 | the lock follows what was installed: when every claim's intersection ends a package at a version other than this project's pin, the pending lock pins it and is written after a successful apply | landed; scenario: lock pins ripgrep 14.1.1, another project's `<14.1.0` claim upgrades it to 14.0.3, the lock then pins 14.0.3 and `install --frozen --plan` plans nothing; red before: the lock kept 14.1.1 |
| TV-15 | apt keeps the installed version no source offers any more: `Policy::choose` admits the installed version (dpkg status, priority 100) as a candidate when the spec admits it, as apt's candidate stays there below priority 1000 | landed; real tier in `debian:13` (apt 3.0.3): bosun-probe 2.0-1 installed, dropped from the local file repository, `upgrade` under `*`, `install` keeps 2.0-1; red without it: a downgrade to 1.0-1 was applied |

**First real-host smoke run (2026-10-14, release build, Debian 13, read-only `--plan` and
`lock` in a scratch dir).** `backends` found apt 3.0.3, cargo 1.96.1 and github-release; jq
resolved to `pkg:deb/debian/jq?arch=amd64&distro=trixie`. Found three blockers, all invisible
to the fake-backed tests:

1. A package already on the machine but not in bosun state was planned as `install` + `hold`
   (it would have upgraded and held the user's jq) instead of `accept` (§4.6).
2. A constraint version invalid in its scheme (`>=0.6` for SemVer) silently matched nothing
   instead of failing at parse.
3. `lock` exited 0 and wrote a partial lock with an unsatisfiable entry.

**Real-backend run (11-2, in the agent container: root, Debian 12).** Found two bugs the fake
seam couldn't: apt `uninstall` left a lingering `apt-mark hold` (now unholds before removing),
and a user-scope `install` opened and locked root-owned system state (now each scope's state is
read only when needed). Cargo was skipped for compile time.

**Dogfood host run (2026-10-14, Debian 13, plan-only per user decision).** `lock` resolved just
1.40.0-1+b1, jq 1.7.1-6+deb13u2 and ctxloom 0.6.4. `install --plan` gives `install ctxloom`,
`accept just`, `accept jq` (exit 2), and no state was written. Five bugs:

1. `install --frozen` refuses ctxloom: "0.6.4 excluded by `vers:semver/>=0.6.4|<0.7.0`",
   though 0.6.4 is in the range.
2. Claims record the manifest as the relative `./bosun.yaml`; §4.6 needs the canonical
   absolute path.
3. The lockfile carries no `checksum` for github-release (§12 says it's enforced), though the
   plan's Fetch step carries the right sha256.
4. `status` reports present-but-unmanaged packages (jq, just) as "not installed".
5. `--frozen` with an accepted (not bosun-owned) package installed at a different version
   than the lock (jq u1 vs locked u2) needs a defined outcome: an error diagnostic naming
   both versions, with no change.

User decisions: ltk, taskloom and harp stay out of M1; the host dogfood is plan-only (the
user runs the real install). `bosun.lock` is committed once bug 3 is fixed.

**Active implementor:** none (M1 done). Last: `finer-urban-brunt` (stopped after 12-3). Previous: `lame-wide-chant`, which can't be resumed (ctxloom compares worktree branch `refs/heads/…` with a short name; see the alpha-issues write-up); all its work is on main, and its worktree is kept as evidence. Before it: `lame-wide-chant` (worktree branch `ctxloom/lame-wide-chant`), one
slice per turn, reporting to the orchestrator after each. Previous: `hoary-quick-elm` (crashed
during slice 1; its scaffold and WIP were cherry-picked onto main; its worktree and branch are removed, and its
session record is kept as crash evidence).

**Resume procedure:** dispatch a new `implementor` agent (worktree) with a brief pointing at
this section; it continues from the first slice not marked landed, and the orchestrator updates
this table as each slice lands.

## Appendix A — PM version-pinning mechanics (verified 2026-10-13)

Command-level reference for the five researched backends. (apt, dnf, apk, uv, npm.)

### apt / dpkg
- Discover: `apt-cache madison <pkg>`, `apt list -a <pkg>`, `apt-cache policy <pkg>` (authoritative
  Installed/Candidate/priority view — parse to verify pins).
- Install exact: `apt-get install <pkg>=<version>`; downgrade needs `--allow-downgrades`
  (+`--allow-change-held-packages` if held). Deps are NOT auto-pinned.
- Pin: apt preferences (`/etc/apt/preferences.d/`, Pin/Pin-Priority; `>=1000` forces downgrade,
  `<0` prevents); `apt-mark hold/unhold/showhold` (freezes currently-installed; prefer for
  "hold"); dpkg `--set-selections`. bosun holds only an exact spec (§5.2) and releases its hold
  with `apt-mark unhold`, which exits 0 on an unheld package.
- Hard limit: the version must still be in a configured repo (archives keep ~latest only →
  snapshot.debian.org / PPA / local `.deb`).

**What the `apt` backend reads to choose** (checked 2026-10-17 in `debian:12` with
bookworm-backports and pin files, apt 2.6, and `ubuntu:24.04`, apt 2.8.3):

- `apt-cache madison` lists every version in every source and ignores pins, so bosun no
  longer uses it. `apt-cache policy <pkg>` (run under `LC_ALL=C`) prints the version table:
  each version indented five columns (` *** ` marks the installed one) with the priority apt
  gives it (a version pin's, else its best source's), then its sources indented eight, each
  with its own priority; dpkg's status file (`100 /var/lib/dpkg/status`) only marks the
  installed version. An unknown package prints nothing and exits 0.
- Candidate rule, as observed: backports at 100 lose to the release at 500 (`curl
  8.14.1-2+deb13u2~bpo13+1` vs `7.88.1-10+deb12u15`); once a backport is installed it stays
  the candidate over the release's older version (below 1000 apt never goes back past the
  installed version, even once no source offers it: `bosun-probe 2.0-1` dropped from its
  repository stays the candidate over the 1.0-1 still offered; Debian 13, apt 3.0.3,
  2026-10-17); a pin of 1001 makes an older version the candidate; a package whose
  versions are all pinned below 0 has `Candidate: (none)`.
- `apt-get install pkg=version` installs a version pinned at -1 or -10 all the same, so a
  pin does not stop an exact install; bosun takes such a version only when the spec names
  it, and warns.
- Phased updates (`500 (phased 80%)` on noble-updates): the policy candidate is the phased
  version whatever `APT::Machine-ID` says; `apt-get install pkg` (named, installed or not)
  takes it; only `apt-get upgrade` defers it, and only with a machine id (an image's empty
  `/etc/machine-id` defers nothing). bosun installs by name, so it takes it too.

### dnf / rpm
- NEVRA: `name-[epoch:]version-release.arch`. Discover: `dnf list --showduplicates <pkg>`,
  `dnf repoquery --available <pkg>`.
- Install exact: `dnf install <pkg>-<version>` (auto up/downgrades); `dnf install-nevra` for exact;
  `--allowerasing`, `--best`.
- Downgrade: `dnf downgrade <pkg>[-<version>]`.
- Version lock: `dnf versionlock add|exclude|list|delete|clear` (plugin); list file via `locklist`
  in `/etc/dnf/plugins/versionlock.conf`. Locked pkgs skip security updates until unlocked.

**What the `dnf` backend runs** (built 2026-10-16; verified in `fedora:41`, dnf5 5.2.17, and
`rockylinux:9`, dnf4 4.14.0 / rpm 4.16). Both take the same commands; system scope, Linux only.

- Probe: `dnf --version`; the version is the last word of the first line (`dnf5 version
  5.2.17.0`, `4.14.0`).
- Available: `dnf repoquery --available --queryformat
  '%{name}|%{epoch}|%{version}|%{release}|%{arch}|%{repoid}\n' <name>`, kept only where the
  name is exact (repoquery also matches globs and provides) and the arch is the target's
  (`x86_64`, `aarch64`) or `noarch`. Recorded as `[epoch:]version-release`, the epoch only
  when non-zero. An unknown package is empty output with exit 0, so a non-zero exit (bad repo,
  no network) is an error. dnf4 prints its own newline after the format's, leaving blank lines.
- Choosing (checked 2026-10-17 in `fedora:41` with `priority=1` on `fedora`, and
  `rockylinux:9` with the 9.5 BaseOS vault added at `priority=1`): `repoquery` applies
  excludes (`excludepkgs`) and module filtering (Rocky 9 with no stream enabled lists only the
  non-modular `nodejs` 16), but **not** repository priority, and `--latest-limit 1` takes the
  newest EVR regardless of it. `dnf install <name>` takes the best-priority repository's build
  even when older (`vim-enhanced 2:9.1.737` from fedora over updates' `2:9.1.1914`; `curl
  7.76.1-31.el9` from the vault over baseos' `-40.el9_8.8`), while `dnf install
  <name>-<version>` from a lower-priority repository installs. So bosun ranks admitted builds
  by their best repository's priority (lower number first, 99 by default), then EVR. The
  priorities are dnf's: dnf5 prints them (`dnf --dump-repo-config='*'`, a `======== "<id>"
  repository configuration: ========` header then `priority = N`); dnf4 has no command that
  does, so its Python API is asked (`dnf.Base().read_all_repos()`, `repo.priority`), which
  works unprivileged.
- The installed build (checked 2026-10-17, `fedora:41` dnf5 and `rockylinux:9` dnf4): with
  wget2 2.2.0 installed from updates and fedora (2.1.0) raised to priority 1, `dnf install
  wget2` says it is already installed and `dnf upgrade` does nothing; only `dnf distro-sync`
  takes 2.1.0. The same holds when no enabled repository offers the installed build (updates,
  or baseos, disabled), and for unzip 6.0-60.el9_8 against the 9.5 vault's 6.0-58.el9_5 at
  priority 1. With 2.1.0 installed from fedora at 1, `dnf upgrade` leaves it though updates has
  2.2.0. dnf4's `install` of an installed package takes a newer build when one is offered
  (6.0-58 → 6.0-60 with the vault gone); dnf5's says it is installed. So bosun's choice ranks
  builds behind the installed one last, and the installed one, admitted but offered nowhere,
  below any offered build newer than it.
- Installed: `rpm -q --queryformat '%|EPOCH?{%{EPOCH}:}:{}|%{VERSION}-%{RELEASE}\n' <name>`;
  exit 1 with `package <name> is not installed` on stdout means absent; several lines
  (installonly packages) take the newest.
- Install exact: `dnf install -y --setopt=install_weak_deps=False <name>-<evr>` (`name-0:v-r`
  is accepted too); downgrade: `dnf downgrade -y --setopt=install_weak_deps=False <name>-<evr>`;
  remove: `dnf remove -y <name>`. Refresh: `dnf makecache --refresh`. Source: a Copr project,
  `dnf copr enable -y <owner/project>` then `dnf makecache`.
- Hold: `dnf versionlock add <name>` locks the installed build. dnf5 has versionlock built in
  (`/etc/dnf/versionlock.toml`); dnf4 needs `python3-dnf-plugin-versionlock` (it provides
  `dnf-command(versionlock)`, checked with `rpm -q --whatprovides`; it is not in the
  `rockylinux:9` image). Without it the backend reports holds unsupported, so plans warn
  `backend cannot hold` and install unheld. `add` on a locked package is a no-op, exit 0.
  bosun locks only an exact spec (§5.2) and releases its lock with `dnf versionlock delete`.
- A lock excludes every other build: `install`/`downgrade` of another version fails while it
  is held (`matches only packages excluded by versionlock` / `All matches were filtered out by
  exclude filtering`). So install, downgrade and remove each start with `dnf versionlock delete
  <name>`, which exits 0 when nothing is locked.
- Identity: `pkg:rpm/<ID>/<name>?arch=<rpm arch>&distro=<ID>-<VERSION_ID>` (`fedora-41`,
  `rocky-9.8`); no `distro` qualifier without `VERSION_ID`. Versions compare as vers `rpm`:
  epoch, then `rpmvercmp` on version, then release (`~` before the end, `^` after it); an
  absent release sorts below any release, so the order stays total.

### apk (Alpine; built and checked 2026-10-16 on Alpine 3.20, 3.21 and 3.23)
- Two majors in use: apk-tools 2.14 (Alpine ≤ 3.22) and 3.0 (3.23); every command below
  behaves alike on both, and their version rules differ (Versions, below). `apk --version` prints `apk-tools 2.14.4, compiled for x86_64.`.
- Discover: `apk policy <pkg>` lists each version (indented two spaces) and where it comes
  from (four): `lib/apk/db/installed`, a repository URL, or `@tag URL` for a tagged
  repository. An unknown name exits 0 with nothing. A repository carries **one version per
  branch**, so what's on offer is one version per configured branch; adding another branch's
  repository (`/etc/apk/repositories`) offers its version too. A version only a tagged
  repository carries needs the tag to install (`apk add jq@edge=1.8.2-r0`); bosun leaves it out.
- Installed: `apk info -e -v <pkg>` prints `<pkg>-<version>`; exits with the number of names it
  did not find (1 for one absent package).
- Install exact / up / down: `apk add <pkg>=<version>`; apk moves to whatever the world asks
  for, so a downgrade is the same command, reaching only versions a repository still has.
  bosun follows it with `apk add <pkg>`, which rewrites the world line unpinned and, without
  `--upgrade`, keeps the version installed (checked on 3.20 and 3.23: `nano=7.2-r1` from a
  second branch, then `apk add nano`, stays at 7.2-r1 until `apk upgrade` takes 8.0-r0).
  `--no-interactive` (both majors) keeps an `/etc/apk/interactive` from prompting.
- Pin: `apk add <pkg>=<version>` writes `<pkg>=<version>` to `/etc/apk/world`, and no
  `apk upgrade` moves it until the world changes: that is the hold, which bosun writes only
  for an exact spec (§5.2). Other world forms: `<pkg>` (unpinned), `<pkg>>=1.7`, `<pkg>~1.7` (fuzzy), `<pkg>@tag`,
  `!<pkg>` (conflict), `cmd:`/`so:` virtuals. `apk add <pkg>` re-adds it unpinned.
- Remove: `apk del <pkg>` removes the package and its world line, pin included.
- What the user asked for (export): the world file, less the image's base (`alpine-baselayout`,
  `alpine-keys`, `alpine-release`, `apk-tools`, `busybox`, `libc-utils`, `musl-utils`;
  `alpine-base` on a disk install), conflicts, virtuals and tagged entries.
- Versions: `digit{.digit}…{letter}{_suffix{#}}…{~hash}{-r#}`, ordered by apk-tools'
  `version.c` (vers type `apk`): suffixes `_alpha < _beta < _pre < _rc < (none) < _cvs < _svn
  < _git < _hg < _p`. apk-tools 3: a `.`-number with a leading zero compares as a string, and a
  `~hash` sits between the suffixes and the `-r` revision. apk-tools 2.14: each leading zero
  after the first lowers a `.`-number (`1.0 > 1.00`), a letter may take a number (`1.0b2`), a
  number of 18 or more digits is invalid, and `~` is not in the grammar (`1.0~ab` is invalid,
  and compares equal to `1.0~b`). bosun follows the probed major in ordering and validity
  (§6); checked against `apk version -t` on each index's versions under both.
- Identity: `pkg:apk/<ID>/<name>?arch=<x86_64|aarch64>`, no release qualifier (§5.4).
- Repositories need their signing key in `/etc/apk/keys` as well as a URL, so bosun
  configures none; `ensure_source` adds no step.

### uv (Python; built and checked 2026-10-16 with uv 0.13.1)
- Project: `pyproject.toml` `==` specifiers + `uv.lock` (true lockfile). `uv add 'pkg==X'`,
  `uv lock [--check|--upgrade|--upgrade-package pkg==X]`, `uv sync [--frozen|--locked|--inexact]`
  (exact-by-default: removes unlisted).
- Tools (binaries): `uv tool install pkg==X` (own venv, symlinked bins); `uv tool upgrade` stays
  within the install-time constraint; `uvx pkg@version` ephemeral.
- pip surface: `uv pip install 'pkg==X'` (no lockfile on this path).
- What bosun drives (backend `uv`, user scope, purl `pkg:pypi/<name>`, vers `pypi`):
  - Choose (this machine): uv resolves the spec, `uv pip compile - --no-deps --no-header
    --no-annotate --quiet --color never --python <interpreter>` with `<name><specifiers>` on
    standard input, one call per interval of the vers range (PEP 440 has no union), the
    newest result taken. Its stdout is `<normalized-name>==<version>`; no match is exit 1 with
    `No solution found` on stderr (logged as uv's explanation), a network failure exit 2. The
    interpreter is the one `uv tool install` uses: `uv python find --system` (uv-managed
    first); tools list it in `uv tool list --show-python`. uv skips yanked releases unless
    pinned with `==` (`httpie==2.0.0` resolves, `httpie>=2.0.0,<2.1` does not), pre-releases
    unless named or nothing else exists, and releases whose `Requires-Python` the interpreter
    misses (black: 24.8.0 for 3.8, 25.11.0 for 3.9, 26.10.1 for 3.10). uv reads its own index
    configuration (`UV_DEFAULT_INDEX`, `UV_INDEX_URL`, `uv.toml`); bosun reads none and
    fetches nothing from PyPI on this machine.
  - Configuration (checked 2026-10-17, uv 0.13.1 in `python:3.12-slim`): `uv tool install`
    reads the user's (`~/.config/uv/uv.toml`) and the system's (`/etc/uv/uv.toml`)
    configuration and the environment, but no project's: a `uv.toml` or `pyproject.toml`
    `[tool.uv]` naming an unreachable index in the directory it runs in (or above it), a
    `.python-version` naming an absent interpreter, or `UV_PROJECT` naming such a project
    changes nothing. `uv pip compile` and `uv python find` read all three, and fail on each.
    `--no-config` would drop the user's and the system's configuration too, so bosun runs both
    as `uv --directory / --project / …`: from the filesystem root only a `/uv.toml` or
    `/pyproject.toml` is a project's, which `uv tool install` run from `/` would also ignore,
    the one place they can still disagree.
  - Discover (remote index): PyPI's JSON API, `https://pypi.org/pypi/<name>/json`, `releases`
    (version → its files); a release with no files, or with every file yanked, is left out,
    as is a version outside PEP 440. An unknown project is a 404. The highest version in the
    range is taken, pre-releases only when named or when every version is one (uv's
    `if-necessary-or-explicit`).
  - Install exact / up / down: `uv tool install <name>==<version>`. With another version
    installed it uninstalls that and installs this one; with this one installed it prints
    "`<name>==<version>` is already installed" and exits 0.
  - Installed: `uv tool list` prints `<name> v<version>` per tool (the name PEP 503-normalized:
    `Django_Upgrade` → `django-upgrade`), then `- <executable>` lines; `No tools installed`
    when empty; a broken tool environment is a warning on stderr and the tool is not listed.
  - Executables go to `~/.local/bin` unless `UV_TOOL_BIN_DIR` / `XDG_BIN_HOME` say otherwise
    (the `ghcr.io/astral-sh/uv` images set `UV_TOOL_BIN_DIR=/usr/local/bin`).
  - Remove: `uv tool uninstall <name>` (exits 2 when it is not installed).
  - No hold: re-asserted each run.
  - What the user asked for (export): `uv tool list --show-version-specifiers` adds
    `[required: ==0.6.9]` (and `[with: six]`); a tool installed from a directory or URL shows
    `[required: file:///…]` and is left out.
  - Versions: PEP 440, ordered by `pep440_rs` (`1.0.dev1 < 1.0a1 < 1.0rc1 < 1.0 < 1.0+local <
    1.0.post1`, `1.0 == 1.0.0`, epochs first). Alpha, beta, rc and dev releases are
    pre-releases; post-releases are not. `bosun add` and `export` write the lower bound `uv add`
    writes (`0.6.9` → `>=0.6.9`): the epoch kept, a local label dropped (`2.1.0+cpu` →
    `>=2.1.0`).

### npm (Node)
- Manifest: bare `"1.2.3"` = exact; `^`/`~`/x-ranges otherwise (default save = `^`). Force exact:
  `npm i pkg -E` or `.npmrc save-exact=true`. `overrides` pins transitive.
- Lockfile: `package-lock.json` (true lockfile). `npm ci` = frozen reproducible install (requires
  lock; errors on mismatch; never writes package.json/lock; wipes node_modules).
- Install exact: `npm i <name>@<version>`; downgrade is just an install of a lower version.
- Global tools: `npm i -g pkg@version` — **no lockfile, no durable pin** (weakest tier).
- What bosun drives (backend `npm`, user scope, purl `pkg:npm/[%40scope/]<name>`, vers `npm` =
  SemVer; built and checked 2026-10-16 with npm 10.9.9 in `node:22-slim`):
  - Choose (this machine, checked 2026-10-17 with npm 10.9.7): npm resolves the spec's
    node-semver range with npm-pick-manifest, `npm pack --dry-run --json <name>@<range>` →
    `[{"version": …}]`; no match is exit 1 with `{"error": {"code": "ETARGET"}}` (`E404` for
    an unknown package) on stdout. It prefers the `latest` dist-tag when it satisfies the
    range (`yarn@*` → 1.22.22 though 2.4.3 is published; `npm@>=12` → 12.2.0, `latest`,
    though 12.2.1 is published), then non-deprecated versions, then those whose `engines`
    fit. It fetches the tarball into npm's cache, which the install reuses. `npm view
    <name>@<range> version` lists *every* matching version, so it is not npm's choice.
  - Discover: npm's configured registry (`npm config get registry`, or `@<scope>:registry`
    for a scoped package, `undefined` when unset; the public registry for the remote index),
    `<registry>/<name>` (a scope's `/` as `%2F`) with `Accept:
    application/vnd.npm.install-v1+json` (abbreviated metadata); every key of `versions` that
    is SemVer. An unknown package is a 404. On this machine the listing serves only the
    message when nothing fits; the remote index takes the highest version in the range.
  - Prefix: global installs always go under npm's own global prefix (`npm prefix -g`): nvm's
    or fnm's Node directory, a configured `~/.npm-global`, `%APPDATA%\npm`, or a system Node's
    `/usr/local`. bosun never picks one of its own. It is read once, logged, and every command
    names it with `--prefix`. When an install or uninstall step is first planned in a run, a
    prefix this user cannot write where npm puts packages (`<p>/lib/node_modules`,
    `<p>\node_modules` on Windows, or its nearest existing parent) is warned of: `npm install
    -g` there needs root, user-scope steps never run elevated (§5.5), so the install fails
    with npm's EACCES, as it would by hand; the remedy is a prefix of the user's own (`npm
    config set prefix ~/.npm-global`). Writability is asked of the kernel, `access(2)` for
    write and search with the effective ids (`faccessat(…, AT_EACCESS)`; the read-only
    attribute on Windows), never by writing there; `status` and reads of what is installed
    check nothing. A prefix whose bin dir (`<p>/bin`, `<p>` on Windows) is not on `PATH` is
    warned of then too, as github-release's bin dir is. `npm prefix -g` failing fails every
    npm step and listing.
  - Install exact / up / down: `npm install -g --prefix <p> <name>@<version>` ("changed N
    packages" when moving an installed one).
  - Installed: `npm ls -g --depth=0 --json --prefix <p>` → `dependencies.<name>.version`. A
    prefix npm has not yet installed into exits 254 with `{"error": {"code": "ENOENT"}}`:
    nothing installed. npm exits 1 when the tree has problems but still lists it.
  - Remove: `npm uninstall -g --prefix <p> <name>` (exits 0 even when absent).
  - No hold: re-asserted each run.
  - What the user asked for (export): the same listing, less `npm` and `corepack`, which
    Node ships in its own prefix.
  - On Windows the program is `npm.cmd`.

### winget, scoop, Chocolatey (Windows; built 2026-10-15, unverified on a real host)

- **winget** (user scope): versions `winget show --id <id> --exact --versions`; install
  `winget install --id <id> --exact --version <v> --silent --accept-package-agreements`;
  installed version = the word after the id in `winget list --id <id> --exact`; hold = a
  gating pin `winget pin add --id <id> --exact --version <v> --force`; downgrade = install with
  `--force`; every call adds `--accept-source-agreements --disable-interactivity`. A
  machine-wide installer raises its own UAC prompt. Uninstall leaves the pin (`pin remove`
  fails when there is none). Sources (URL + type) are configured outside bosun.
- **scoop** (user scope, run as `scoop.cmd`): the bucket manifest's `version` (`scoop cat`)
  is the only version listed; install `scoop install <bucket/app>@<v>`; installed version from
  `scoop list <app>`; hold `scoop hold`; no downgrade (the bucket has no older version);
  buckets are sources (`scoop bucket add`).
- **Chocolatey** (system scope): versions `choco search <id> --exact --all-versions
  --limit-output` (`id|version`); installed `choco list <id> --exact --limit-output` (local
  only since v2); install `choco install <id> --version <v> -y --no-progress`; hold
  `choco pin add --name=<id> --version=<v>`; downgrade adds `--allow-downgrade`.

### Cross-cutting
- Registries: on this machine each language backend lists from the registry its tool is
  configured with: npm's (`npm config get`), cargo's source replacement (`[source.crates-io]
  replace-with` followed to a `sparse+` `registry`/`[registries.<n>] index`, read from
  `.cargo/config.toml` (or `config`) in the working directory and its ancestors, then
  `$CARGO_HOME`, the deeper file winning; `cargo config get` is nightly-only on the pinned
  toolchain), and uv's own resolution. A git, local-registry or vendored cargo source is not
  one bosun can list, and says so. Remote indexes keep the public registries.
- True lockfiles: only uv (`uv.lock`) and npm (`package-lock.json`). apt/dnf/apk = policy/list only;
  all binary/tool installs = manifest/imperative only.
- Impedance mismatches: repo-bound (apt/dnf) vs registry-bound (uv/npm) availability;
  non-interchangeable version grammars; OS/system vs per-user tool scope (OS can cascade erasures →
  need dry-run); pin durability varies (hold/versionlock persist; `npm -g` has none → re-assert
  every run); lock vs no-lock reproducibility.

## Appendix B — Known problems from metapac's issue tracker

metapac drives many of the same package managers, so its issue tracker records real failures
bosun will meet too. Each row is a design consequence and a cheap test case. Issue numbers are
in `ripytide/metapac`.

| problem (issues) | consequence for bosun |
|---|---|
| State/config recorded as changed even though the package manager failed (#229 clean marks packages removed; #130 package added to config after a failed install) | Write machine state only after `verify_installed` confirms the change, per package, never from the command's intent. |
| One failing backend aborts every other backend (#189, #226) | Apply per backend and report each backend's result separately. Order within the plan (§4.8) still holds; a failed backend's dependents are skipped, not the whole run. |
| Package manager flags change between releases (#224: brew 5.1.3 dropped `--no-quarantine`) | Record each backend's version in its probe (§5.3); gate flags on it through `capabilities()`; contract tests run against pinned PM versions. |
| "Installed" listings include dependencies (#183 dnf, #222/#175 pacman, #178 xbps, #73 snap base snaps, #129 flatpak runtimes) | Avoided by design: bosun only queries the package IDs it manages (§9). Export reads each PM's own explicit-install record. |
| Packages reinstalled every run, or not detected as installed (#51, #93, #152) | Usually a mismatch between the name requested and the name the PM reports (provides/virtual packages, repo-qualified names). Verify by the package ID recorded in state (§5.4), and add a test per backend for "install, then verify reports it". |
| Groups and metapackages aren't packages (#31, #120, #131, #150 pacman `gnome`) | A group is not a valid package ID; reject it at `add` (or in plan diagnostics) rather than expanding it. |
| One backend, several package kinds (#154 brew formulae vs casks; #103 scoop `bucket/app` form) | The package ID must carry the kind (e.g. a `cask` flag, or a bucket-qualified ID), so verify and prune target the same thing install did. |
| Installation scope set by a flag (#63 flatpak system vs user triggered sudo; #199 flatpak `--installation`) | Scope comes from the backend's reported scope per install (§4.6), feeding the privilege check (§5.5). |
| Install options beyond name + version (#81 snap `--classic`; #110/#227/#228 uv Python version, extras, `--with`) | Backend-specific install options on a manifest entry, recorded in state so re-installs and holds repeat them. |
| Sources needed before install (#218 apt PPAs, #176 dnf copr, #173 brew taps, #201 flatpak remotes) | Confirms the per-backend `sources` section (§9). |
| Non-interactive runs (#202 flatpak `-y`; #193 many confirmation prompts) | bosun confirms once for the whole plan (§4.8) and passes each PM's non-interactive flag; batch installs per backend. |
| Windows command shims (#184: `npm` is `npm.cmd`, so "program not found") | Probe and run commands by resolving `PATHEXT` shims (`.cmd`, `.ps1`), not bare names. |
| Index refresh (#230) | Already explicit `bosun refresh` (§9). |
| Self-elevation requests (#80) | Already decided: never self-elevate (§4.6). |
