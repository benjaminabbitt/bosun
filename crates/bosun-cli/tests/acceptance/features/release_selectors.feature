Feature: an install block can name one release of a distro

  Releases of one distro ship different packages: Debian bookworm's apt has jq
  1.6 and no just at all, while trixie's has jq 1.7.1 and just 1.40. A
  selector `linux:<distro>@<release>` picks a block for one release, so one
  manifest serves both. The release is spelled as the package identity's
  `distro` qualifier is: the `VERSION_CODENAME` for apt (`bookworm`), so it
  only matches through a backend whose packages carry one. It names the
  distro's own `ID`, and wins over every other selector. A machine with no
  release falls back to the less specific blocks; that is never an error.

  Background:
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

  Scenario: on bookworm the bookworm block wins over the distro block
    Given a Debian bookworm amd64 machine, not elevated
    And apt 2.6.1 is available
    And apt publishes jq 1.6-6
    When I run `bosun lock .`
    Then it succeeds
    And a lock is written
    And the lock pins jq 1.6-6

  Scenario: on trixie a bookworm block does not apply
    Given a Debian trixie amd64 machine, not elevated
    And apt 2.9.8 is available
    And apt publishes jq 1.6-6 and 1.7.1-6+deb13u2
    When I run `bosun lock .`
    Then it succeeds
    And a lock is written
    And the lock pins jq 1.7.1-6+deb13u2

  Scenario: a machine with no release codename falls back to the distro block
    Given a Debian amd64 machine with no release codename, not elevated
    And apt 2.9.8 is available
    And apt publishes jq 1.6-6 and 1.7.1-6+deb13u2
    When I run `bosun lock .`
    Then it succeeds
    And a lock is written
    And the lock pins jq 1.7.1-6+deb13u2

  Scenario: a release names the distro's own ID, not a family it is like
    Given an Ubuntu jammy amd64 machine, not elevated
    And apt 2.4.13 is available
    And apt publishes jq 1.6-2.1ubuntu3 and 1.7.1-3build1
    And the manifest:
      """
      version: 2
      packages:
        - name: jq
          spec: ">=1.7"
          install:
            linux:debian: {backend: apt}
            linux:debian@jammy: {backend: apt, spec: ">=1.6|<1.7"}
            linux:ubuntu@noble: {backend: apt, spec: ">=1.6|<1.7"}
      """
    When I run `bosun lock .`
    Then it succeeds
    And a lock is written
    And the lock pins jq 1.7.1-3build1

  Scenario: the release where apt has a package uses apt; the others fall back
    Given a Debian bookworm amd64 machine, not elevated
    And apt 2.6.1 is available
    And github-release 1.0.0 is available
    And github-release publishes casey/just 1.40.0
    And the manifest:
      """
      version: 2
      packages:
        - name: just
          spec: ">=1.21.0"
          install:
            linux: {backend: github-release, package: casey/just}
            linux:debian@trixie: {backend: apt}
      """
    When I run `bosun lock .`
    Then it succeeds
    And a lock is written
    And the lock pins just 1.40.0

  Scenario: on trixie the same manifest installs just from apt
    Given a Debian trixie amd64 machine, not elevated
    And apt 2.9.8 is available
    And github-release 1.0.0 is available
    And apt publishes just 1.40.0-1
    And github-release publishes casey/just 1.40.0
    And the manifest:
      """
      version: 2
      packages:
        - name: just
          spec: ">=1.21.0"
          install:
            linux: {backend: github-release, package: casey/just}
            linux:debian@trixie: {backend: apt}
      """
    When I run `bosun lock .`
    Then it succeeds
    And a lock is written
    And the lock pins just 1.40.0-1

  Scenario: a release block with several backends applies through the one that carries the release
    Given a Debian bookworm amd64 machine, not elevated
    And apt 2.6.1 is available
    And github-release 1.0.0 is available
    And apt publishes jq 1.6-6
    And github-release publishes jqlang/jq 1.7.1
    And the manifest:
      """
      version: 2
      packages:
        - name: jq
          spec: "*"
          install:
            linux:debian: {backend: apt}
            linux:debian@bookworm:
              backends: {apt: {}, github-release: {package: jqlang/jq}}
      """
    When I run `bosun lock .`
    Then it succeeds
    And a lock is written
    And the lock pins jq 1.7.1

  Scenario: a release block whose backends carry no release never applies
    Given a Debian bookworm amd64 machine, not elevated
    And apt 2.6.1 is available
    And github-release 1.0.0 is available
    And apt publishes jq 1.6-6
    And github-release publishes jqlang/jq 1.7.1
    And the manifest:
      """
      version: 2
      packages:
        - name: jq
          spec: "*"
          install:
            linux:debian: {backend: apt}
            linux:debian@bookworm: {backend: github-release, package: jqlang/jq}
      """
    When I run `bosun lock .`
    Then it succeeds
    And a lock is written
    And the lock pins jq 1.6-6

  # Linux Mint 22's own codename is wilma, but its apt reads Ubuntu noble's
  # archive, so its packages and release selectors say noble (§5.4): one
  # naming wilma matches nothing, and bosun says which release to name.
  Scenario: a selector naming a derivative's own codename warns with the release to use
    Given a Linux Mint wilma amd64 machine based on Ubuntu noble, not elevated
    And apt 2.7.14 is available
    And apt publishes jq 1.7.1-3build1
    And the manifest:
      """
      version: 2
      packages:
        - name: jq
          spec: "*"
          install:
            linux:linuxmint@wilma: {backend: apt}
      """
    When I run `bosun lock .`
    Then it succeeds
    And it warns "names the distro's own codename"
    And it warns "linux:linuxmint@noble"
    And the lock does not mention jq
    When the manifest is replaced by:
      """
      version: 2
      packages:
        - name: jq
          spec: "*"
          install:
            linux:linuxmint@noble: {backend: apt}
      """
    And I run `bosun lock .`
    Then it succeeds
    And it does not warn "own codename"
    And the lock pins jq 1.7.1-3build1
