Feature: accept, hold and privilege at system scope (apt)

  Background:
    Given apt 2.6.1 is available
    And apt publishes jq 1.7.1-6+deb13u2
    And the manifest:
      """
      version: 2
      packages:
        - name: jq
          spec: ">=1.6"
          install:
            linux:debian: {backend: apt}
      """

  Scenario: an absent apt package is planned to install and then hold
    Given a Debian trixie amd64 machine, not elevated
    When I run `bosun --format json install . --plan`
    Then it reports changes pending
    And it says "install"
    And it says "hold"

  Scenario: a package present outside bosun is accepted, never installed or held
    Given a Debian trixie amd64 machine, not elevated
    And jq 1.7.1-6+deb13u1 is installed outside bosun
    When I run `bosun --format json install . --plan`
    Then it succeeds
    And it says "accept"
    And it does not say "hold"

  Scenario: a system change without elevation is refused with a remedy
    Given a Debian trixie amd64 machine, not elevated
    When I run `bosun --format json install . --yes`
    Then it fails
    And it says "elevation_required"
    And it says "sudo bosun install --scope system"

  Scenario: elevated, the system install applies
    Given a Debian trixie amd64 machine, elevated
    When I run `bosun --format json install . --yes`
    Then it succeeds
    And it says "applied"
