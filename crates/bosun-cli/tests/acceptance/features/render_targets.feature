Feature: render resolves versions for a machine bosun isn't running on

  A container build installs into an image, not into the machine running
  bosun, so `render --target linux/arm64 --distro debian@bookworm` resolves the
  manifest for that machine: its platform, and the distro and release given in
  selector form. apt's versions come from the release's own mirror, read as
  the image's apt would read it: the release, its updates and its security
  suite, each package index checked against the hash the release file lists.
  The release file is trusted as fetched over HTTPS; the image's apt checks
  its signature when it installs.
  Backends whose index is already remote (cargo, npm, uv, GitHub releases)
  resolve as they always do, and need not be on this machine; one that can only ask a local package manager is an error
  naming it. Nothing on this machine is asked: not its package managers, and
  not its lock.

  Background:
    Given a Debian trixie amd64 machine, not elevated
    And apt 2.9.8 is available
    And apt publishes jq 1.7.1-6+deb13u2
    And Debian bookworm lists jq 1.6-2.1+deb12u2 for amd64
    And Debian bookworm-security lists jq 1.6-2.1+deb12u3 for amd64
    And Debian bookworm lists jq 1.6-2.1+deb12u2 for arm64
    And the manifest:
      """
      version: 2
      packages:
        - name: jq
          spec: "*"
          install:
            linux:debian: {backend: apt}
      """

  Scenario: render args for a Debian 12 image gives the newest version its suites carry
    When I run `bosun render args . --target linux/amd64 --distro debian@bookworm`
    Then it succeeds
    And it prints:
      """
      --build-arg JQ_PACKAGE=jq --build-arg JQ_VERSION=1.6-2.1+deb12u3
      """
    And it does not warn "--distro"

  Scenario: a spec nothing on the mirror meets is an error naming what is there
    Given the manifest:
      """
      version: 2
      packages:
        - name: jq
          spec: ">=1.7"
          install:
            linux:debian: {backend: apt}
      """
    When I run `bosun render args . --target linux/amd64 --distro debian@bookworm`
    Then it fails
    And it reports "unsatisfiable"
    And it reports "1.6-2.1+deb12u3"

  Scenario: the installer preference picks among backends with a remote index
    Given cargo 1.76.0 is available
    And cargo publishes jq 1.7.1
    And the manifest:
      """
      version: 2
      packages:
        - name: jq
          spec: "*"
          install:
            linux:debian:
              backends: {apt: {}, cargo: {}}
      """
    When I run `bosun render env . --target linux/amd64 --distro debian@bookworm --installer cargo`
    Then it succeeds
    And it says "JQ_VERSION=1.7.1"
    When I run `bosun render env . --target linux/amd64 --distro debian@bookworm --installer apt`
    Then it succeeds
    And it says "JQ_VERSION=1.6-2.1+deb12u3"

  Scenario: reading the release file warns of nothing
    When I run `bosun render env . --target linux/amd64 --distro debian@bookworm`
    Then it succeeds
    And it does not warn "signature"

  Scenario: npm and uv entries resolve for another target from their registries
    Given the npm registry lists cowsay 1.5.0 and 1.6.0
    And PyPI lists httpie 3.2.3, 3.2.4 and 4.0.0b1
    And the manifest:
      """
      version: 2
      packages:
        - name: cowsay
          spec: "*"
          install: {backend: npm}
        - name: httpie
          spec: ">=3.2"
          install: {backend: uv}
      """
    When I run `bosun render env . --target linux/arm64 --distro debian@bookworm`
    Then it succeeds
    And it says "COWSAY_VERSION=1.6.0"
    And it says "HTTPIE_VERSION=3.2.4"

  Scenario: render json gives each platform its own versions
    When I run `bosun render json . --targets linux/amd64,linux/arm64 --distro debian@bookworm`
    Then it succeeds
    And the versions file gives jq 1.6-2.1+deb12u3 as jq from apt for linux/amd64
    And the versions file gives jq 1.6-2.1+deb12u2 as jq from apt for linux/arm64

  Scenario: a release selector applies to the release given
    Given the manifest:
      """
      version: 2
      packages:
        - name: jq
          spec: ">=1.7"
          install:
            linux:debian: {backend: apt}
            linux:debian@bookworm: {backend: apt, spec: ">=1.6|<1.7"}
      """
    When I run `bosun render env . --target linux/amd64 --distro debian@bookworm`
    Then it succeeds
    And it says "JQ_VERSION=1.6-2.1+deb12u3"

  Scenario: an Ubuntu release is like Debian
    Given Ubuntu noble lists jq 1.7.1-3build1 for amd64
    And Ubuntu noble-updates lists jq 1.7.1-3ubuntu0.24.04.1 for amd64
    When I run `bosun render env . --target linux/amd64 --distro ubuntu@noble`
    Then it succeeds
    And it says "JQ_VERSION=1.7.1-3ubuntu0.24.04.1"

  Scenario: platform-independent backends resolve for the target as they do here
    Given cargo 1.76.0 is available
    And cargo publishes ripgrep 14.1.1
    And the manifest:
      """
      version: 2
      packages:
        - name: ripgrep
          spec: ">=14.0.0|<15.0.0"
          install: {backend: cargo}
      """
    When I run `bosun render env . --target linux/arm64 --distro debian@bookworm`
    Then it succeeds
    And it prints:
      """
      RIPGREP_PACKAGE=ripgrep
      RIPGREP_VERSION=14.1.1
      """

  Scenario: a backend with no remote index is an error naming it
    Given the manifest:
      """
      version: 2
      packages:
        - name: jq
          spec: "*"
          install:
            linux:fedora: {backend: dnf}
      """
    When I run `bosun render args . --target linux/amd64 --distro fedora@fedora-41`
    Then it fails
    And it reports "dnf"
    And it reports "remote index"

  Scenario: libc selectors match only when the target's libc is given
    Given cargo 1.76.0 is available
    And cargo publishes jq-musl 1.7.1
    And the manifest:
      """
      version: 2
      packages:
        - name: jq
          spec: "*"
          install:
            linux:debian: {backend: apt}
            linux:musl: {backend: cargo, package: jq-musl}
        - name: tool
          spec: "*"
          install:
            linux:musl: {backend: cargo, package: jq-musl}
      """
    When I run `bosun render env . --target linux/amd64 --distro alpine --libc musl`
    Then it succeeds
    And it says "JQ_PACKAGE=jq-musl"
    And it says "TOOL_VERSION=1.7.1"
    And it does not warn "--libc"
    When I run `bosun render env . --target linux/amd64 --distro debian@bookworm`
    Then it succeeds
    And it says "JQ_PACKAGE=jq"
    And it does not say "TOOL_"
    And it warns "--libc"

  Scenario: without the target's distro, distro selectors don't match it
    When I run `bosun render args . --target linux/arm64`
    Then it succeeds
    And it warns "--distro"
    And it does not say "JQ_"

  Scenario: an apt entry for another platform needs the target's distro
    Given the manifest:
      """
      version: 2
      packages:
        - name: jq
          spec: "*"
          install:
            linux: {backend: apt}
      """
    When I run `bosun render args . --target linux/arm64`
    Then it fails
    And it reports "--distro"

  Scenario: a malformed --libc says what it takes
    When I run `bosun render args . --target linux/amd64 --distro debian@bookworm --libc glibc`
    Then it fails
    And it reports "gnu"
    And it reports "musl"
