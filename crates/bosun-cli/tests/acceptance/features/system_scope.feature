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

  # Only an exact spec is held: under a range or `*` the distro's security
  # and stable updates reach the package between bosun runs.
  Scenario: an absent apt package under a range is planned to install, unheld
    Given a Debian trixie amd64 machine, not elevated
    When I run `bosun --format json install . --plan`
    Then it reports changes pending
    And the plan's actions are "install jq"

  Scenario: an absent apt package under an exact spec is planned to install and then hold
    Given a Debian trixie amd64 machine, not elevated
    And the manifest:
      """
      version: 2
      packages:
        - name: jq
          spec: "1.7.1-6+deb13u2"
          install:
            linux:debian: {backend: apt}
      """
    When I run `bosun --format json install . --plan`
    Then it reports changes pending
    And the plan's actions are "install jq", "hold jq"

  # State from before bosun held only exact specs records no hold: bosun held
  # everything it installed then, so it releases what a range no longer pins.
  Scenario: a package bosun held is unheld once its spec is a range
    Given a Debian trixie amd64 machine, elevated
    And jq 1.7.1-6+deb13u2 is installed outside bosun
    And bosun installed jq 1.7.1-6+deb13u2 for this project wanting ">=1.6"
    When I run `bosun --format json install . --plan`
    Then it reports changes pending
    And the plan's actions are "unhold jq"
    When I run `bosun install . --yes`
    Then it succeeds
    And bosun records jq as not held
    When I run `bosun --format json install . --plan`
    Then it succeeds
    And the plan's actions are none

  Scenario: an unheld package is held once its spec is exact
    Given a Debian trixie amd64 machine, elevated
    And jq 1.7.1-6+deb13u2 is installed outside bosun
    And bosun installed jq 1.7.1-6+deb13u2 unheld for this project wanting ">=1.6"
    And the manifest:
      """
      version: 2
      packages:
        - name: jq
          spec: "1.7.1-6+deb13u2"
          install:
            linux:debian: {backend: apt}
      """
    When I run `bosun --format json install . --plan`
    Then it reports changes pending
    And the plan's actions are "claim jq", "hold jq"
    When I run `bosun install . --yes`
    Then it succeeds
    And bosun records jq as held

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

  Scenario: Debian has no pre-releases: the newest build apt offers is locked, a ~rc one too
    Given a Debian trixie amd64 machine, not elevated
    And apt publishes hello 2.10-3 and 2.11~rc1-1
    And the manifest:
      """
      version: 2
      packages:
        - name: hello
          spec: "*"
          install:
            linux:debian: {backend: apt}
      """
    When I run `bosun lock .`
    Then it succeeds
    And the lock pins hello 2.11~rc1-1
