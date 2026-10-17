Feature: accept, hold and privilege at system scope (apt)

  Background:
    Given apt 2.6.1 is available
    And apt publishes jq 1.7.1-6+deb13u2
    And the manifest:
      """
      version: 2
      packages:
        - name: jq
          spec: ">=1.6"
          install:
            linux:debian: {backend: apt}
      """

  # Only an exact spec is held: under a range or `*` the distro's security
  # and stable updates reach the package between bosun runs.
  Scenario: an absent apt package under a range is planned to install, unheld
    Given a Debian trixie amd64 machine, not elevated
    When I run `bosun --format json install . --plan`
    Then it reports changes pending
    And the plan's actions are "install jq"

  Scenario: an absent apt package under an exact spec is planned to install and then hold
    Given a Debian trixie amd64 machine, not elevated
    And the manifest:
      """
      version: 2
      packages:
        - name: jq
          spec: "1.7.1-6+deb13u2"
          install:
            linux:debian: {backend: apt}
      """
    When I run `bosun --format json install . --plan`
    Then it reports changes pending
    And the plan's actions are "install jq", "hold jq"

  # State from before bosun held only exact specs records no hold: bosun held
  # everything it installed then, so it releases what a range no longer pins.
  Scenario: a package bosun held is unheld once its spec is a range
    Given a Debian trixie amd64 machine, elevated
    And jq 1.7.1-6+deb13u2 is installed outside bosun
    And bosun installed jq 1.7.1-6+deb13u2 for this project wanting ">=1.6"
    When I run `bosun --format json install . --plan`
    Then it reports changes pending
    And the plan's actions are "unhold jq"
    When I run `bosun install . --yes`
    Then it succeeds
    And bosun records jq as not held
    When I run `bosun --format json install . --plan`
    Then it succeeds
    And the plan's actions are none

  Scenario: an unheld package is held once its spec is exact
    Given a Debian trixie amd64 machine, elevated
    And jq 1.7.1-6+deb13u2 is installed outside bosun
    And bosun installed jq 1.7.1-6+deb13u2 unheld for this project wanting ">=1.6"
    And the manifest:
      """
      version: 2
      packages:
        - name: jq
          spec: "1.7.1-6+deb13u2"
          install:
            linux:debian: {backend: apt}
      """
    When I run `bosun --format json install . --plan`
    Then it reports changes pending
    And the plan's actions are "claim jq", "hold jq"
    When I run `bosun install . --yes`
    Then it succeeds
    And bosun records jq as held

  Scenario: a package present outside bosun is accepted, never installed or held
    Given a Debian trixie amd64 machine, not elevated
    And jq 1.7.1-6+deb13u1 is installed outside bosun
    When I run `bosun --format json install . --plan`
    Then it succeeds
    And it says "accept"
    And it does not say "hold"

  Scenario: a system change without elevation is refused with a remedy
    Given a Debian trixie amd64 machine, not elevated
    When I run `bosun --format json install . --yes`
    Then it fails
    And it says "elevation_required"
    And it says "sudo bosun install --scope system"

  Scenario: elevated, the system install applies
    Given a Debian trixie amd64 machine, elevated
    When I run `bosun --format json install . --yes`
    Then it succeeds
    And it says "applied"

  Scenario: Debian has no pre-releases: the newest build apt offers is locked, a ~rc one too
    Given a Debian trixie amd64 machine, not elevated
    And apt publishes hello 2.10-3 and 2.11~rc1-1
    And the manifest:
      """
      version: 2
      packages:
        - name: hello
          spec: "*"
          install:
            linux:debian: {backend: apt}
      """
    When I run `bosun lock .`
    Then it succeeds
    And the lock pins hello 2.11~rc1-1

  # An elevated run leaves the system state root's: readable by every user,
  # its lock writable only by root. A run without elevation reads it under a
  # shared lock it opens read-only.
  @unprivileged
  Scenario: without elevation, system state root left readable is read
    Given a Debian trixie amd64 machine, not elevated
    And jq 1.7.1-6+deb13u2 is installed outside bosun
    And bosun installed jq 1.7.1-6+deb13u2 unheld for this project wanting ">=1.6"
    And the system state is root's, readable by every user
    When I run `bosun --format json prune --yes`
    Then it succeeds
    When I run `bosun --format text status .`
    Then it succeeds
    And it says "1.7.1-6+deb13u2 (up to date)"
    And it does not warn "system state is not readable"

  # State this user cannot read is left out, never failed on or written: its
  # packages are skipped, said once.
  @unprivileged
  Scenario: without elevation, system state this user cannot read is left out
    Given a Debian trixie amd64 machine, not elevated
    And jq 1.7.1-6+deb13u2 is installed outside bosun
    And bosun installed jq 1.7.1-6+deb13u2 unheld for this project wanting ">=1.6"
    And the system state is root's, readable only by root
    When I run `bosun --format json prune --yes`
    Then it succeeds
    And it warns "system state is not readable by this user; its packages are left out"
    When I run `bosun --format json install . --plan`
    Then it succeeds
    And the plan's actions are none
    And it warns once "system state is not readable by this user"
    When I run `bosun --format text status .`
    Then it succeeds
    And it does not say "jq"
    And the system state is unchanged

  @unprivileged
  Scenario: elevated, system state that cannot be read is an error
    Given a Debian trixie amd64 machine, elevated
    And bosun installed jq 1.7.1-6+deb13u2 unheld for this project wanting ">=1.6"
    And the system state is root's, readable only by root
    When I run `bosun --format json prune --yes`
    Then it fails
    And it says "machine state is not readable by this user"

  # accept, claim and drop-claim change no package, only the system state,
  # which only an elevated run writes: without elevation they wait for the
  # next elevated run, and the user-scope tools beside them still install.
  @unprivileged
  Scenario: without elevation, system-scope records wait and user-scope tools install
    Given a Debian trixie amd64 machine, not elevated
    And cargo 1.76.0 is available
    And cargo publishes ripgrep 14.1.1
    And apt publishes tree 2.2.1-1
    And tree 2.2.1-1 is installed outside bosun
    And jq 1.7.1-6+deb13u2 is installed outside bosun
    And bosun installed jq 1.7.1-6+deb13u2 unheld for another project wanting ">=1.6"
    And the system state is root's, readable by every user
    And the manifest:
      """
      version: 2
      packages:
        - name: jq
          spec: ">=1.6"
          install:
            linux:debian: {backend: apt}
        - name: tree
          spec: "*"
          install:
            linux:debian: {backend: apt}
        - name: ripgrep
          spec: "*"
          install:
            linux: {backend: cargo}
      """
    When I run `bosun --format json install . --plan`
    Then it reports changes pending
    And the plan's actions are "install ripgrep"
    When I run `bosun --format text install . --plan`
    Then it reports changes pending
    And it says "jq: claim recorded on the next elevated run"
    And it says "tree: accept recorded on the next elevated run"
    When I run `bosun --format text install . --yes`
    Then it succeeds
    And it says "install    ripgrep          applied"
    And it says "claim      jq               recorded on the next elevated run"
    And it says "accept     tree             recorded on the next elevated run"
    And bosun has recorded ripgrep 14.1.1
    And the system state is unchanged

  @unprivileged
  Scenario: without elevation, a system-scope claim prune drops waits for the next elevated run
    Given a Debian trixie amd64 machine, not elevated
    And jq 1.7.1-6+deb13u2 is installed outside bosun
    And bosun accepted jq 1.7.1-6+deb13u2 for this project wanting ">=1.6"
    And the system state is root's, readable by every user
    When the manifest is deleted
    And I run `bosun --format text prune --yes`
    Then it succeeds
    And it says "drop-claim jq               recorded on the next elevated run"
    And the system state is unchanged

  # Once the entry is gone no later run would drop its claim, so remove's
  # drop-claim cannot wait like install's records: it is refused before the
  # manifest is edited.
  @unprivileged
  Scenario: without elevation, remove of a package with a system-scope claim is refused
    Given a Debian trixie amd64 machine, not elevated
    And jq 1.7.1-6+deb13u2 is installed outside bosun
    And bosun accepted jq 1.7.1-6+deb13u2 for this project wanting ">=1.6"
    And the system state is root's, readable by every user
    When I run `bosun --format text remove jq --yes`
    Then it fails
    And it says "dropping a system-scope claim needs elevation: jq (apt)"
    And it says "re-run as root: sudo bosun remove jq"
    And it does not say "--scope"
    And the manifest is unchanged
    And the system state is unchanged
