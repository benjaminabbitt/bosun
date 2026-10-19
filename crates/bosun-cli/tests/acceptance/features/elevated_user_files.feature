Feature: under sudo, the manifest is read and written as the invoking user

  An elevated run reads the manifest, writes the edits add, remove, export
  and migrate make (and the original migrate keeps), and resolves the
  canonical path that keys the lock as the user who elevated it, never as
  root: a symlink the user planted reaches only what they may themselves.
  An edit writes the manifest in place, so one kept behind a symlink stays
  one. The seam's sudo names this test's own ids, so each operation runs in
  the child that has become them.

  Background:
    Given a Linux amd64 machine, not elevated
    And cargo 1.76.0 is available
    And cargo publishes ripgrep 14.1.1
    And cargo publishes fd-find 10.2.0
    And the manifest:
      """
      version: 2
      packages:
        - name: ripgrep
          spec: ">=14.0.0|<15.0.0"
          install: {backend: cargo}
      """

  Scenario: status under sudo reads the manifest as the user
    When later runs are elevated
    And I run `bosun -vv --format text status .`
    Then it succeeds
    And it does "is directory" on the project as the invoking user
    And it does "exists" on bosun.yaml as the invoking user
    And it does "read" on bosun.yaml as the invoking user

  Scenario: lock under sudo resolves the manifest's path, which keys the lock, as the user
    Given the manifest is a symlink to one kept elsewhere
    When later runs are elevated
    And I run `bosun -vv lock .`
    Then it succeeds
    And it does "resolve" on bosun.yaml as the invoking user
    And the lock records the manifest at the one kept elsewhere

  Scenario: add under sudo writes the manifest in place as the user, keeping its symlink
    Given the manifest is a symlink to one kept elsewhere
    When later runs are elevated
    And I run `bosun -vv add fd-find --backend cargo`
    Then it succeeds
    And it does "write in place" on bosun.yaml as the invoking user
    And the manifest is still a symlink to the one kept elsewhere
    And the manifest names fd-find
    And the lock records the manifest at the one kept elsewhere

  Scenario: remove under sudo writes the manifest as the user
    When later runs are elevated
    And I run `bosun -vv remove ripgrep --yes`
    Then it succeeds
    And it does "write in place" on bosun.yaml as the invoking user

  Scenario: export under sudo reads and writes the manifest as the user
    Given fd-find 10.2.0 is installed by hand
    When later runs are elevated
    And I run `bosun -vv export --yes`
    Then it succeeds
    And it does "exists" on bosun.yaml as the invoking user
    And it does "write in place" on bosun.yaml as the invoking user
    And the manifest names fd-find

  Scenario: migrate under sudo keeps the original and writes the migrated manifest as the user
    Given the manifest:
      """
      version: 1
      targets: [linux/amd64]
      packages:
        - name: ripgrep
          spec: ">=14.0.0|<15.0.0"
          install:
            linux: {backends: [cargo]}
      """
    When later runs are elevated
    And I run `bosun -vv migrate . --yes`
    Then it succeeds
    And it does "read" on bosun.yaml as the invoking user
    And it does "write in place" on bosun.yaml.v1 as the invoking user
    And it does "write in place" on bosun.yaml as the invoking user
