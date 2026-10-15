Feature: a locked version stays until it's upgraded

  `bosun lock` keeps each version this machine's lock already pins while it
  still fits: the same backend and package, still published, still in the
  spec. New entries, and pins that no longer fit, get the newest version their
  spec allows. `bosun upgrade` moves pins forward; `install` installs the
  pinned versions and writes the lock once it has installed them.

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

  Scenario: lock keeps a pinned version when a newer one is published
    When I run `bosun lock .`
    And cargo publishes ripgrep 14.1.1, 14.2.0
    And I run `bosun lock .`
    Then it succeeds
    And the lock pins ripgrep 14.1.1

  Scenario: adding a package leaves the others' pins alone
    When I run `bosun lock .`
    And cargo publishes ripgrep 14.1.1, 14.2.0
    And cargo publishes fd-find 10.2.0
    And I run `bosun add fd --backend cargo --package fd-find`
    Then it succeeds
    And the lock pins ripgrep 14.1.1
    And the lock pins fd 10.2.0

  Scenario: a spec that no longer admits the pin gets the newest it does
    When I run `bosun lock .`
    And cargo publishes ripgrep 14.1.1, 14.2.0
    And the manifest is replaced by:
      """
      version: 2
      packages:
        - name: ripgrep
          spec: ">=14.2.0|<15.0.0"
          install: {backend: cargo}
      """
    And I run `bosun lock .`
    Then it succeeds
    And the lock pins ripgrep 14.2.0

  Scenario: upgrade moves every pin to the newest its spec allows
    When I run `bosun lock .`
    And cargo publishes ripgrep 14.1.1, 14.2.0
    And I run `bosun --format text upgrade`
    Then it succeeds
    And it says "ripgrep          14.1.1 -> 14.2.0"
    And it says "Run `bosun install` to install them."
    And the lock pins ripgrep 14.2.0
    And ripgrep is not installed

  Scenario: upgrade with a name moves only that package
    Given cargo publishes fd-find 10.1.0
    And the manifest:
      """
      version: 2
      packages:
        - name: ripgrep
          spec: ">=14.0.0|<15.0.0"
          install: {backend: cargo}
        - name: fd
          spec: ">=10.0.0|<11.0.0"
          install: {backend: cargo, package: fd-find}
      """
    When I run `bosun lock .`
    And cargo publishes ripgrep 14.1.1, 14.2.0
    And cargo publishes fd-find 10.1.0, 10.2.0
    And I run `bosun upgrade fd`
    Then it succeeds
    And the lock pins fd 10.2.0
    And the lock pins ripgrep 14.1.1

  Scenario: upgrade with nothing newer says so
    When I run `bosun lock .`
    And I run `bosun --format text upgrade`
    Then it succeeds
    And it says "Everything is at the newest version its spec allows."

  Scenario: upgrade naming a package the manifest doesn't have changes nothing
    When I run `bosun lock .`
    And cargo publishes ripgrep 14.1.1, 14.2.0
    And I run `bosun upgrade rg`
    Then it fails
    And it reports "has no package named \"rg\""
    And the lock pins ripgrep 14.1.1

  Scenario: install installs the pinned version, not the newest
    When I run `bosun lock .`
    And cargo publishes ripgrep 14.1.1, 14.2.0
    And I run `bosun install . --yes`
    Then it succeeds
    And bosun has recorded ripgrep 14.1.1

  Scenario: install without a lock writes one once it has installed
    When I run `bosun install . --yes`
    Then it succeeds
    And bosun has recorded ripgrep 14.1.1
    And the lock pins ripgrep 14.1.1

  Scenario: install --plan writes no lock
    When I run `bosun install . --plan`
    Then it reports changes pending
    And no lock is written

  Scenario: an install that fails writes no lock
    Given installing ripgrep fails
    When I run `bosun install . --yes`
    Then it fails
    And no lock is written
