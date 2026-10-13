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

# Mutation testing in a disposable worktree (see docs/design/verprop.md quality bars).
# Usage: just mutants crates/bosun-core
mutants target="":
    #!/usr/bin/env bash
    set -euo pipefail
    git worktree add --detach ../.mutants-worktree HEAD
    trap 'git worktree remove ../.mutants-worktree --force' EXIT
    if [ -n "{{target}}" ]; then
        cargo mutants -d ../.mutants-worktree --in-place --timeout 120 -f "{{target}}"
    else
        cargo mutants -d ../.mutants-worktree --in-place --timeout 120
    fi
