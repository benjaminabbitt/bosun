Feature: bosun status reports what is installed against what the manifest wants

  Background:
    Given a Linux amd64 machine, not elevated
    And cargo 1.76.0 is available
    And cargo publishes ripgrep 14.1.1
    And the manifest:
      """
      version: 2
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

  Scenario: a package two projects claim lists both manifests, each with its spec
    Given bosun installed ripgrep 14.1.1 for another project wanting ">=14.0.0"
    And bosun installed ripgrep 14.1.1 for this project wanting ">=14.0.0|<15.0.0"
    When I run `bosun --format text status .`
    Then it succeeds
    And it says "claimed by /other/bosun.yaml (vers:cargo/>=14.0.0)"
    And it says the claim of this project's manifest with spec "vers:cargo/>=14.0.0|<15.0.0"

  Scenario: --format json lists each claim's manifest and spec
    Given bosun installed ripgrep 14.1.1 for another project wanting ">=14.0.0"
    And bosun installed ripgrep 14.1.1 for this project wanting ">=14.0.0|<15.0.0"
    When I run `bosun --format json status .`
    Then it succeeds
    And the JSON for ripgrep claims "/other/bosun.yaml" with "vers:cargo/>=14.0.0", manifest gone
    And the JSON for ripgrep claims this project's manifest with "vers:cargo/>=14.0.0|<15.0.0", manifest present

  Scenario: a package present outside bosun has no claims
    Given ripgrep 14.0.0 is installed outside bosun
    When I run `bosun --format json status .`
    Then it succeeds
    And the JSON for ripgrep has no claims

  Scenario: with no path, status reports everything bosun manages, not just this manifest
    Given bosun installed fd 10.2.0 for another project wanting ">=10.0.0"
    And bosun installed ripgrep 14.1.1 for this project wanting ">=14.0.0|<15.0.0"
    When I run `bosun --format text status`
    Then it succeeds
    And it says "fd"
    And it says "10.2.0"
    And it says "ripgrep"
    And it says "claimed by /other/bosun.yaml (vers:cargo/>=10.0.0), manifest gone"
    And it does not say "not installed"

  Scenario: with no manifest, --format json lists each managed package's scope, origin and claims
    Given bosun accepted ripgrep 14.1.1 for this project wanting ">=14.0.0|<15.0.0"
    When I run `bosun --format json status` in an empty directory
    Then it succeeds
    And the JSON for ripgrep claims this project's manifest with "vers:cargo/>=14.0.0|<15.0.0", manifest present
    And the JSON for ripgrep says scope "user", origin "accepted", installed "14.1.1"

  Scenario: with no manifest and nothing managed, status says so
    When I run `bosun --format text status` in an empty directory
    Then it succeeds
    And it says "bosun manages nothing on this machine"
