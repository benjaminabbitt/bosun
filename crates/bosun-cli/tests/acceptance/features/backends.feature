Feature: bosun backends lists the backends available on this host

  Background:
    Given a Linux amd64 machine, not elevated
    And cargo 1.76.0 is available
    And github-release 1.0.0 is available

  Scenario: each registered backend is listed with its version
    When I run `bosun --format text backends`
    Then it succeeds
    And it says "cargo"
    And it says "1.76.0"
    And it says "github-release"

  Scenario: --format json reports the registered backends as a JSON document
    When I run `bosun --format json backends`
    Then it succeeds
    And it prints a JSON document
    And it says "registered"
    And it says "skipped"
    And it says "1.76.0"

  Scenario: a package manager the machine lacks is skipped as not present
    Given the machine has no apt
    When I run `bosun --format text backends`
    Then it succeeds
    And it lists cargo 1.76.0 as registered
    And it lists apt as skipped: not present
