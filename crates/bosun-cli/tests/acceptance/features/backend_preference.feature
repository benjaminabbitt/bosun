Feature: this machine's preference picks among the backends a manifest accepts

  The manifest says which backends may install a package; which one does is
  the machine's choice: `--installer`, then `BOSUN_INSTALLER`, then the user
  config's `installer`, then bosun's own order, which tries user-scope backends
  before system-scope ones so nothing needs root when it doesn't have to.

  Background:
    Given a Debian trixie amd64 machine, not elevated
    And apt 2.6.1 is available
    And cargo 1.76.0 is available
    And apt publishes ripgrep 14.1.0-1
    And cargo publishes ripgrep 14.1.1
    And the manifest:
      """
      version: 2
      packages:
        - name: ripgrep
          spec: "*"
          install:
            linux:
              backends: {apt: {}, cargo: {}}
      """

  Scenario: with no preference stated, a user-scope backend is chosen over a system one
    When I run `bosun --format text lock .`
    Then it succeeds
    And the lock pins ripgrep 14.1.1

  Scenario: the user config's installer order wins over bosun's own
    Given the config:
      """
      version: 1
      installer: [apt]
      """
    When I run `bosun --format text lock .`
    Then it succeeds
    And the lock pins ripgrep 14.1.0-1

  Scenario: the --installer flag wins over the user config
    Given the config:
      """
      version: 1
      installer: [apt]
      """
    When I run `bosun --format text --installer cargo lock .`
    Then it succeeds
    And the lock pins ripgrep 14.1.1

  Scenario: a config that doesn't load is an error, not a silently dropped preference
    Given the config:
      """
      version: 1
      installers: [apt]
      """
    When I run `bosun lock .`
    Then it fails
    And it reports "unknown key \"installers\""
    And no lock is written

  Scenario: a config from a newer bosun is refused
    Given the config:
      """
      version: 2
      """
    When I run `bosun lock .`
    Then it fails
    And it reports "newer"
