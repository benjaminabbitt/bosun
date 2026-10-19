Feature: under sudo, the user state is the invoking user's

  An elevated run reads and writes the user-scope state of the user who
  elevated it (sudo's SUDO_USER, doas's DOAS_USER), in the default state dir
  of their home, as that user, not root's own: so status, prune and lock see
  what the user's own runs recorded. The system state's claims name that user.

  Background:
    Given a Linux amd64 machine, not elevated
    And cargo 1.76.0 is available
    And cargo publishes ripgrep 14.0.0, 14.1.1
    And the manifest:
      """
      version: 2
      packages:
        - name: ripgrep
          spec: ">=14.0.0|<15.0.0"
          install: {backend: cargo}
      """

  Scenario: status under sudo reports what the user's runs installed
    When I run `bosun install . --yes`
    And later runs are elevated
    And I run `bosun --format text status .`
    Then it succeeds
    And it says "14.1.1 (up to date)"
    And root's state holds no user state

  # Dropping a claim changes only the state, so it needs no change of
  # privilege; an accepted package is never removed.
  Scenario: prune under sudo drops a gone manifest's claim from the user's state
    Given ripgrep 14.1.1 is installed outside bosun
    And bosun accepted ripgrep 14.1.1 for this project wanting ">=14.0.0|<15.0.0"
    When the manifest is deleted
    And later runs are elevated
    And I run `bosun --format text prune --yes`
    Then it succeeds
    And it says "drop-claim"
    And the user state records ripgrep with no claims
    And root's state holds no user state

  Scenario: lock under sudo keeps the version the user's runs installed
    Given bosun installed ripgrep 14.0.0 for another project wanting ">=14.0.0"
    When later runs are elevated
    And I run `bosun lock .`
    Then it succeeds
    And the lock pins ripgrep 14.0.0

  Scenario: under doas, system claims name the user who elevated
    Given a Debian trixie amd64 machine, not elevated
    And apt 2.6.1 is available
    And apt publishes jq 1.7.1-6+deb13u2
    And jq 1.7.1-6+deb13u2 is installed outside bosun
    And the manifest:
      """
      version: 2
      packages:
        - name: jq
          spec: ">=1.6"
          install:
            linux:debian: {backend: apt}
      """
    When later runs are elevated through doas by ann
    And I run `bosun install . --yes`
    Then it succeeds
    And the system state claims jq for ann
