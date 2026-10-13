# bosun — future backends

Backends that are **not in v1** (v1 list: [verprop.md §4.1](verprop.md)). Most come from
metapac's backend list. v1 takes the backends in heavy use; snap and flatpak qualified and moved
into v1 (verprop.md §5.2). Each entry below says what it would take and why it waits.

Adding any of these is one more `Installer` impl (verprop.md §5.1), plus the pin/hold
mechanics verified to the standard of verprop.md Appendix A. The pinning notes below are
unverified.

| backend | what it is | pinning (unverified) | why it waits |
|---|---|---|---|
| **nix** (`nix profile`) | Nix package manager, any Linux/macOS | Pins by nixpkgs revision (flake ref), not per-package version | Heavy use among developers, but its pinning model overlaps bosun's: a flake lock already pins. Needs a design for mapping a bosun spec to a nixpkgs revision. Strongest candidate here. |
| **mise** | Polyglot runtime/tool manager | `mise use -g tool@version`; mise pins well itself | bosun would be managing a manager. Decide alongside the mise escape hatch in verprop.md §2. |
| **pnpm** (global) | Node package manager | `pnpm add -g pkg@version`; no durable pin, like `npm -g` | npm already covers the registry. Cheap to add when someone uses pnpm instead of npm. |
| **bun** (global) | JS runtime + package manager | `bun add -g pkg@version`; same tier as `npm -g` | As pnpm. |
| **yarn** (global) | Node package manager | `yarn global add` exists only in Yarn 1 (classic) | Yarn 2+ dropped global installs. Probably never. |
| **xbps** | Void Linux | Install a specific version and hold (`xbps-pkgdb -m hold`) | Small user base. Fits the "OS, versioned" tier once wanted. |
| **mas** | Mac App Store CLI | The App Store serves only the latest version: no exact install | Pin would be presence-only plus a warning; low value. |
| **vscode** (extensions) | VS Code extensions | `code --install-extension pub.ext@1.2.3` | Editor extensions, not binaries or tools; outside bosun's "applications/binaries" scope unless that widens. |

## Not from metapac, considered

| backend | note |
|---|---|
| **choco** | Windows; `choco install pkg --version x` and `choco pin`. Decide based on demand alongside winget and scoop. |
| **AppImage** | The "downloaded artifact" fallback for rolling backends (verprop.md §5.2). GitHub release binaries moved to v1 as the `github-release` backend (verprop.md §12). |
