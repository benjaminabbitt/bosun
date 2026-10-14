Feature: bosun status reports what is installed against what the manifest wants

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

  Scenario: a package that is absent is reported not installed
    When I run `bosun --format text status .`
    Then it succeeds
    And it says "not installed"

  Scenario: a package bosun installed is reported up to date
    When I run `bosun install . --yes`
    And I run `bosun --format text status .`
    Then it succeeds
    And it says "14.1.1 (up to date)"

  Scenario: a package present outside bosun is shown as present, never "not installed"
    Given ripgrep 14.0.0 is installed outside bosun
    When I run `bosun --format text status .`
    Then it succeeds
    And it says "14.0.0 present, not managed (would be accepted)"
    And it does not say "not installed"

  Scenario: a package bosun accepted is reported as managed at its present version
    Given ripgrep 14.1.1 is installed outside bosun
    And bosun accepted ripgrep 14.1.1 for this project wanting ">=14.0.0|<15.0.0"
    When I run `bosun --format text status .`
    Then it succeeds
    And it says "14.1.1 (up to date)"
    And it does not say "not managed"

  Scenario: --format json reports each package's presence and ownership
    Given ripgrep 14.0.0 is installed outside bosun
    When I run `bosun --format json status .`
    Then it succeeds
    And it prints a JSON document
    And it says "managed"
    And it says "present"
    And it says "up_to_date"
