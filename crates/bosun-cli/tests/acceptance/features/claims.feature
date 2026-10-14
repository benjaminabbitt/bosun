Feature: claims, frozen installs and the plan's exit codes

  Background:
    Given a Linux amd64 machine, not elevated
    And cargo 1.76.0 is available
    And cargo publishes ripgrep 14.1.1
    And the manifest:
      """
      version: 1
      targets: [linux/amd64]
      packages:
        - name: ripgrep
          spec: ">=14.0.0|<15.0.0"
          install:
            linux: {backend: cargo}
      """

  Scenario: --plan with changes to make exits 2 and prints a JSON plan
    When I run `bosun --format json install . --plan`
    Then it reports changes pending
    And it prints a JSON document

  Scenario: --plan with nothing to do exits 0
    When I run `bosun install . --yes`
    And I run `bosun install . --plan`
    Then it succeeds

  Scenario: a claim records the manifest by its absolute path, even from a relative one
    When I run `bosun install ./bosun.yaml --yes`
    Then it succeeds
    And the claim names this project's manifest by its absolute path, not "./bosun.yaml"

  Scenario: --frozen refuses to change a package bosun does not own at another version
    Given ripgrep 14.0.0 is installed outside bosun
    When I run `bosun lock .`
    And I run `bosun --format text install . --frozen --yes`
    Then it fails
    And it says "ripgrep: installed 14.0.0, lock pins 14.1.1"
    And bosun has recorded nothing about ripgrep
