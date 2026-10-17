Feature: apk installs Alpine packages at system scope

  Alpine's apk offers what its repositories carry: one version of a package
  per branch, so an exact version is only there while a configured branch
  still has it. Versions order as apk-tools orders them: a revision (`-r1`)
  after the release, a pre-release suffix (`_rc1`) before it, though apk, like
  apt and dnf, installs such a build when its branch carries it. The package is
  identified as `pkg:apk/<ID>/<name>?arch=<apk arch>`, with no release in it:
  Alpine has no release selectors, so its entries use `linux:alpine`. An
  install pins the version in apk's world file, which is also the hold.

  Background:
    Given an Alpine 3.20.3 amd64 machine, not elevated
    And apk 2.14.4 is available

  Scenario: the newest version the branch carries is locked, an _rc build too
    Given apk publishes jq 1.7.1-r0, 1.8.0_rc1-r0 and 1.7.1-r10
    And the manifest:
      """
      version: 2
      packages:
        - name: jq
          spec: ">=1.7"
          install:
            linux:alpine: {backend: apk}
      """
    When I run `bosun lock .`
    Then it succeeds
    And the lock pins jq 1.8.0_rc1-r0
    And the lock identifies jq as pkg:apk/alpine/jq?arch=x86_64

  Scenario: an absent package is planned to install, then hold
    Given apk publishes jq 1.7.1-r0
    And the manifest:
      """
      version: 2
      packages:
        - name: jq
          spec: ">=1.7"
          install:
            linux:alpine: {backend: apk}
      """
    When I run `bosun --format json install . --plan`
    Then it reports changes pending
    And it says "install"
    And it says "hold"

  Scenario: elevated, the install applies and bosun records it
    Given an Alpine 3.20.3 amd64 machine, elevated
    And apk publishes jq 1.7.1-r0
    And the manifest:
      """
      version: 2
      packages:
        - name: jq
          spec: ">=1.7"
          install:
            linux:alpine: {backend: apk}
      """
    When I run `bosun install . --yes`
    Then it succeeds
    And bosun has recorded jq 1.7.1-r0

  Scenario: an Alpine release selector never applies
    Given apk publishes jq 1.6-r4 and 1.7.1-r0
    And the manifest:
      """
      version: 2
      packages:
        - name: jq
          spec: ">=1.6"
          install:
            linux:alpine: {backend: apk}
            linux:alpine@3.20.3: {backend: apk, spec: ">=1.6|<1.7"}
      """
    When I run `bosun lock .`
    Then it succeeds
    And the lock pins jq 1.7.1-r0

  Scenario: a version apk would not accept is a manifest error
    Given apk publishes jq 1.7.1-r0
    And the manifest:
      """
      version: 2
      packages:
        - name: jq
          spec: "1.7.1-1"
          install:
            linux:alpine: {backend: apk}
      """
    When I run `bosun lock .`
    Then it fails
    And it reports "is not a valid apk version"
    And no lock is written

  Scenario: export writes what the world file names, less the base system
    Given apk publishes jq 1.7.1-r0
    And apk publishes busybox 1.36.1-r31
    And jq 1.7.1-r0 is installed by hand
    And busybox 1.36.1-r31 is part of Alpine's base system, in the world file
    When I run `bosun --format text export --yes`
    Then it succeeds
    And it says "Added 1 package"
    And the manifest reads:
      """
      version: 2
      packages:
        - name: jq
          spec: ">=1.7.1|<1.7.2_alpha"
          install:
            linux: {backend: apk}
      """
    And the lock pins jq 1.7.1-r0
    And bosun has recorded nothing about jq
