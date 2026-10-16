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

**Remaining escape hatch (not yet researched):** a custom apt/brew backend for `mise`. mise
pins well, but only for language ecosystems.

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
 + purl per    availability:  versions,
 entry)        highest in set) per target)
```

- **manifest** — declarative; per-entry vers spec and per-backend install selectors (§5.6).
- **resolver** — queries available versions, applies ordering, selects the highest in each spec's set.
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
| `bosun-resolve` | manifest + `Index` queries → lock (ordering; highest in set) |
| `bosun-io` | manifest and user config (YAML via `annals-yaml`, comment-preserving edits) and lock/state (JSON) |
| `bosun-apply` | drivers: host applier, container renderer |
| `bosun-host` | probe OS/arch/distro family; run backend probes; build the registry of present backends (§5.3) |
| `bosun-cli` | clap, output formatting, orchestration |

### 4.2 Commands

| command | does |
|---|---|
| `add <name> --backend … [--package] [--spec] [--on <selector>] [--opt k=v]` / `remove <name> [--plan] [--yes]` | edit one manifest entry, keeping every other byte, then run `lock` on the edited manifest. Both refuse a manifest older than the current version (run `migrate`). `add` writes under `any` when every backend is platform-independent, else this machine's OS, unless `--on` says; it checks the entry first (backend options, then a resolve on this machine) and writes nothing on error; without `--spec` it writes the range from the newest version up to the next breaking one (`>=2.0.2\|<3.0.0`, below 1.0 the next minor, apt: any revision of the upstream version); it never installs. `remove` first releases the package (drops this manifest's claim, uninstalls only if bosun installed it and no other manifest claims it, behind the usual confirm), then edits and locks |
| `refresh` | explicit index refresh, per backend |
| `install [path] [--plan] [--scope system\|user] [--yes]` | plan → privilege check → confirm → apply: resolve each entry against this machine's lock (its pins that still fit, the newest in spec for the rest), install, record in the machine state, then write the lock (only after a successful apply; never with `--plan`) (§4.8, §5.5, §7). **Never uninstalls** (except the old name in a `formerly` migration, §5.4) |
| `uninstall [path] [--plan] [--scope system\|user] [--yes]` | the inverse of `install` for one manifest: drop all of its claims, then uninstall each package bosun installed that no other manifest still claims; same plan → confirm → apply flow (§9) |
| `prune [--plan] [--scope system\|user] [--yes]` | machine-wide cleanup: uninstall what bosun installed that no manifest claims any more, and drop claims whose manifest no longer exists (§9) |
| `upgrade [pkg…] [--manifest]` | let go of the named pins (all, when none are named), re-resolve those packages to the newest version their spec allows, write this machine's lock, and show old → new; installs nothing |
| `export` | seed a manifest from the current machine |
| `status [path]` | with a manifest: each of its packages, installed against wanted, and every manifest that claims it (path, spec, user; a claim whose manifest is gone is marked, `prune` drops it). With no path: everything bosun manages on this machine, both scopes, with origin and claims; no manifest needed. Drift is `install --plan`'s job |
| `adopt <name>` | explicitly take ownership of an existing install: `accepted` → `installed` (§4.6) |
| `lock` | resolve the manifest for this machine (its platform, plus platform-independent `any` entries) and write this machine's lock for it, keeping each version it already pins while that still fits (§4.5, §7). Warns when an older bosun's `bosun.lock` sits beside the manifest: it is no longer read |
| `migrate [path] [--plan] [--yes]` | rewrite an older-version manifest as the current version (loading migrates in memory anyway; this writes it, so `add` / `remove` can edit it). annals edits only the entries the migration changes (`Loader::upgrade_keeping`), so comments elsewhere stay; when it can't edit in place it writes the whole file and says comments are not kept. It shows the result and confirms first (§4.8), and keeps the original as `<file>.v<N>` |
| `install --frozen` | install exactly what this machine's lock records; never re-resolves, never writes the lock; errors when there is no lock for this manifest, or the manifest changed since it was locked (§7) |
| `render args\|env\|json` | versions for a target platform as build args, env lines, or a JSON versions file (§8.3) |
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
| **user** | cargo, go install, pipx / uv tool, npm with a user prefix, flatpak `--user`, scoop and winget defaults, **Homebrew** | Linux and macOS `$XDG_STATE_HOME/bosun/state.json` (default `~/.local/state`); Windows `%LOCALAPPDATA%\bosun\state.json` |

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
| per-target leaf | `resolved`, `indexSnapshot`, `checksum` | `installed`, `origin`, `claimedBy` |
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
          "installed": "1.7.1-3ubuntu0.1", "origin": "installed",
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
          "installed": "2.43.0", "origin": "accepted",
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

- Specs whose vers sets intersect: pick the highest available version in the intersection. This is the
  main reason to prefer ranges for widely shared tools.
- Claims that can't all be satisfied (empty intersection, e.g. two different exact pins): **refuse (DECIDED)**,
  naming the manifests and specs that disagree. Never last-writer-wins, which would flip the
  installed version every time you switch projects.
- **Locks advise; specs decide (DECIDED).** Plain `install` picks a version satisfying every
  claim's *spec*, preferring, in order: this project's pin; else what bosun already installed
  (for any project), when every spec admits it; else the newest every spec allows. A project
  resolving a shared tool for the first time therefore locks the installed version rather than
  bumping it, and `bosun upgrade` is how a project moves it. Only `install --frozen` holds a
  claim to its lock's exact version, and refuses on a clash with another claim.

**Multiple OS users.** Claims in the *system* state file also record the claiming user
(`"user": "babbitt"` alongside `manifest`), so prune and conflict checks in one user's project
see every other user's claims on the same system package. User-scope state stays private to
its user.

**Privilege — DECIDED: the user elevates bosun; bosun never elevates itself.** System scope
runs under `sudo bosun …` (Linux/macOS) or from an Administrator terminal (Windows). bosun
checks privilege against the computed plan and fails fast before changing anything (§5.5).

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

**Later: remote indexes**, added backend by backend, so one machine can resolve for another
platform (for `render --target`, §8.3, and container builds). Resolution is already split from
installation for this:

- **`Index`** (query side): answers "which versions exist for this target" from each source's
  index over HTTP: Homebrew's formula API, the winget-pkgs repo, scoop bucket repos, apt
  `Packages` / dnf repodata files, crates.io, the npm registry, PyPI. Runs anywhere.
- **`Installer`** (mutation side): runs the local package manager. Runs only on a matching host.

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
`accept` (record only, no change), `adopt` (explicit only: `accepted` → `installed`),
`hold` / `unhold` — the `install` plan. `remove` (`installed` origin only, no remaining
claims) and `drop-claim` — the `uninstall` and `prune` plans, and `remove <name>`. Each carries scope,
backend, package ID, from → to version, the claim(s) that drove it, and the privilege it needs.

- **Ordering is fixed:** sources first, then migrations, then installs, upgrades and
  downgrades, then holds, so a renamed package's old version is gone before the new one is
  installed.
- **Diagnostics live in the plan:** conflicting exact pins, entries not applicable on this
  machine, missing backends. Errors block apply.
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
    fn hold(&self, pkg: &Ref, v: &Version) -> Result<()>;
    fn downgrade(&self, pkg: &Ref, v: &Version) -> Result<Installed>;  // explicit flag on apt
    fn uninstall(&self, pkg: &Ref) -> Result<()>;                      // prune; managed set only
}
```

