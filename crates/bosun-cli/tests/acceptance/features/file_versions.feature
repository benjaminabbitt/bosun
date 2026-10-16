Feature: every file bosun reads carries a format version, and older ones migrate

  A manifest or config written for an older format still works: bosun migrates
  it in memory each time it reads it. `bosun migrate` writes a manifest's
  migration back, editing only what changed so comments stay, so `add` and
  `remove` can edit it. A file from a newer bosun is refused, never misread.

  Background:
    Given a Linux amd64 machine, not elevated
    And cargo 1.76.0 is available
    And cargo publishes ripgrep 14.1.1

  Scenario: a version 1 manifest still locks
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
    When I run `bosun lock .`
    Then it succeeds
    And the lock pins ripgrep 14.1.1
    And the manifest is unchanged

  Scenario: add refuses to edit a version 1 manifest and names bosun migrate
    Given the manifest:
      """
      version: 1
      packages: []
      """
    When I run `bosun add ripgrep --backend cargo`
    Then it fails
    And it reports "run `bosun migrate` first"
    And the manifest is unchanged

  Scenario: migrate --plan shows the rewrite and changes nothing
    Given the manifest:
      """
      # kept through the migration
      version: 1
      targets: [linux/amd64]
      packages:
        - name: ripgrep
          spec: ">=14.0.0|<15.0.0"
          install:
            linux: {backends: [cargo]}
      """
    When I run `bosun --format text migrate . --plan`
    Then it succeeds
    And it says "would be rewritten from version 1 to 2 (comments kept)"
    And the manifest is unchanged

  Scenario: migrate rewrites the manifest, keeps the original, and add then works
    Given the manifest:
      """
      # kept through the migration
      version: 1
      targets: [linux/amd64]
      packages:
        - name: fd
          spec: ">=10.0.0|<11.0.0"
          install:
            linux: {backends: [cargo, apt], package: fd-find}
      """
    And cargo publishes fd-find 10.2.0
    When I run `bosun --format text migrate . --yes`
    Then it succeeds
    And it says "Migrated"
    And it says "(comments kept)"
    And the manifest reads:
      """
      # kept through the migration
      version: 2
      packages:
        - name: fd
          spec: ">=10.0.0|<11.0.0"
          install:
            linux:
              backends:
                cargo: {package: fd-find}
                apt: {package: fd-find}
      """
    And the file bosun.yaml.v1 reads:
      """
      # kept through the migration
      version: 1
      targets: [linux/amd64]
      packages:
        - name: fd
          spec: ">=10.0.0|<11.0.0"
          install:
            linux: {backends: [cargo, apt], package: fd-find}
      """
    When I run `bosun add ripgrep --backend cargo`
    Then it succeeds
    And the lock pins ripgrep 14.1.1

  Scenario: migrate without --yes and no terminal rewrites without asking
    Given the manifest:
      """
      version: 1
      packages: []
      """
    When I run `bosun migrate .`
    Then it succeeds
    And it warns "no terminal to ask; applying"
    And the manifest reads:
      """
      version: 2
      packages: []
      """
    And the file bosun.yaml.v1 reads:
      """
      version: 1
      packages: []
      """

  Scenario: migrate leaves a current manifest alone
    Given the manifest:
      """
      version: 2
      packages: []
      """
    When I run `bosun --format text migrate . --yes`
    Then it succeeds
    And it says "is already version 2"
    And the manifest is unchanged

  Scenario: a manifest from a newer bosun is refused
    Given the manifest:
      """
      version: 3
      packages: []
      """
    When I run `bosun lock .`
    Then it fails
    And it reports "newer"
    And no lock is written

  Scenario: a config written before configs had a version still sets the preference
    Given apt 2.6.1 is available
    And apt publishes ripgrep 14.1.0-1
    And the config:
      """
      installer: [cargo]
      """
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
    When I run `bosun --format text lock .`
    Then it succeeds
    And the lock pins ripgrep 14.1.1
