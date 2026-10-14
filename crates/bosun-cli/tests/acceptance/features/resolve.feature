Feature: lock, lock --check, and resolution errors

  Scenario: lock writes the platform's versions and --check then passes
    Given a Debian trixie amd64 machine, not elevated
    And apt 2.6.1 is available
    And apt publishes jq 1.7.1-6+deb13u2
    And the manifest:
      """
      version: 1
      targets: [linux/amd64]
      packages:
        - name: jq
          spec: ">=1.6"
          install:
            linux:debian: {backend: apt}
      """
    When I run `bosun lock .`
    Then a lock is written
    And the lock pins jq 1.7.1-6+deb13u2
    When I run `bosun --format json lock . --check`
    Then it succeeds
    And it says "fresh"

  Scenario: lock --check reports a missing target before the lock exists
    Given a Debian trixie amd64 machine, not elevated
    And apt 2.6.1 is available
    And apt publishes jq 1.7.1-6+deb13u2
    And the manifest:
      """
      version: 1
      targets: [linux/amd64]
      packages:
        - name: jq
          spec: ">=1.6"
          install:
            linux:debian: {backend: apt}
      """
    When I run `bosun --format json lock . --check`
    Then it reports changes pending
    And it says "missing"
    And no lock is written

  Scenario: an unsatisfiable entry makes lock write nothing and exit 1
    Given a Linux amd64 machine, not elevated
    And github-release 1.0.0 is available
    And github-release publishes ctxloom/ctxloom 0.6.4
    And the manifest:
      """
      version: 1
      targets: [linux/amd64]
      packages:
        - name: ctxloom
          spec: ">=99.0.0"
          install:
            linux: {backend: github-release, package: ctxloom/ctxloom}
      """
    When I run `bosun lock .`
    Then it fails
    And no lock is written

  Scenario: an invalid constraint version is a parse error, not a silent miss
    Given a Linux amd64 machine, not elevated
    And github-release 1.0.0 is available
    And github-release publishes ctxloom/ctxloom 0.6.4
    And the manifest:
      """
      version: 1
      targets: [linux/amd64]
      packages:
        - name: ctxloom
          spec: ">=0.6"
          install:
            linux: {backend: github-release, package: ctxloom/ctxloom}
      """
    When I run `bosun --format json lock .`
    Then it fails
    And it says "SemVer"
    And no lock is written
