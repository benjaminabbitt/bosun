Feature: the lock is this machine's own

  `bosun lock` writes what this machine resolved to a lock in the user's state
  directory, one per manifest; it is never written into the project, so it is
  never shared or committed. `install --frozen` installs exactly what that lock
  pins, and only while it matches the manifest.

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
          install: {backend: cargo}
      """

  Scenario: lock writes this machine's lock outside the project and names it
    When I run `bosun --format text lock .`
    Then it succeeds
    And a lock is written
    And it says "state/user/locks/"
    And the lock pins ripgrep 14.1.1

  Scenario: install --frozen installs what the lock pins, not the newest
    When I run `bosun lock .`
    And cargo publishes ripgrep 14.1.1, 14.2.0
    And I run `bosun install . --frozen --yes`
    Then it succeeds
    And bosun has recorded ripgrep 14.1.1

  Scenario: install --frozen without a lock for this manifest fails
    When I run `bosun install . --frozen --yes`
    Then it fails
    And it reports "needs this machine's lock"
    And it reports "run `bosun lock`"

  Scenario: install --frozen refuses a lock the manifest has moved on from
    When I run `bosun lock .`
    And the manifest is replaced by:
      """
      version: 2
      packages:
        - name: ripgrep
          spec: ">=14.1.0|<15.0.0"
          install: {backend: cargo}
      """
    And I run `bosun install . --frozen --yes`
    Then it fails
    And it reports "changed since it was locked"
    And ripgrep is not installed

  Scenario: a comment-only edit leaves the lock usable
    When I run `bosun lock .`
    And the manifest is replaced by:
      """
      # ripgrep, for searching
      version: 2
      packages:
        - name: ripgrep
          spec: ">=14.0.0|<15.0.0"
          install: {backend: cargo}
      """
    And I run `bosun install . --frozen --yes`
    Then it succeeds

  Scenario: a bosun.lock left in the project by an older bosun is named and not read
    Given the project has a bosun.lock from an older bosun
    When I run `bosun --format text lock .`
    Then it succeeds
    And it warns "bosun.lock beside the manifest is no longer read"
    And the lock pins ripgrep 14.1.1
