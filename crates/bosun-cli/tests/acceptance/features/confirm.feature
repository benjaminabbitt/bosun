@pty
Feature: install asks before it changes anything when someone is at a terminal

  Without --yes, an install run from a terminal shows the plan and asks
  "Apply? [y/N]". Only a yes applies it; anything else leaves the machine and
  bosun's records as they were.

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

  Scenario: answering n aborts and changes nothing
    When I run `bosun install .` at a terminal and answer "n"
    Then it succeeds
    And it asked "Apply? [y/N]"
    And it reports "Aborted; nothing changed."
    And bosun has recorded nothing about ripgrep
    And no lock is written

  Scenario: answering y applies the plan
    When I run `bosun install .` at a terminal and answer "y"
    Then it succeeds
    And it asked "Apply? [y/N]"
    And it does not report "Aborted"
    And bosun has recorded ripgrep 14.1.1
    And a lock is written
