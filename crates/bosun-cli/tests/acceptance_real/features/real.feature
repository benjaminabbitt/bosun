Feature: real backends against a disposable container

  Each tag names a program the scenario needs on `PATH`: its package
  manager (`@apt-get`, `@dnf`, `@apk`, `@npm`, `@uv`; `@dnf-3` only under
  dnf4), or `useradd` and `runuser` to act as a non-root user. So one feature
  serves Debian, Fedora, RHEL-family and Alpine containers, and any of them
  with Node or uv.

  @apt-get
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

  # apt refuses to change a held package under -y, so bosun releases its own
  # hold before it moves the package to another version.
  @apt-get
  Scenario: apt moves a package it holds up and down, holding each version
    Given the system state is clean
    And a local apt repository offers bosun-probe 1.0-1 and 2.0-1
    And the manifest:
      """
      version: 2
      packages:
        - name: bosun-probe
          spec: "1.0-1"
          install:
            linux:debian: {backend: apt}
      """
    When I run `bosun install . --yes --scope system`
    Then it succeeds
    And dpkg reports bosun-probe at 1.0-1
    And apt-mark reports bosun-probe held
    When the manifest becomes:
      """
      version: 2
      packages:
        - name: bosun-probe
          spec: "2.0-1"
          install:
            linux:debian: {backend: apt}
      """
    And I run `bosun install . --yes --scope system`
    Then it succeeds
    And dpkg reports bosun-probe at 2.0-1
    And apt-mark reports bosun-probe held
    When the manifest becomes:
      """
      version: 2
      packages:
        - name: bosun-probe
          spec: "1.0-1"
          install:
            linux:debian: {backend: apt}
      """
    And I run `bosun install . --yes --scope system`
    Then it succeeds
    And dpkg reports bosun-probe at 1.0-1
    And apt-mark reports bosun-probe held
    When the manifest is deleted
    And I run `bosun prune --yes --scope system`
    Then it succeeds
    And dpkg reports bosun-probe not installed
    And apt-mark reports bosun-probe not held

  # github-deb holds the release .deb it installs through apt, so it too
  # releases the hold before it moves the package to another release.
  @apt-get
  Scenario: github-deb moves a release .deb it holds up and down
    Given the system state is clean and apt has no bat
    And the manifest:
      """
      version: 2
      packages:
        - name: bat
          spec: "0.26.0"
          install:
            linux:
              backends:
                github-deb:
                  package: sharkdp/bat
                  asset: "{{ binary }}_{{ version }}_{{ arch }}.deb"
                  unverified: allow
      """
    When I run `bosun install . --yes --scope system`
    Then it succeeds
    And dpkg reports bat at 0.26.0
    And apt-mark reports bat held
    When the manifest becomes:
      """
      version: 2
      packages:
        - name: bat
          spec: "0.26.1"
          install:
            linux:
              backends:
                github-deb:
                  package: sharkdp/bat
                  asset: "{{ binary }}_{{ version }}_{{ arch }}.deb"
                  unverified: allow
      """
    And I run `bosun install . --yes --scope system`
    Then it succeeds
    And dpkg reports bat at 0.26.1
    And apt-mark reports bat held
    When the manifest becomes:
      """
      version: 2
      packages:
        - name: bat
          spec: "0.26.0"
          install:
            linux:
              backends:
                github-deb:
                  package: sharkdp/bat
                  asset: "{{ binary }}_{{ version }}_{{ arch }}.deb"
                  unverified: allow
      """
    And I run `bosun install . --yes --scope system`
    Then it succeeds
    And dpkg reports bat at 0.26.0
    And apt-mark reports bat held
    When the manifest is deleted
    And I run `bosun prune --yes --scope system`
    Then it succeeds
    And dpkg reports bat not installed
    And apt-mark reports bat not held

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

  @dnf-3
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
  @useradd @runuser
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

  @apk
  Scenario: apk installs a package pinned in the world, then prune deletes it
    Given the apk index is refreshed
    And the system state is clean of tree installed through apk
    And the manifest:
      """
      version: 2
      packages:
        - name: tree
          spec: ">=2.0"
          install:
            linux:alpine: {backend: apk}
      """
    When I run `bosun install . --yes --scope system`
    Then it succeeds
    And apk reports tree installed
    And the apk world pins tree to its installed version
    When the manifest is deleted
    And I run `bosun prune --yes --scope system`
    Then it succeeds
    And apk reports tree not installed
    And the apk world does not name tree

  # A branch carries one version of a package; an older one is gone.
  @apk
  Scenario: apk offers only the version its branch carries
    Given the apk index is refreshed
    And the manifest:
      """
      version: 2
      packages:
        - name: tree
          spec: "1.8.0-r0"
          install:
            linux:alpine: {backend: apk}
      """
    When I run `bosun lock .`
    Then it fails
    And it reports "unsatisfiable"

  # npm is user scope, so it runs as a non-root user (§5.5). A system Node's
  # own prefix is root's, so global installs go under ~/.local.
  @npm @useradd @runuser
  Scenario: npm installs a tool into the user's prefix, moves it down, and prune removes it
    Given the manifest:
      """
      version: 2
      packages:
        - name: semver
          spec: "7.6.0"
          install: {backend: npm}
      """
    And a non-root user owns the project
    When the non-root user runs `bosun install . --yes`
    Then it succeeds
    And the binary semver exists and is executable
    And npm lists semver 7.6.0 under ~/.local
    When the manifest becomes:
      """
      version: 2
      packages:
        - name: semver
          spec: "7.5.0"
          install: {backend: npm}
      """
    And the non-root user runs `bosun install . --yes`
    Then it succeeds
    And npm lists semver 7.5.0 under ~/.local
    When the manifest is deleted
    And the non-root user runs `bosun prune --yes`
    Then it succeeds
    And the binary semver does not exist

  # uv is user scope too; it places tools' executables in ~/.local/bin.
  @uv @useradd @runuser
  Scenario: uv installs a tool, moves it down, and prune removes it
    Given the manifest:
      """
      version: 2
      packages:
        - name: ruff
          spec: "0.6.9"
          install: {backend: uv}
      """
    And a non-root user owns the project
    When the non-root user runs `bosun install . --yes`
    Then it succeeds
    And the binary ruff exists and is executable
    And uv lists ruff 0.6.9
    When the manifest becomes:
      """
      version: 2
      packages:
        - name: ruff
          spec: "0.6.8"
          install: {backend: uv}
      """
    And the non-root user runs `bosun install . --yes`
    Then it succeeds
    And uv lists ruff 0.6.8
    When the manifest is deleted
    And the non-root user runs `bosun prune --yes`
    Then it succeeds
    And the binary ruff does not exist