Backends declare **capabilities** (`supports_exact_version`, `supports_hold`,
`supports_downgrade`), because they are wildly unequal (see spectrum below). bosun
degrades-with-a-warning when a backend can't honor a pin.

**Mutations are described, then executed (DECIDED).** The mutating `Installer` methods
(`ensure_source`, `install_exact`, `hold`, `downgrade`, `uninstall`) don't act directly. Each
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
| **OS, versioned** (apt/dnf/winget) | good, repo-bound | strong (`apt-mark hold` / `dnf versionlock` / `winget pin`) | repo — latest 1–2 |
| **OS, rolling** (brew/scoop) | weak | freeze-current only | weak — old versions scarce |
| **Universal app** (snap/flatpak) | weak: snap selects a channel (track/risk), not a version; flatpak can check out a past commit | strong (`snap refresh --hold`, `flatpak mask`) | snap: current release per channel; flatpak: the remote's commit history |

Behavior differs per tier: language → just re-assert exact every run; versioned-OS → install +
set a hold; rolling-OS → warn pin is best-effort + fall back to a downloaded artifact;
universal-app → install the channel or commit, then hold.

snap and flatpak are in v1 because they're heavily used on desktop Linux: snap ships by default
on Ubuntu, and flatpak (with Flathub) on Fedora Workstation, Linux Mint and SteamOS. The
mechanics in the row above are unverified (§11).

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
parallel, once per invocation, in `bosun-host`, which also detects OS, architecture and distro
family (for `linux:<family>` selectors, §5.4).

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

