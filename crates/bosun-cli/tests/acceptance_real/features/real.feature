Feature: real backends against a disposable container

  Each tag names a program the scenario needs on `PATH`: its package
  manager (`@apt-get`, `@dnf`, `@apk`, `@npm`, `@uv`; `@dnf-3` only under
  dnf4), or `useradd` and `runuser` to act as a non-root user. So one feature
  serves Debian, Fedora, RHEL-family and Alpine containers, and any of them
  with Node or uv.

  # Only an exact spec is held; under a range apt's own updates reach it.
  @apt-get
  Scenario: apt installs a package under a range unheld, then prune removes it
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
    And apt-mark reports hello not held
    When the manifest is deleted
    And I run `bosun prune --yes --scope system`
    Then it succeeds
    And dpkg reports hello not installed
    And apt-mark reports hello not held

  # A package bosun held under an exact spec is released once the spec is a
  # range, and apt may then move it; bosun leaves that version be.
  @apt-get
  Scenario: apt releases bosun's hold when the spec becomes a range, and apt may update it
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
          spec: "*"
          install:
            linux:debian: {backend: apt}
      """
    And I run `bosun install . --yes --scope system`
    Then it succeeds
    And it reports "unhold"
    And dpkg reports bosun-probe at 1.0-1
    And apt-mark reports bosun-probe not held
    When apt upgrades bosun-probe
    Then dpkg reports bosun-probe at 2.0-1
    When I run `bosun install . --yes --scope system`
    Then it succeeds
    And dpkg reports bosun-probe at 2.0-1
    And apt-mark reports bosun-probe not held
    When the manifest is deleted
    And I run `bosun prune --yes --scope system`
    Then it succeeds
    And dpkg reports bosun-probe not installed

  # Under a range apt's own updates move a package bosun installed; bosun
  # reads the version dpkg reports, shows it, and plans nothing for it.
  @apt-get
  Scenario: apt updates a package bosun installed under a range, and bosun follows
    Given the system state is clean
    And a local apt repository offers bosun-probe 1.0-1 and 2.0-1
    And the manifest:
      """
      version: 2
      packages:
        - name: bosun-probe
          spec: "<2.0"
          install:
            linux:debian: {backend: apt}
      """
    When I run `bosun install . --yes --scope system`
    Then it succeeds
    And dpkg reports bosun-probe at 1.0-1
    When the manifest becomes:
      """
      version: 2
      packages:
        - name: bosun-probe
          spec: "*"
          install:
            linux:debian: {backend: apt}
      """
    And I run `bosun install . --yes --scope system`
    Then it succeeds
    And dpkg reports bosun-probe at 1.0-1
    When apt upgrades bosun-probe
    Then dpkg reports bosun-probe at 2.0-1
    When I run `bosun --format text status .`
    Then it succeeds
    And it says "2.0-1 (up to date)"
    When I run `bosun --format json install . --plan --scope system`
    Then it succeeds
    And it does not report "downgrade"
    When the manifest is deleted
    And I run `bosun prune --yes --scope system`
    Then it succeeds
    And dpkg reports bosun-probe not installed

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

  # Within a spec, apt's pin priorities choose, as its candidate is chosen: a
  # version at priority 100 (what a backports suite gets) loses to the
  # release's 500 unless the spec admits only it.
  @apt-get
  Scenario: apt's pin priorities choose the version within the spec
    Given the system state is clean
    And a local apt repository offers bosun-probe 1.0-1 and 2.0-1
    And apt pins bosun-probe 2.0-1 at priority 100
    And the manifest:
      """
      version: 2
      packages:
        - name: bosun-probe
          spec: "*"
          install:
            linux:debian: {backend: apt}
      """
    When I run `bosun install . --yes --scope system`
    Then it succeeds
    And dpkg reports bosun-probe at 1.0-1
    When the manifest becomes:
      """
      version: 2
      packages:
        - name: bosun-probe
          spec: ">=2.0"
          install:
            linux:debian: {backend: apt}
      """
    And I run `bosun install . --yes --scope system`
    Then it succeeds
    And dpkg reports bosun-probe at 2.0-1
    When the manifest is deleted
    And I run `bosun prune --yes --scope system`
    Then it succeeds
    And dpkg reports bosun-probe not installed

  # A version pinned below 0 is never apt's candidate; only a spec naming it
  # exactly installs it, as `apt-get install pkg=version` does.
  @apt-get
  Scenario: apt never takes a version pinned away unless the spec names it
    Given the system state is clean
    And a local apt repository offers bosun-probe 1.0-1 and 2.0-1
    And apt pins bosun-probe 2.0-1 at priority -1
    And the manifest:
      """
      version: 2
      packages:
        - name: bosun-probe
          spec: ">=2.0"
          install:
            linux:debian: {backend: apt}
      """
    When I run `bosun lock .`
    Then it fails
    And it reports "unsatisfiable"
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
    And it warns "the spec names a version apt's pins keep from installing"
    And dpkg reports bosun-probe at 2.0-1
    When the manifest is deleted
    And I run `bosun prune --yes --scope system`
    Then it succeeds
    And dpkg reports bosun-probe not installed

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
  Scenario: dnf versionlocks a package only while its spec is exact, then prune removes it
    Given the dnf index is refreshed
    And dnf has a versionlock command
    And the system state is clean and dnf has no tree
    And the manifest:
      """
      version: 2
      packages:
        - name: tree
          spec: "*"
          install:
            linux: {backend: dnf}
      """
    When I run `bosun install . --yes --scope system`
    Then it succeeds
    And rpm reports tree installed
    And dnf reports tree not locked
    When the manifest pins tree through dnf to the version installed
    And I run `bosun install . --yes --scope system`
    Then it succeeds
    And dnf reports tree locked
    When the manifest becomes:
      """
      version: 2
      packages:
        - name: tree
          spec: ">=1.0"
          install:
            linux: {backend: dnf}
      """
    And I run `bosun install . --yes --scope system`
    Then it succeeds
    And it reports "unhold"
    And rpm reports tree installed
    And dnf reports tree not locked
    When the manifest is deleted
    And I run `bosun prune --yes --scope system`
    Then it succeeds
    And rpm reports tree not installed
    And dnf reports tree not locked

  # Within a spec, dnf's repository priorities choose, as `dnf install`
  # does: the best-priority repository's build wins over a newer one, which
  # is taken only when the spec admits nothing better. Fedora 41 (dnf5):
  # fedora has wget2 2.1.0, updates 2.2.0.
  @dnf5
  Scenario: dnf5's repository priorities choose the version within the spec
    Given the dnf repository fedora has priority 1
    And the dnf index is refreshed
    And the system state is clean and dnf has no wget2
    And the manifest:
      """
      version: 2
      packages:
        - name: wget2
          spec: "*"
          install:
            linux:fedora: {backend: dnf}
      """
    When I run `bosun install . --yes --scope system`
    Then it succeeds
    And rpm reports wget2 at 2.1.0-12.fc41
    When the manifest becomes:
      """
      version: 2
      packages:
        - name: wget2
          spec: ">=2.2"
          install:
            linux:fedora: {backend: dnf}
      """
    And I run `bosun install . --yes --scope system`
    Then it succeeds
    And rpm reports wget2 at 2.2.0-3.fc41
    When the manifest is deleted
    And I run `bosun prune --yes --scope system`
    Then it succeeds
    And rpm reports wget2 not installed

  # Rocky 9 (dnf4), with the 9.5 BaseOS vault at priority 1: it has unzip
  # 6.0-58.el9_5, baseos 6.0-60.el9_8.
  @dnf-3
  Scenario: dnf4's repository priorities choose the version within the spec
    Given a dnf repository vault95 at "https://dl.rockylinux.org/vault/rocky/9.5/BaseOS/x86_64/os/" with priority 1
    And the system state is clean and dnf has no unzip
    And the manifest:
      """
      version: 2
      packages:
        - name: unzip
          spec: "*"
          install:
            linux:rhel: {backend: dnf}
      """
    When I run `bosun install . --yes --scope system`
    Then it succeeds
    And rpm reports unzip at 6.0-58.el9_5
    When the manifest becomes:
      """
      version: 2
      packages:
        - name: unzip
          spec: ">=6.0-59"
          install:
            linux:rhel: {backend: dnf}
      """
    And I run `bosun install . --yes --scope system`
    Then it succeeds
    And rpm reports unzip at 6.0-60.el9_8
    When the manifest is deleted
    And I run `bosun prune --yes --scope system`
    Then it succeeds
    And rpm reports unzip not installed

  # dnf never goes back past the installed build: with fedora raised to
  # priority 1, `dnf install wget2` keeps updates' 2.2.0 over fedora's 2.1.0,
  # and so does bosun's choice when `upgrade` resolves the spec afresh.
  @dnf5
  Scenario: dnf5 keeps the installed build over an older one of better priority
    Given the dnf index is refreshed
    And the system state is clean and dnf has no wget2
    And the manifest:
      """
      version: 2
      packages:
        - name: wget2
          spec: ">=2.2"
          install:
            linux:fedora: {backend: dnf}
      """
    When I run `bosun install . --yes --scope system`
    Then it succeeds
    And rpm reports wget2 at 2.2.0-3.fc41
    Given the dnf repository fedora has priority 1
    When the manifest becomes:
      """
      version: 2
      packages:
        - name: wget2
          spec: "*"
          install:
            linux:fedora: {backend: dnf}
      """
    And I run `bosun upgrade`
    Then it succeeds
    When I run `bosun install . --yes --scope system`
    Then it succeeds
    And it does not report "downgrade"
    And rpm reports wget2 at 2.2.0-3.fc41
    When the manifest is deleted
    And I run `bosun prune --yes --scope system`
    Then it succeeds
    And rpm reports wget2 not installed

  # Rocky 9 (dnf4): baseos' unzip 6.0-60.el9_8 installed, then the 9.5 vault
  # at priority 1 offering 6.0-58.el9_5.
  @dnf-3
  Scenario: dnf4 keeps the installed build over an older one of better priority
    Given the system state is clean and dnf has no unzip
    And the manifest:
      """
      version: 2
      packages:
        - name: unzip
          spec: ">=6.0-59"
          install:
            linux:rhel: {backend: dnf}
      """
    When I run `bosun install . --yes --scope system`
    Then it succeeds
    And rpm reports unzip at 6.0-60.el9_8
    Given a dnf repository vault95 at "https://dl.rockylinux.org/vault/rocky/9.5/BaseOS/x86_64/os/" with priority 1
    When the manifest becomes:
      """
      version: 2
      packages:
        - name: unzip
          spec: "*"
          install:
            linux:rhel: {backend: dnf}
      """
    And I run `bosun upgrade`
    Then it succeeds
    When I run `bosun install . --yes --scope system`
    Then it succeeds
    And it does not report "downgrade"
    And rpm reports unzip at 6.0-60.el9_8
    When the manifest is deleted
    And I run `bosun prune --yes --scope system`
    Then it succeeds
    And rpm reports unzip not installed

  @dnf-3
  Scenario: dnf4 without the versionlock plugin installs an exact spec unheld and says so
    Given the dnf index is refreshed
    And dnf has no versionlock command
    And the system state is clean and dnf has no tree
    And the manifest pins tree through dnf to the newest version dnf offers
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

  # An elevated run leaves the system state root's: every user reads it,
  # under a shared lock taken through a lock file opened read-only. State a
  # user may not read is left out, said once, and never touched.
  @apt-get @useradd @runuser
  Scenario: a run without elevation reads the system state an elevated run left, or leaves it out
    Given the apt index is refreshed
    And the system state is clean
    And the manifest:
      """
      version: 2
      packages:
        - name: hello
          spec: "*"
          install:
            linux:debian: {backend: apt}
      """
    When I run `bosun install . --yes --scope system`
    Then it succeeds
    Given a non-root user owns the project
    When the non-root user runs `bosun prune --yes`
    Then it succeeds
    And dpkg reports hello installed
    When the non-root user runs `bosun --format text status .`
    Then it succeeds
    And it says "(up to date)"
    And it does not warn "system state is not readable"
    When the system state is made readable only by root
    And the non-root user runs `bosun prune --yes`
    Then it succeeds
    And it warns "system state is not readable by this user"
    And the system state is unchanged
    And dpkg reports hello installed
    When the manifest is deleted
    And I run `bosun prune --yes --scope system`
    Then it succeeds
    And dpkg reports hello not installed

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
  Scenario: apk pins a package in the world only while its spec is exact, then prune deletes it
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
    And the apk world names tree unpinned
    When the manifest pins tree through apk to the version installed
    And I run `bosun install . --yes --scope system`
    Then it succeeds
    And the apk world pins tree to its installed version
    When the manifest becomes:
      """
      version: 2
      packages:
        - name: tree
          spec: "*"
          install:
            linux:alpine: {backend: apk}
      """
    And I run `bosun install . --yes --scope system`
    Then it succeeds
    And it reports "unhold"
    And apk reports tree installed
    And the apk world names tree unpinned
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

  # npm is user scope, so it runs as a non-root user (§5.5), under npm's own
  # global prefix. A system Node's is root's, so the user sets one of theirs.
  @npm @useradd @runuser
  Scenario: npm installs a tool under the user's npm prefix, moves it down, and prune removes it
    Given the manifest:
      """
      version: 2
      packages:
        - name: semver
          spec: "7.6.0"
          install: {backend: npm}
      """
    And the non-root user's npm prefix is ~/.npm-global
    And a non-root user owns the project
    When the non-root user runs `bosun install . --yes`
    Then it succeeds
    And it warns "npm's global bin dir is not on PATH"
    And ~/.npm-global/bin/semver exists and is executable
    And npm lists semver 7.6.0 under ~/.npm-global
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
    And npm lists semver 7.5.0 under ~/.npm-global
    When the manifest is deleted
    And the non-root user runs `bosun prune --yes`
    Then it succeeds
    And ~/.npm-global/bin/semver does not exist

  # npm resolves the spec itself: yarn's `latest` dist-tag is 1.22.22 though
  # 2.4.3 is published, and npm's picker prefers the tag.
  @npm @useradd @runuser
  Scenario: npm resolves the spec's range with its own picker
    Given the manifest:
      """
      version: 2
      packages:
        - name: yarn
          spec: "*"
          install: {backend: npm}
      """
    And the non-root user's npm prefix is ~/.npm-global
    And a non-root user owns the project
    When the non-root user runs `bosun install . --yes`
    Then it succeeds
    And npm lists yarn 1.22.22 under ~/.npm-global
    When the manifest becomes:
      """
      version: 2
      packages:
        - name: yarn
          spec: ">=2.0.0"
          install: {backend: npm}
      """
    And the non-root user runs `bosun install . --yes`
    Then it succeeds
    And npm lists yarn 2.4.3 under ~/.npm-global
    When the manifest is deleted
    And the non-root user runs `bosun prune --yes`
    Then it succeeds

  # Whether npm's prefix is writable matters only to a step that writes
  # there: with the tool already installed by root, nothing is planned and
  # nothing is warned of (and nothing is written to find out).
  @npm @useradd @runuser
  Scenario: npm's prefix is checked only when an install or uninstall is planned
    Given root has npm install semver@7.6.0 globally
    And the manifest:
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
    And it does not warn "npm install -g there needs root"
    When the non-root user runs `bosun status .`
    Then it succeeds
    And it does not warn "npm install -g there needs root"
    And root removes semver from npm's global prefix

  @npm @useradd @runuser
  Scenario: npm under a system Node's prefix needs root, and bosun says so
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
    Then it fails
    And it warns "npm install -g there needs root"

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

  # `uv tool install` reads no project configuration, so neither does the
  # resolution: a uv.toml where bosun runs, naming an index that cannot be
  # reached, changes nothing.
  @uv @useradd @runuser
  Scenario: uv resolves with the configuration uv tool install reads, not the project's
    Given the manifest:
      """
      version: 2
      packages:
        - name: ruff
          spec: ">=0.6.0|<0.7.0"
          install: {backend: uv}
      """
    And the project has a uv.toml naming an index that cannot be reached
    And a non-root user owns the project
    When the non-root user runs `bosun install . --yes`
    Then it succeeds
    And uv lists ruff 0.6.9
    When the manifest is deleted
    And the non-root user runs `bosun prune --yes`
    Then it succeeds
    And the binary ruff does not exist

  # uv resolves the spec itself, for the interpreter its tools use: under
  # Python 3.9 black's newest release that still supports it, 25.11.0, though
  # 26.x is published.
  @uv @python3.9 @useradd @runuser
  Scenario: uv resolves the spec for the interpreter its tools use
    Given the manifest:
      """
      version: 2
      packages:
        - name: black
          spec: "*"
          install: {backend: uv}
      """
    And a non-root user owns the project
    When the non-root user runs `bosun install . --yes`
    Then it succeeds
    And uv lists black 25.11.0
    When the manifest is deleted
    And the non-root user runs `bosun prune --yes`
    Then it succeeds

  # bosun orders versions itself (render, and claims across manifests, need
  # it where the tool is absent), so its ordering is held to the tool's own:
  # every version the index offers, sorted by bosun, must be in order by the
  # tool too, pair by adjacent pair, as must the conformance pairs.
  @apt-get @dpkg
  Scenario: bosun orders Debian versions as dpkg does
    Given the apt index is refreshed
    Then bosun orders the versions apt offers as dpkg --compare-versions does
    And bosun orders the Debian conformance pairs as dpkg --compare-versions does

  @dnf @rpm
  Scenario: bosun orders RPM versions as rpm does
    Given the dnf index is refreshed
    Then bosun orders the versions dnf offers as rpm.vercmp does
    And bosun orders the RPM conformance pairs as rpm.vercmp does

  # apk-tools 2 and 3 order a few versions differently; bosun follows the
  # major the apk backend probes.
  @apk
  Scenario: bosun orders Alpine versions as the host's apk does
    Given the apk index is refreshed
    And bosun follows the host's apk-tools
    Then bosun orders the versions apk offers as apk version -t does
    And bosun orders the Alpine conformance pairs as apk version -t does
