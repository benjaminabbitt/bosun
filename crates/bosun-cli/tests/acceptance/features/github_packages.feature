Feature: github-deb and github-rpm install the OS package a release attaches

  Many projects attach a .deb and an .rpm to each release. github-deb and
  github-rpm find them the github-release way (tag, asset template, checksum)
  and install them through apt or dnf, machine-wide, so dependencies resolve
  and the package manager knows the package. They install as root, so a
  release that publishes no checksum is refused unless the entry says
  `unverified: allow`.

  Scenario: bat's .deb is installed through apt, held, and removed
    Given a Debian trixie amd64 machine, elevated
    And bosun installs release .deb packages
    And sharkdp/bat v0.26.1 publishes bat_0.26.1_amd64.deb holding just the binary
    And sharkdp/bat v0.26.1 also publishes bat-v0.26.1-x86_64-unknown-linux-musl.tar.gz
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
                github-deb:
                  package: sharkdp/bat
                  asset: "{{ binary }}_{{ version }}_{{ arch }}.deb"
      """
    When I run `bosun --format json install . --plan`
    Then it reports changes pending
    And it says "apt-get"
    And it says "bat_0.26.1_amd64.deb"
    And it says "hold"
    When I run `bosun install . --yes`
    Then it succeeds
    And bosun has recorded bat 0.26.1
    When I run `bosun install . --plan`
    Then it succeeds
    When I run `bosun uninstall . --yes`
    Then it succeeds
    And bosun has recorded nothing about bat

  Scenario: an .rpm is installed through dnf under its own package name
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
                github-rpm:
                  package: sharkdp/bat
                  asset: "{{ binary }}-{{ version }}-1.{{ arch_uname }}.rpm"
                  package_name: bat-cat
                  unverified: allow
      """
    When I run `bosun --format json install . --plan`
    Then it says "dnf"
    And it says "github-rpm/bat-cat/bat-0.26.1-1.x86_64.rpm"
    And it says "unsupported-hold"
    When I run `bosun install . --yes`
    Then it succeeds
    And bosun has recorded bat 0.26.1
    When I run `bosun uninstall . --yes`
    Then it succeeds
    And bosun has recorded nothing about bat

  Scenario: a system-scope package needs elevation
    Given a Debian trixie amd64 machine, not elevated
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
                github-deb:
                  package: sharkdp/bat
                  asset: "{{ binary }}_{{ version }}_{{ arch }}.deb"
      """
    When I run `bosun install . --yes`
    Then it fails
    And it reports "sudo"
    And bosun has recorded nothing about bat

  Scenario: a github-deb asset template naming a tarball is refused
    Given a Debian trixie amd64 machine, elevated
    And bosun installs release .deb packages
    And sharkdp/bat v0.26.1 publishes bat-v0.26.1-x86_64-unknown-linux-musl.tar.gz with bat at its root
    And the manifest:
      """
      version: 2
      packages:
        - name: bat
          spec: ">=0.26.0|<0.27.0"
          install:
            linux:
              backends:
                github-deb:
                  package: sharkdp/bat
                  asset: "{{ binary }}-{{ tag }}-{{ arch_uname }}-unknown-linux-musl.tar.gz"
      """
    When I run `bosun lock .`
    Then it fails
    And it reports "not a .deb package"

  Scenario: a release .deb without a checksum is refused unless the entry allows it
    Given a Debian trixie amd64 machine, elevated
    And bosun installs release .deb packages
    And sharkdp/bat v0.26.1 publishes bat_0.26.1_amd64.deb holding just the binary
    And the manifest:
      """
      version: 2
      packages:
        - name: bat
          spec: ">=0.26.0|<0.27.0"
          install:
            linux:
              backends:
                github-deb:
                  package: sharkdp/bat
                  asset: "{{ binary }}_{{ version }}_{{ arch }}.deb"
      """
    When I run `bosun install . --yes`
    Then it fails
    And it reports "the release publishes no checksum for it"
    And it reports "set backends.github-deb.unverified: allow"
    And bosun has recorded nothing about bat
