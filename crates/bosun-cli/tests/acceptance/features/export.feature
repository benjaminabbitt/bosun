Feature: bosun export seeds a manifest from what this machine has installed by hand

  `export` asks each package manager what the user installed themselves, never
  for everything installed: apt's manual marks, less the distro's base system
  (priority required, important or standard); every crates.io crate cargo
  installed; and what bosun itself installed from GitHub releases (binaries,
  .deb and .rpm packages, installers), with the options each was installed
  with. Each becomes an entry like `add` writes: the range from the installed
  version up to the next breaking one, under `any` for a platform-independent
  backend, else this machine's OS. Entries the manifest already has are left as they are. It shows
  what it adds and asks first; with no terminal to ask on, it writes. Then it
  locks. It never installs.

  Background:
    Given a Debian trixie amd64 machine, not elevated
    And apt 3.0.3 is available
    And cargo 1.76.0 is available
    And apt publishes jq 1.7.1-6+deb13u4
    And apt publishes libonig5 6.9.9-1+b1
    And apt publishes bash 5.2.37-2+b10
    And cargo publishes ripgrep 14.1.1

  Scenario: export writes what was installed by hand, not the base system, and locks it
    Given jq 1.7.1-6+deb13u4 is installed by hand
    And libonig5 6.9.9-1+b1 is installed outside bosun
    And bash 5.2.37-2+b10 is part of the base system (priority required), marked as installed by hand
    And ripgrep 14.1.1 is installed by hand
    When I run `bosun --format text export --yes`
    Then it succeeds
    And it says "Added 2 packages"
    And the manifest reads:
      """
      version: 2
      packages:
        - name: jq
          spec: ">=1.7.1|<1.7.2~"
          install:
            linux: {backend: apt}
        - name: ripgrep
          spec: ">=14.1.1|<15.0.0"
          install: {backend: cargo}
      """
    And the lock pins jq 1.7.1-6+deb13u4
    And the lock pins ripgrep 14.1.1
    And bosun has recorded nothing about jq

  Scenario: export --plan shows what it would add and writes nothing
    Given jq 1.7.1-6+deb13u4 is installed by hand
    And ripgrep 14.1.1 is installed by hand
    When I run `bosun --format text export --plan`
    Then it succeeds
    And it says "jq"
    And it says "ripgrep"
    And no manifest is written
    And no lock is written

  Scenario: export adds only what the manifest does not have, keeping its comments
    Given jq 1.7.1-6+deb13u4 is installed by hand
    And ripgrep 14.1.1 is installed by hand
    And the manifest:
      """
      # my tools
      version: 2
      packages:
        # what CI has
        - name: jq
          spec: "1.7.1-6+deb13u4"
          install:
            linux:debian: {backend: apt}
      """
    When I run `bosun --format text export`
    Then it succeeds
    And it says "jq (apt): already in the manifest"
    And the manifest reads:
      """
      # my tools
      version: 2
      packages:
        # what CI has
        - name: jq
          spec: "1.7.1-6+deb13u4"
          install:
            linux:debian: {backend: apt}
        - name: ripgrep
          spec: ">=14.1.1|<15.0.0"
          install: {backend: cargo}
      """
    And the lock pins jq 1.7.1-6+deb13u4
    And the lock pins ripgrep 14.1.1

  Scenario: export writes its entries but fails when another entry can't be locked
    Given ripgrep 14.1.1 is installed by hand
    And the manifest:
      """
      version: 2
      packages:
        - name: just
          spec: ">=99.0.0"
          install: {backend: cargo}
      """
    And cargo publishes just 1.40.0
    When I run `bosun --format text export --yes`
    Then it fails
    And it reports "Added 1 package"
    And it reports "just: unsatisfiable"
    And no lock is written

  Scenario: a package whose version the backend no longer offers is left out
    Given cargo publishes tokei 13.0.0
    And tokei 12.1.2 is installed by hand
    And ripgrep 14.1.1 is installed by hand
    When I run `bosun --format text export --yes`
    Then it succeeds
    And it says "tokei (cargo): unsatisfiable >=12.1.2|<13.0.0"
    And the manifest reads:
      """
      version: 2
      packages:
        - name: ripgrep
          spec: ">=14.1.1|<15.0.0"
          install: {backend: cargo}
      """
    And the lock pins ripgrep 14.1.1

  Scenario: with nothing installed by hand, export writes nothing
    Given libonig5 6.9.9-1+b1 is installed outside bosun
    And bash 5.2.37-2+b10 is part of the base system (priority required), marked as installed by hand
    When I run `bosun --format text export --yes`
    Then it succeeds
    And it says "Nothing to export"
    And no manifest is written
    And no lock is written

  Scenario: export takes a GitHub release from what bosun installed
    Given bosun installs GitHub releases itself
    And muesli/duf v0.8.1 publishes duf_0.8.1_linux_x86_64.tar.gz with duf at its root
    And the manifest:
      """
      version: 2
      packages:
        - name: duf
          spec: ">=0.8.1|<0.9.0"
          install:
            linux:
              backends:
                github-release: {package: muesli/duf, asset: "{{ binary }}_{{ version }}_{{ os }}_{{ arch_goreleaser }}.tar.gz"}
      """
    When I run `bosun install . --yes`
    And I run `bosun export --yes` in an empty directory
    Then it succeeds
    And the file elsewhere/bosun.yaml reads:
      """
      version: 2
      packages:
        - name: duf
          spec: ">=0.8.1|<0.9.0"
          install:
            linux:
              backends:
                github-release: {package: muesli/duf, asset: "{{ binary }}_{{ version }}_{{ os }}_{{ arch_goreleaser }}.tar.gz"}
      """

  Scenario: export takes a release .deb from what bosun installed
    Given a Debian trixie amd64 machine, elevated
    And bosun installs release .deb packages
    And sharkdp/bat v0.26.1 publishes bat_0.26.1_amd64.deb holding just the binary
    And sharkdp/bat v0.26.1 publishes checksums.txt
    And the manifest:
      """
      version: 2
      packages:
        - name: bat
          spec: ">=0.26.0|<0.27.0"
          install:
            linux:
              backends:
                github-deb: {package: sharkdp/bat, asset: "{{ binary }}_{{ version }}_{{ arch }}.deb"}
      """
    When I run `bosun install . --yes`
    And I run `bosun export --yes` in an empty directory
    Then it succeeds
    And the file elsewhere/bosun.yaml reads:
      """
      version: 2
      packages:
        - name: bat
          spec: ">=0.26.1|<0.27.0"
          install:
            linux:
              backends:
                github-deb: {package: sharkdp/bat, asset: "{{ binary }}_{{ version }}_{{ arch }}.deb"}
      """

  Scenario: export takes a release .rpm with its package name and checksum waiver
    Given a Linux amd64 machine, elevated
    And bosun installs release .rpm packages
    And sharkdp/bat v0.26.1 publishes bat-0.26.1-1.x86_64.rpm holding just the binary
    And the manifest:
      """
      version: 2
      packages:
        - name: bat
          spec: ">=0.26.0|<0.27.0"
          install:
            linux:
              backends:
                github-rpm: {package: sharkdp/bat, asset: "{{ binary }}-{{ version }}-1.{{ arch_uname }}.rpm", package_name: bat-cat, unverified: allow}
      """
    When I run `bosun install . --yes`
    And I run `bosun export --yes` in an empty directory
    Then it succeeds
    And the file elsewhere/bosun.yaml reads:
      """
      version: 2
      packages:
        - name: bat
          spec: ">=0.26.1|<0.27.0"
          install:
            linux:
              backends:
                github-rpm: {package: sharkdp/bat, asset: "{{ binary }}-{{ version }}-1.{{ arch_uname }}.rpm", package_name: bat-cat, unverified: allow}
      """