- **Selectors:** `install` keys are `any`, `<os>` or `<os>:<distro>`; the most specific match
  for the current machine wins (exact distro `ID` > `ID_LIKE` match > plain `<os>` > `any`),
  then `defaults.install`. A bare block under `install` (its keys are block keys, not
  selectors) is `any`; mixing the two is an error. No match = the entry doesn't apply on this
  platform (reported by `status`, not an error). Distro detection is below.
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

#### Distro detection (proposed)

Source: `/etc/os-release` (systemd/freedesktop standard; fallback `/usr/lib/os-release`),
present on essentially every current distro and container base image. It is plain
`KEY=value` lines, so a small hand-written parser suffices. Fields used:

| distro | `ID` | `ID_LIKE` | `VERSION_ID` |
|---|---|---|---|
| Ubuntu 24.04 | `ubuntu` | `debian` | `24.04` |
| Debian 12 | `debian` | — | `12` |
| Linux Mint | `linuxmint` | `ubuntu debian` | `22` |
| Fedora 41 | `fedora` | — | `41` |
| RHEL / Rocky | `rhel` / `rocky` | `fedora` / `rhel centos fedora` | `9.4` |
| Amazon Linux 2023 | `amzn` | `fedora` | `2023` |
| openSUSE Tumbleweed | `opensuse-tumbleweed` | `opensuse suse` | — |
| Arch / Manjaro | `arch` / `manjaro` | — / `arch` | — |
| Alpine | `alpine` | — | `3.20.3` |

(Typical values; confirm against real images during implementation.)

- `linux:<distro>` matches when `ID` equals it **or** `ID_LIKE` contains it, so Ubuntu, Mint
  and Pop!_OS all match `linux:debian` without being listed.
- Release qualifiers (`linux:ubuntu@24.04`, matching `VERSION_ID`) are reserved syntax for
  later, for packages renamed between releases of one distro.
- No os-release file (e.g. a `scratch` image): no distro, so only plain `linux` selectors
  apply. Legacy files (`/etc/redhat-release`, `lsb_release`) are not supported.
- **`render` needs the distro supplied:** a Docker platform string (`linux/amd64`) doesn't say
  which distro the base image is, so `render` takes `--distro ubuntu@24.04` (or reads it from
  a manifest target declaration) to pick selectors.

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

- **Unelevated, plan changes system scope** → `ElevationRequired`.
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
  `ID`, with `arch` and `distro` qualifiers; a source becomes the standard `repository_url`
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
- **No other range grammar in M1:** no `^`, `~` or `1.7.*`. They can come back later as
  documented sugar that expands to vers.
