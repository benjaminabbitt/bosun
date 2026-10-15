Feature: github-release packages are locked with their release checksum

  Background:
    Given a Linux amd64 machine, not elevated
    And github-release 1.0.0 is available
    And github-release publishes ctxloom/ctxloom 0.6.4 and 0.6.5
    And the ctxloom/ctxloom release publishes checksum sha256:4f1c0a9e2b7d
    And the manifest:
      """
      version: 2
      packages:
        - name: ctxloom
          spec: ">=0.6.4|<0.7.0"
          install:
            linux: {backend: github-release, package: ctxloom/ctxloom}
      """

  Scenario: lock records the checksum the release publishes
    When I run `bosun lock .`
    Then it succeeds
    And a lock is written
    And the lock pins ctxloom 0.6.5
    And the lock records the checksum sha256:4f1c0a9e2b7d

  Scenario: a github-release install needs no hold and warns about none
    When I run `bosun --format json install . --plan`
    Then it reports changes pending
    And it says "install"
    And it does not say "hold"
    And it does not say "not pinned"

  Scenario: --frozen accepts a locked version inside a semver range
    When I run `bosun lock .`
    And I run `bosun --format json install . --frozen --yes`
    Then it succeeds
    And it says "applied"
    And bosun has recorded ctxloom 0.6.5
