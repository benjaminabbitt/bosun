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
    And it says "home/.local/state/bosun/locks/"
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

  # Under sudo the state dir is root's; the lock stays the invoking user's,
  # so an elevated run reads and writes the lock the user's own runs do.
  Scenario: under sudo, lock writes the invoking user's lock, not root's
    When later runs are elevated
    And I run `bosun --format text lock .`
    Then it succeeds
    And a lock is written
    And it says "home/.local/state/bosun/locks/"
    And the lock and its directory are the invoking user's
    And root's state holds no lock

  Scenario: under sudo, lock keeps the pins of the lock the user wrote
    When I run `bosun lock .`
    And cargo publishes ripgrep 14.1.1, 14.2.0
    And later runs are elevated
    And I run `bosun lock .`
    Then it succeeds
    And the lock pins ripgrep 14.1.1
    And root's state holds no lock

  # Root's own login names no invoking user: its lock is root's.
  Scenario: in root's own login, lock writes root's lock
    When later runs are root's own login
    And I run `bosun --format text lock .`
    Then it succeeds
    And no lock is written
    And root's state holds a lock
