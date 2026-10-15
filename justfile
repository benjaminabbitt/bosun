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

# Install the bosun binary into ~/.cargo/bin from this checkout.
install:
    cargo install --locked --path crates/bosun-cli

# Mutation testing in a disposable tree copy (see docs/design/verprop.md quality
# bars). A plain copy — not a git worktree — because the shared .git may be
# read-only; -j1 and TMPDIR off /tmp so the copy's target tree is self-contained.
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
        cargo mutants --in-place -j1 --timeout 120 -f "{{target}}"
    else
        cargo mutants --in-place -j1 --timeout 120
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

# Acceptance against real apt / cargo / github-release (tier 2). Needs a
# disposable container with the backends and network; ignored by `just test`.
# Set BOSUN_ACCEPTANCE_REAL=1 so the real-backend scenarios run.
acceptance-real:
    BOSUN_ACCEPTANCE_REAL=1 cargo test -p bosun-cli --test acceptance_real -- --nocapture
