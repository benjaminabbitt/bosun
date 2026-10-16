# bosun task runner.

# List recipes.
default:
    @just --list

# Build the whole workspace.
build:
    cargo build --workspace

# Run all tests (unit, integration, acceptance).
test:
    cargo test --workspace

# Lint: clippy over all targets, warnings are errors.
lint:
    cargo clippy --all-targets -- -D warnings

# Format all code.
fmt:
    cargo fmt --all

# Check formatting without rewriting.
fmt-check:
    cargo fmt --all -- --check

# Build the bosun binary (release, static where configured).
release:
    cargo build --release -p bosun-cli

# Type-check the workspace for Windows (x86_64-pc-windows-gnu) from Linux. Needs
# the rustup target and zig: ring's build script compiles C, and `zig cc` stands
# in for a MinGW gcc (the --target flag cc adds is dropped; zig takes its own).
check-win:
    #!/usr/bin/env bash
    set -euo pipefail
    tmp="$(mktemp -d)"
    trap 'rm -rf "$tmp"' EXIT
    cat > "$tmp/cc" <<'SH'
    #!/bin/sh
    for a in "$@"; do shift; case "$a" in --target=*) ;; *) set -- "$@" "$a";; esac; done
    exec zig cc -target x86_64-windows-gnu "$@"
    SH
    printf '#!/bin/sh\nexec zig ar "$@"\n' > "$tmp/ar"
    chmod +x "$tmp/cc" "$tmp/ar"
    CC_x86_64_pc_windows_gnu="$tmp/cc" AR_x86_64_pc_windows_gnu="$tmp/ar" \
        cargo check --workspace --all-targets --target x86_64-pc-windows-gnu

# Install the bosun binary into ~/.cargo/bin from this checkout.
install:
    cargo install --locked --path crates/bosun-cli

# Mutation testing in a disposable tree copy (see docs/design/verprop.md quality
# bars). A plain copy — not a git worktree — because the shared .git may be
# read-only; TMPDIR off /tmp so the copy's target tree is self-contained
# (--in-place runs mutants one at a time).
# Usage: just mutants [crates/bosun-core/src/vers.rs]
mutants target="":
    #!/usr/bin/env bash
    set -euo pipefail
    copy="$(mktemp -d "${TMPDIR:-$HOME}/.bosun-mutants.XXXXXX")"
    trap 'rm -rf "$copy"' EXIT
    tar -cf - --exclude=target --exclude=.git . | (cd "$copy" && tar -xf -)
    export TMPDIR="$copy/tmp"; mkdir -p "$TMPDIR"
    cd "$copy"
    if [ -n "{{target}}" ]; then
        cargo mutants --in-place --timeout 120 -f "{{target}}"
    else
        cargo mutants --in-place --timeout 120
    fi

# Line coverage over the hermetic suite with cargo-llvm-cov. Pass `html` for a
# browsable report, else a text summary.
coverage report="summary":
    #!/usr/bin/env bash
    set -euo pipefail
    if [ "{{report}}" = "html" ]; then
        cargo llvm-cov --workspace --html
    else
        cargo llvm-cov --workspace --summary-only
    fi

# Acceptance: hermetic cucumber scenarios against the fixture seam (tier 1).
acceptance:
    cargo test -p bosun-cli --test acceptance

# Acceptance against real apt / dnf / github-release (tier 2). Needs a
# disposable container with the backends and network; ignored by `just test`.
# Set BOSUN_ACCEPTANCE_REAL=1 so the real-backend scenarios run.
acceptance-real:
    BOSUN_ACCEPTANCE_REAL=1 cargo test -p bosun-cli --test acceptance_real -- --nocapture

# Tier 2 on musl, run from the host: build bosun and the real-backend tests
# static for x86_64-unknown-linux-musl (needs that rustup target and zig, whose
# `zig cc` compiles ring's C), then run them in a disposable Alpine container
# with this checkout mounted read-only at its own path. Scenarios whose
# programs the image lacks (apt-get, runuser) are skipped.
acceptance-real-musl image="docker.io/library/alpine:3.23":
    #!/usr/bin/env bash
    set -euo pipefail
    tmp="$(mktemp -d)"
    trap 'rm -rf "$tmp"' EXIT
    cat > "$tmp/cc" <<'SH'
    #!/bin/sh
    for a in "$@"; do shift; case "$a" in --target=*) ;; *) set -- "$@" "$a";; esac; done
    exec zig cc -target x86_64-linux-musl -fno-sanitize=undefined "$@"
    SH
    printf '#!/bin/sh\nexec zig ar "$@"\n' > "$tmp/ar"
    chmod +x "$tmp/cc" "$tmp/ar"
    tests="$(CC_x86_64_unknown_linux_musl="$tmp/cc" AR_x86_64_unknown_linux_musl="$tmp/ar" \
        cargo test -p bosun-cli --test acceptance_real --target x86_64-unknown-linux-musl \
        --no-run --message-format=json \
        | jq -r 'select(.target.name == "acceptance_real" and .executable != null) | .executable')"
    root="$(pwd)"
    podman run --rm -v "$root:$root:ro" -w "$root/crates/bosun-cli" \
        -e BOSUN_ACCEPTANCE_REAL=1 "{{image}}" "$tests"