- The lockfile stores each spec in canonical vers form (`vers:<type>/…`), so a reader needs
  only the vers spec to know what a lock entry allowed.

**One selection rule, no policies (DECIDED).** A spec is a vers set; bosun selects the
**highest available version in the set**, or fails naming the spec and what was available.
A bare version is a one-version set, so an exact pin needs no separate mode. There is no
`policy` field in the manifest, lockfile or state, and no `deviation`: what was resolved is
always inside what was written.

- "Prefer 1.7.1, but accept a later revision" is a range: `vers:deb/>=1.7.1|<1.7.2~`.
  Because selection takes the highest, write an upper bound to stay close to a version.
- **Pre-releases:** they are *in* a vers set by plain ordering, but selection never picks
  one unless some constraint version is itself a pre-release (the SemVer convention, §12
  item 3), or the spec is a bare pre-release version.
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
  - Reference impls to depend on where possible: `deb-version`, an `rpm`/`rpmvercmp` crate,
    `semver`, a PEP 440 crate. Reserve the custom regex-chunk path for the long tail.
  - Go pseudo-versions (`v0.0.0-TIMESTAMP-hash`) validate the model — they order by the embedded
    timestamp chunk, no special-casing.

## 7. Resolution (orthogonal to backend)

Every spec is a vers set, and resolution selects the highest available version in it (§5.6).
There are no per-entry policies: an exact pin is a one-version set and a range is a wider one.

Consequences:

- `highest-in-set` is impossible without correct ordering (§6). That is the payoff for
  getting ordering right.
- A pin that the configured sources no longer carry is unsatisfiable. The remedy is to widen
  the range (e.g. `>=1.7.1|<1.7.2~` for "any Debian revision of 1.7.1"), not a fallback mode.
- Ranges **break reproducibility by design**, so bosun inherits uv/npm's frozen-vs-update split.
  **A locked version stays until it's upgraded (DECIDED):** resolving keeps each pin that
  still fits — the same backend and package identity, still published, still in the spec —
  and gives the newest in-spec version only to new entries and pins that no longer fit. A new
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
  Rendering needs the base image's distro (for selectors and purl namespace): declared per
  target (`--distro debian@12`), or read once from the image's `/etc/os-release`. Versions
  for a platform other than the rendering machine's need remote indexes (§4.7); until then
  this is rendered inside the base image, or from exact specs.
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

- **Variable names** derive from the logical name: uppercase, non-alphanumerics → `_`, suffixes
  `_VERSION` and `_PACKAGE`. The package ID is emitted because it differs per platform
  (`fd-find` vs `fd`), so the Dockerfile never hardcodes it. Two entries mapping to the same
  variable is an error; an entry can set `"var": "FD"` to override.
- **Target selection** applies the same `install` selectors and installer preference as the
  `install` command (§5.3, §5.4), for the requested target rather than the current machine.
- **`render json` covers multi-arch:** build args are the same for every platform in a
  multi-platform build, so where versions or package IDs differ per platform the Dockerfile
  looks them up by `TARGETPLATFORM` instead.
- **Versions come from this machine's lock** for this machine's platform (§4.5); a stale or
  missing lock is an error naming `bosun lock`. Another platform's versions are resolved
  through remote indexes when the backend has one (§4.7), else rendering for it is an error.

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
- **Install scope + PATH — DECIDED: delegate, no validation (for now).** bosun does **not**
  validate PATH reachability or manage install scope/location itself — it delegates to the
  installer / package manager's own default placement (`npm -g` prefix, `~/.cargo/bin`, pipx,
  system prefix, etc.). "installed but not on PATH" is the user's / the manager's concern, not
  bosun's, at this stage.
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

## 11. Open questions / next steps

- ~~metapac as a base~~ — decided against (§2). Still open: a quick look at a mise custom
  backend before committing to greenfield.
