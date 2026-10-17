Feature: install, upgrade and cross-claim resolution at user scope (cargo)

  Background:
    Given a Linux amd64 machine, not elevated
    And cargo 1.76.0 is available
    And cargo publishes ripgrep 14.0.0 and 14.1.1
    And the manifest:
      """
      version: 2
      packages:
        - name: ripgrep
          spec: ">=14.0.0|<15.0.0"
          install:
            linux: {backend: cargo}
      """

  Scenario: --plan on an absent package plans an install and exits 2
    When I run `bosun --format json install . --plan`
    Then it reports changes pending
    And it says "install"
    And it says "14.1.1"

  Scenario: apply records the installed version in state
    When I run `bosun --format json install . --yes`
    Then it succeeds
    And it says "applied"
    And bosun has recorded ripgrep 14.1.1
    When I run `bosun --format json status .`
    Then it says "14.1.1"

  Scenario: a cargo install needs no hold and warns about none
    When I run `bosun --format json install . --plan`
    Then it does not say "hold"
    And it does not say "not pinned"

  Scenario: --frozen applies the locked version
    When I run `bosun lock .`
    Then a lock is written
    And the lock pins ripgrep 14.1.1
    When I run `bosun --format json install . --frozen --yes`
    Then it succeeds
    And it says "applied"

  Scenario: no terminal and no --yes applies without asking, and says so
    When I run `bosun --format json install .`
    Then it succeeds
    And it says "applied"
    And bosun has recorded ripgrep 14.1.1
    And it warns "no terminal to ask; applying"
    And it does not warn "[y/N]"

  Scenario: an installed-lower package that fits is kept, and upgrade moves it
    Given bosun installed ripgrep 14.0.0 for another project wanting ">=14.0.0|<15.0.0"
    When I run `bosun --format json install . --plan`
    Then it succeeds
    And it does not say "upgrade"
    When I run `bosun install . --yes`
    And I run `bosun upgrade ripgrep`
    And I run `bosun --format json install . --plan`
    Then it reports changes pending
    And it says "upgrade"

  Scenario: another project's tighter claim is respected, not upgraded past
    Given bosun installed ripgrep 14.0.0 for another project wanting ">=14.0.0|<14.1.0"
    When I run `bosun --format json install . --plan`
    Then it succeeds
    And it does not say "upgrade"

  Scenario: conflicting claims are refused rather than flip-flopping
    Given bosun installed ripgrep 14.0.0 for another project wanting "<14.0.0"
    When I run `bosun --format json install . --yes`
    Then it fails
    And it says "jointly satisfied"

  Scenario: a tool another project installed gains this project's claim, so pruning that project keeps it
    Given bosun installed ripgrep 14.1.1 for another project wanting ">=14.0.0"
    When I run `bosun --format json install . --plan`
    Then it succeeds
    And it says "claim"
    When I run `bosun install . --yes`
    Then it succeeds
    When I run `bosun --format json status .`
    Then the JSON for ripgrep claims this project's manifest with "vers:cargo/>=14.0.0|<15.0.0", manifest present
    When I run `bosun --format json install . --plan`
    Then it succeeds
    And it does not say "claim"
    When I run `bosun prune --yes`
    Then it succeeds
    And bosun has recorded ripgrep 14.1.1

  # The lock follows what was installed: when another project's claim makes
  # bosun install a version this project's lock does not pin, the lock pins
  # the installed one once the install succeeds.
  Scenario: another project's claim moves this project's lock to the version installed
    Given cargo publishes ripgrep 14.0.0, 14.0.3 and 14.1.1
    When I run `bosun lock .`
    Then the lock pins ripgrep 14.1.1
    Given bosun installed ripgrep 14.0.0 for another project wanting ">=14.0.0|<14.1.0"
    When I run `bosun --format json install . --plan`
    Then the plan's actions are "upgrade ripgrep"
    And the lock pins ripgrep 14.1.1
    When I run `bosun --format json install . --yes`
    Then it succeeds
    And bosun has recorded ripgrep 14.0.3
    And the lock pins ripgrep 14.0.3
    When I run `bosun --format json install . --frozen --plan`
    Then it succeeds
    And the plan's actions are none
