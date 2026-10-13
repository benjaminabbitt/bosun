# Default agent-image BASE: the shared stage-1 of every locally-built agent
# image (tagged ctxloom-agent-base:latest). The per-engine Containerfiles under
# container/production/ layer the agent specifics — engine CLI install, the
# ctxloom binary, the build gates — onto this via ARG BASE_IMAGE, so the base is
# the ONE place the common environment lives (DRY), and the one thing a user
# replaces to bring their own: set isolation_base to devcontainer (`ctxloom
# container scaffold` seeds one from this file) or to an image ref, and the
# same agent layers build on top of it instead.
#
# node:22-slim is the default: Debian (apt — the engine layers' install contract)
# with a current node runtime, which the claude layer's npm install needs anyway.
# A user base must keep that contract: Debian-family with apt, plus node/npm for
# claude-backed images (the in-image build gates fail fast when it doesn't).
#
# The tool layer is the coding-agent baseline the engines expect at RUN time:
# git (repo operations, worktree-in-container), ripgrep (claude's Grep tool —
# the CLI no longer vendors its own), curl + ca-certificates (fetches, the
# opencode installer), unzip (installer archives), jq (ubiquitous in hook scripts),
# strace (the acceptance harness's read-observation probe wraps the engine exec
# in it; inert under the default seccomp profile, which denies ptrace).
FROM node:22-slim

# Default TERM so interactive agent runs get color/ANSI. The runtime starts
# processes with a bare environment, leaving TERM=dumb, which makes every CLI
# (git, ripgrep, the engine itself) strip color and cursor control. A sane
# 256-color default restores it; the isolation runtime can still override TERM
# per run when the host terminal reports something richer or poorer.
ENV TERM=xterm-256color \
    COLORTERM=truecolor

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        git ripgrep curl ca-certificates unzip jq strace \
    && rm -rf /var/lib/apt/lists/*

# Project tooling for bosun and annals. Versions match the host's, so agents and
# the host build and lint alike.
ARG RUST_VERSION=1.96.1
ARG JUST_VERSION=1.40.0
ARG BUF_VERSION=1.65.0
ARG PROTOC_GEN_PROST_VERSION=0.5.0
ARG CARGO_MUTANTS_VERSION=26.2.0
ARG BACON_VERSION=3.22.0
ARG CARGO_LLVM_COV_VERSION=0.8.4

# C toolchain for linking, musl-tools for bosun's static builds, and the ssh
# client. Keys are not baked in; GitHub's host keys are, from its own API.
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        build-essential pkg-config musl-tools openssh-client \
    && rm -rf /var/lib/apt/lists/* \
    && mkdir -p /etc/ssh \
    && curl -fsSL https://api.github.com/meta \
        | jq -r '.ssh_keys[] | "github.com " + .' > /etc/ssh/ssh_known_hosts

# Rust, system-wide and writable by the agent's user, so a project's
# rust-toolchain.toml can add toolchains and `cargo install` works unprivileged.
ENV RUSTUP_HOME=/usr/local/rustup \
    CARGO_HOME=/usr/local/cargo \
    PATH=/usr/local/cargo/bin:$PATH
RUN curl -fsSL https://sh.rustup.rs | sh -s -- -y --no-modify-path \
        --profile minimal --default-toolchain "${RUST_VERSION}" \
        --component clippy,rustfmt,llvm-tools-preview \
    && rustup target add "$(uname -m)-unknown-linux-musl" \
    && cargo install --locked \
        "just@${JUST_VERSION}" \
        "protoc-gen-prost@${PROTOC_GEN_PROST_VERSION}" \
        "cargo-mutants@${CARGO_MUTANTS_VERSION}" \
        "bacon@${BACON_VERSION}" \
        "cargo-llvm-cov@${CARGO_LLVM_COV_VERSION}" \
    && rm -rf "${CARGO_HOME}/registry" "${CARGO_HOME}/git" \
    && chmod -R a+rwX "${RUSTUP_HOME}" "${CARGO_HOME}"

# buf, verified against the release's own sha256.txt.
RUN arch="$(uname -m)" \
    && base="https://github.com/bufbuild/buf/releases/download/v${BUF_VERSION}" \
    && cd /tmp \
    && curl -fsSLO "${base}/buf-Linux-${arch}" \
    && curl -fsSL "${base}/sha256.txt" | grep " buf-Linux-${arch}\$" | sha256sum -c - \
    && install -m 0755 "buf-Linux-${arch}" /usr/local/bin/buf \
    && rm -f "buf-Linux-${arch}"