- Verify exact pin/hold/install-exact mechanics for the un-researched backends (scoop, winget,
  brew, pacman, zypper, apk, snap, flatpak; go install, cargo install, gem) — same
  command-level rigor as the apt/dnf/uv/npm reference in §A. For snap: can a non-publisher
  install a specific `--revision`, or only a channel? For flatpak: how are past commits
  discovered and installed (`remote-info --log`, `update --commit`)?
- Nail the manifest schema (`bosun.yaml`): top-level `targets`, `sources`, `defaults`;
  entry = {name, backend, spec (vers), source?, per-target overrides?}.
- Confirm the name. `bosun` is the working name. The `bosun` crate on crates.io is taken,
  which doesn't matter while bosun ships only as release binaries (§3.1).
- annals-yaml's comment-preserving editor: confirm it covers every manifest edit `add`,
  `remove` and `export` make, including entries inside compact `- name: …` items.
- Distro detection (§5.4): confirm `ID_LIKE` matching, deferring release qualifiers, and
  `render --distro`.
- Remote `Index` per backend (§4.7, later): confirm which are feasible (Homebrew API,
  winget-pkgs, apt `Packages`, dnf repodata), which `render --target` and container builds
  for another platform need now that the lock is per machine.
- Dogfood (§12): how do ltk, taskloom and harp install, given that only `ctxloom` is published
  as a release asset? Confirm the `github-release` backend and its two exceptions to §9 (bosun
  verifies checksums; bosun picks a bin directory); decide how a family of entries shares one
  release version; find out how reprise ships.

## 12. Dogfood

The first real manifest manages bosun's own toolbelt: **just, jq, and the ctxloom family**
(ctxloom, ltk, taskloom, harp, reprise), on Debian 13 first, then macOS and Windows.

| tool | Linux (Debian) | macOS | Windows |
|---|---|---|---|
| `just` | apt `just` | brew `just` | winget / scoop (ID to verify) |
| `jq` | apt `jq` | brew `jq` | winget `jqlang.jq` (to verify) |
| ctxloom, ltk, taskloom, harp | GitHub release archive | brew tap `ctxloom/tap` | GitHub release archive |
| reprise | not in the ctxloom release config; distribution to find out | | |

**Verified 2026-10-14 against the public `ctxloom/ctxloom` releases (latest `v0.6.4`):** only
the `ctxloom` binary is published. Each OS/arch has `ctxloom_<ver>_<os>_<arch>.tar.gz` and a
`…_full.tar.gz` variant (Windows: `.zip`), plus `checksums.txt`, `install.sh` and
`install.ps1`. The archive holds `ctxloom` at its root, with LICENSE, README.md and man pages.
No pre-releases are published (`0.7.0-pre1` exists only as local dev builds). **ltk, taskloom
and harp are not release assets**, so their install path is an open question (§11).
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
   gets a way to share one `spec` across entries, or `lock` warns when they diverge.
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
- The dogfood `bosun.yaml` targets plain `linux:debian`, not a specific release.
- ~~Range grammar is per version scheme (semver `VersionReq` / AND-ed comparators)~~ —
  superseded by vers (§5.6), slice 4b.
- **No policies (user decision):** tolerant, exact/range modes and `deviation` are removed;
  vers ranges cover them (§5.6, §7).
- **purl + vers (user decision, §5.6):** package identity is purl; every spec is vers;
  selection is "highest available in the vers set". This supersedes the per-scheme range grammar recorded below (no `^`, `~`,
  `1.2.*` in M1).
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
  Install blocks hold an ordered `backends` list (the YAML accepts a scalar or a sequence).
  The deb `distro` qualifier is os-release `VERSION_CODENAME`.
- An empty claim intersection is one refusal (listing each manifest's spec and what's
  available) whether the specs are disjoint or nothing published fits; telling those apart
  needs vers interval algebra, deferred. Reporting lockfiles a plain `install` doesn't match is
  the command layer's job.
