Feature: real backends against a disposable container

  A tagged scenario runs only where what it needs is installed: its package
  manager (`@apt`, `@dnf`; `@dnf4` only under dnf4), or `runuser` to act as a
  non-root user (`@github`). So one feature serves Debian, Fedora and
  RHEL-family containers.

  @apt
  Scenario: apt installs and holds a package, then prune removes and unholds it
    Given the apt index is refreshed
    And the system state is clean
    And the manifest:
      """
      version: 1
      targets: [linux/amd64]
      packages:
        - name: hello
          spec: ">=2.0"
          install:
            linux:debian: {backend: apt}
      """
    When I run `bosun install . --yes --scope system`
    Then it succeeds
    And dpkg reports hello installed
    And apt-mark reports hello held
    When the manifest is deleted
    And I run `bosun prune --yes --scope system`
    Then it succeeds
    And dpkg reports hello not installed
    And apt-mark reports hello not held

  @dnf
  Scenario: dnf installs and versionlocks a package, then prune removes and unlocks it
    Given the dnf index is refreshed
    And dnf has a versionlock command
    And the system state is clean and dnf has no tree
    And the manifest:
      """
      version: 2
      packages:
        - name: tree
          spec: ">=1.0"
          install:
            linux:fedora: {backend: dnf}
      """
    When I run `bosun install . --yes --scope system`
    Then it succeeds
    And rpm reports tree installed
    And dnf reports tree locked
    When the manifest is deleted
    And I run `bosun prune --yes --scope system`
    Then it succeeds
    And rpm reports tree not installed
    And dnf reports tree not locked

  @dnf4
  Scenario: dnf4 without the versionlock plugin installs unheld and says so
    Given the dnf index is refreshed
    And dnf has no versionlock command
    And the system state is clean and dnf has no tree
    And the manifest:
      """
      version: 2
      packages:
        - name: tree
          spec: ">=1.0"
          install:
            linux:fedora: {backend: dnf}
      """
    When I run `bosun --format json install . --plan --scope system`
    Then it reports changes pending
    And it says "unsupported-hold"
    When I run `bosun install . --yes --scope system`
    Then it succeeds
    And it warns "install python3-dnf-plugin-versionlock"
    And rpm reports tree installed
    When the manifest is deleted
    And I run `bosun prune --yes --scope system`
    Then it succeeds
    And rpm reports tree not installed

  # github-release is user scope, so it runs as a non-root user (§5.5).
  @github
  Scenario: github-release verifies the checksum and installs an executable
    Given the manifest:
      """
      version: 1
      targets: [linux/amd64]
      packages:
        - name: ctxloom
          spec: ">=0.6.0|<0.7.0"
          install:
            linux: {backend: github-release, package: ctxloom/ctxloom}
      """
    And a non-root user owns the project
    When the non-root user runs `bosun --format json install . --plan`
    Then it reports changes pending
    And it says "89a356c697c9458929ecf851bc85da3a9548d19aa978837b54c0b0b27100810f"
    When the non-root user runs `bosun install . --yes`
    Then it succeeds
    And the binary ctxloom exists and is executable
    And the bin dir has no temp files
