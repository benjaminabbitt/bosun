Feature: github-release names the archive by template and finds the binary inside it

  Releases spell the same platform differently, and put the binary at the
  archive's root or inside a directory. The `ext.github-release.asset` template names the archive
  exactly; bosun then finds the one file named after the binary, wherever it is.

  Background:
    Given a Linux amd64 machine, not elevated
    And bosun installs GitHub releases itself

  Scenario: duf spells its platform linux_x86_64 and keeps the binary at the root
    Given muesli/duf v0.8.1 publishes duf_0.8.1_linux_x86_64.tar.gz with duf at its root
    And muesli/duf v0.8.1 also publishes duf_0.8.1_linux_arm64.tar.gz
    And muesli/duf v0.8.1 publishes checksums.txt
    And the manifest:
      """
      version: 1
      targets: [linux/amd64]
      packages:
        - name: duf
          spec: ">=0.8.0|<0.9.0"
          install:
            linux:
              backend: github-release
              package: muesli/duf
              ext:
                github-release:
                  asset: "{{ binary }}_{{ version }}_{{ os }}_{{ arch_goreleaser }}.tar.gz"
      """
    When I run `bosun install . --yes`
    Then it succeeds
    And duf is installed from duf_0.8.1_linux_x86_64.tar.gz
    And bosun has recorded duf 0.8.1
    And it does not warn "installing unverified"
    When I run `bosun uninstall . --yes`
    Then it succeeds
    And duf is not installed
    And bosun has recorded nothing about duf

  Scenario: gum spells its platform Linux_x86_64 and wraps the binary in a directory
    Given charmbracelet/gum v2.0.2 publishes gum_2.0.2_Linux_x86_64.tar.gz with gum in gum_2.0.2_Linux_x86_64/
    And charmbracelet/gum v2.0.2 also publishes gum_2.0.2_Linux_arm64.tar.gz
    And charmbracelet/gum v2.0.2 publishes checksums.txt
    And the manifest:
      """
      version: 1
      targets: [linux/amd64]
      packages:
        - name: gum
          spec: ">=2.0.0|<3.0.0"
          install:
            linux:
              backend: github-release
              package: charmbracelet/gum
              ext:
                github-release:
                  asset: "{{ binary }}_{{ version }}_{{ os | title }}_{{ arch_goreleaser }}.tar.gz"
      """
    When I run `bosun install . --yes`
    Then it succeeds
    And gum is installed from gum_2.0.2_Linux_x86_64.tar.gz
    And bosun has recorded gum 2.0.2
    When I run `bosun uninstall . --yes`
    Then it succeeds
    And gum is not installed
    And bosun has recorded nothing about gum

  Scenario: a release that publishes no checksums installs with a warning
    Given muesli/duf v0.8.1 publishes duf_0.8.1_linux_x86_64.tar.gz with duf at its root
    And the manifest:
      """
      version: 1
      targets: [linux/amd64]
      packages:
        - name: duf
          spec: ">=0.8.0|<0.9.0"
          install:
            linux:
              backend: github-release
              package: muesli/duf
              ext:
                github-release:
                  asset: "{{ binary }}_{{ version }}_{{ os }}_{{ arch_goreleaser }}.tar.gz"
      """
    When I run `bosun install . --yes`
    Then it succeeds
    And duf is installed from duf_0.8.1_linux_x86_64.tar.gz
    And it warns "installing unverified"
    And it warns "duf_0.8.1_linux_x86_64.tar.gz"

  Scenario: a template naming an unknown variable installs nothing
    Given muesli/duf v0.8.1 publishes duf_0.8.1_linux_x86_64.tar.gz with duf at its root
    And the manifest:
      """
      version: 1
      targets: [linux/amd64]
      packages:
        - name: duf
          spec: ">=0.8.0|<0.9.0"
          install:
            linux:
              backend: github-release
              package: muesli/duf
              ext:
                github-release:
                  asset: "{{ binary }}_{{ version }}_{{ platform }}.tar.gz"
      """
    When I run `bosun install . --yes`
    Then it fails
    And it reports "unknown variable `platform`"
    And duf is not installed

  Scenario: a misspelt ext key is a manifest error that names the allowed keys
    Given the manifest:
      """
      version: 1
      targets: [linux/amd64]
      packages:
        - name: duf
          spec: ">=0.8.0|<0.9.0"
          install:
            linux:
              backend: github-release
              package: muesli/duf
              ext:
                github-release:
                  aset: "duf.tar.gz"
      """
    When I run `bosun lock .`
    Then it fails
    And it reports "unknown key \"aset\" in ext.github-release (allowed: package, asset, tag)"

  Scenario: asset written outside ext is an unknown key
    Given the manifest:
      """
      version: 1
      targets: [linux/amd64]
      packages:
        - name: duf
          spec: ">=0.8.0|<0.9.0"
          install:
            linux:
              backend: github-release
              package: muesli/duf
              asset: "duf.tar.gz"
      """
    When I run `bosun lock .`
    Then it fails
    And it reports "unknown key \"asset\" in an install block"
