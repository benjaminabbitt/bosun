Feature: the package manager moves what bosun installed, and bosun follows

  Under a range bosun holds nothing, so apt's own updates move a package
  between bosun runs. bosun reads the version the package manager reports,
  not the one it recorded, and never takes the package back down for it.

  Background:
    Given apt 2.6.1 is available
    And apt publishes jq 1.7.1-6+deb13u1 and 1.7.1-6+deb13u2
    And a Debian trixie amd64 machine, elevated
    And bosun installed jq 1.7.1-6+deb13u1 unheld for this project wanting ">=1.6"
    And jq 1.7.1-6+deb13u2 is installed outside bosun

  Scenario: a package apt moved within the spec is shown and kept at apt's version
    Given the manifest:
      """
      version: 2
      packages:
        - name: jq
          spec: ">=1.6"
          install:
            linux:debian: {backend: apt}
      """
    When I run `bosun --format text status .`
    Then it succeeds
    And it says "1.7.1-6+deb13u2 (up to date)"
    When I run `bosun --format text status`
    Then it succeeds
    And it says "1.7.1-6+deb13u2"
    When I run `bosun --format json install . --plan`
    Then it succeeds
    And the plan's actions are none
    When I run `bosun install . --yes`
    Then it succeeds
    And the lock pins jq 1.7.1-6+deb13u2

  Scenario: the next write of the state records the version apt moved to
    Given apt publishes hello 2.10-3
    And the manifest:
      """
      version: 2
      packages:
        - name: jq
          spec: ">=1.6"
          install:
            linux:debian: {backend: apt}
        - name: hello
          spec: "*"
          install:
            linux:debian: {backend: apt}
      """
    When I run `bosun --format json install . --plan`
    Then it reports changes pending
    And the plan's actions are "install hello"
    When I run `bosun install . --yes`
    Then it succeeds
    And bosun has recorded jq 1.7.1-6+deb13u2
    And bosun has recorded hello 2.10-3

  # Planned as any version outside the spec: back into it, here by a
  # downgrade the fixture's apt cannot do.
  Scenario: a package apt moved outside the spec is planned back into it, and said so
    Given the manifest:
      """
      version: 2
      packages:
        - name: jq
          spec: "<1.7.1-6+deb13u2"
          install:
            linux:debian: {backend: apt}
      """
    When I run `bosun --format json install . --plan`
    Then it fails
    And it says "unsupported-downgrade"
    And it warns "moved this package outside the spec"