- **Plan exit code counts package changes only** (install / upgrade / downgrade / hold /
  remove). Claim bookkeeping (`accept`, `drop-claim`) is shown and applied but exits 0, so
  `install --plan` stays a drift check on a fresh CI machine whose tools are already present.
- **Unsupported capability degrades** (§5.2): a hold the backend can't do is a warning and is
  skipped; a downgrade it can't do is an error diagnostic for that package, not a failure of
  the whole plan.
- Holds are change-driven: emitted only when the package changes this run, so a fully-applied
  manifest plans to nothing.
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

**Deferred past M1:** `render` and
`Containerfile.bosun` (§8, steps already describable); `upgrade`, `export`, `adopt`, `sources`,
`formerly` migration; persisted plans; macOS / Windows machines; other backends; remote
indexes; distinct messages for disjoint-vs-unpublished claim intersections (needs vers interval
algebra); ltk / taskloom / harp in the dogfood (not published as release assets); a cargo
scenario in the real-backend tier (skipped for compile time).

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

Command-level reference for the four researched backends. (apt, dnf, uv, npm.)

### apt / dpkg
- Discover: `apt-cache madison <pkg>`, `apt list -a <pkg>`, `apt-cache policy <pkg>` (authoritative
  Installed/Candidate/priority view — parse to verify pins).
- Install exact: `apt-get install <pkg>=<version>`; downgrade needs `--allow-downgrades`
  (+`--allow-change-held-packages` if held). Deps are NOT auto-pinned.
- Pin: apt preferences (`/etc/apt/preferences.d/`, Pin/Pin-Priority; `>=1000` forces downgrade,
  `<0` prevents); `apt-mark hold/unhold/showhold` (freezes currently-installed; prefer for
  "hold"); dpkg `--set-selections`.
- Hard limit: the version must still be in a configured repo (archives keep ~latest only →
  snapshot.debian.org / PPA / local `.deb`).

### dnf / rpm
- NEVRA: `name-[epoch:]version-release.arch`. Discover: `dnf list --showduplicates <pkg>`,
  `dnf repoquery --available <pkg>`.
- Install exact: `dnf install <pkg>-<version>` (auto up/downgrades); `dnf install-nevra` for exact;
  `--allowerasing`, `--best`.
- Downgrade: `dnf downgrade <pkg>[-<version>]`.
- Version lock: `dnf versionlock add|exclude|list|delete|clear` (plugin); list file via `locklist`
  in `/etc/dnf/plugins/versionlock.conf`. Locked pkgs skip security updates until unlocked.

### uv (Python)
- Project: `pyproject.toml` `==` specifiers + `uv.lock` (true lockfile). `uv add 'pkg==X'`,
  `uv lock [--check|--upgrade|--upgrade-package pkg==X]`, `uv sync [--frozen|--locked|--inexact]`
  (exact-by-default: removes unlisted).
- Tools (binaries): `uv tool install pkg==X` (own venv, symlinked bins); `uv tool upgrade` stays
  within the install-time constraint; `uvx pkg@version` ephemeral.
- pip surface: `uv pip install 'pkg==X'` (no lockfile on this path).

### npm (Node)
- Manifest: bare `"1.2.3"` = exact; `^`/`~`/x-ranges otherwise (default save = `^`). Force exact:
  `npm i pkg -E` or `.npmrc save-exact=true`. `overrides` pins transitive.
- Lockfile: `package-lock.json` (true lockfile). `npm ci` = frozen reproducible install (requires
  lock; errors on mismatch; never writes package.json/lock; wipes node_modules).
- Install exact: `npm i <name>@<version>`; downgrade is just an install of a lower version.
- Global tools: `npm i -g pkg@version` — **no lockfile, no durable pin** (weakest tier).

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
- True lockfiles: only uv (`uv.lock`) and npm (`package-lock.json`). apt/dnf = policy/list only;
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
