Feature: render reads the target machine from its image

  A container build installs into an image whose own files say what it is:
  `/etc/os-release` names the distro, its release and the families it is
  like, and apt's sources list the repositories its apt reads, a vendor's
  repository a Dockerfile added among them. `render --image <ref>` copies
  those files out of the image with podman or docker, without running it;
  `render --rootfs <dir>` reads them from a directory. apt's versions then
  come from the repositories the image lists, not from the ones a stock
  image of the release would.

  Background:
    Given a Debian trixie amd64 machine, not elevated
    And apt 2.9.8 is available
    And Debian bookworm lists jq 1.6-2.1+deb12u2 for amd64
    And Debian bookworm-security lists jq 1.6-2.1+deb12u3 for amd64
    And the vendor repository https://apt.vendor.example/repo lists tool 2.0-1
    And the target's file etc/os-release:
      """
      ID=debian
      VERSION_ID="12"
      VERSION_CODENAME=bookworm
      """
    And the target's file etc/apt/sources.list.d/debian.sources:
      """
      Types: deb
      URIs: http://deb.debian.org/debian
      Suites: bookworm
      Components: main
      """
    And the target's file etc/apt/sources.list.d/vendor.list:
      """
      deb [arch=amd64] https://apt.vendor.example/repo ./
      """
    And the manifest:
      """
      version: 2
      packages:
        - name: jq
          spec: "*"
          install:
            linux:debian@bookworm: {backend: apt}
        - name: tool
          spec: "*"
          install:
            linux:debian: {backend: apt}
      """

  Scenario: the target's own sources give apt's versions, its vendor repository's too
    When I run `bosun render env . --target linux/amd64 --rootfs target`
    Then it succeeds
    And it prints:
      """
      JQ_PACKAGE=jq
      JQ_VERSION=1.6-2.1+deb12u2
      TOOL_PACKAGE=tool
      TOOL_VERSION=2.0-1
      """
    And it does not warn "--distro"

  Scenario: a target root is another machine even on this machine's platform
    When I run `bosun render env . --rootfs target`
    Then it succeeds
    And it says "TOOL_VERSION=2.0-1"

  Scenario: a repository for another architecture is not read
    Given Debian bookworm lists jq 1.6-2.1+deb12u2 for arm64
    When I run `bosun render env . --target linux/arm64 --rootfs target`
    Then it fails
    And it reports "tool: unsatisfiable"

  Scenario: the target is read from one place
    When I run `bosun render env . --rootfs target --distro debian@bookworm`
    Then it refuses the command line
    And it reports "--distro"
    When I run `bosun render env . --image debian:12 --rootfs target`
    Then it refuses the command line
    And it reports "--rootfs"

  Scenario: the families the target is like come from its os-release
    Given Ubuntu noble lists jq 1.7.1-3build1 for amd64
    And the target's file etc/os-release:
      """
      ID=pop
      ID_LIKE="ubuntu debian"
      VERSION_ID="24.04"
      VERSION_CODENAME=noble
      """
    And the target's file etc/apt/sources.list.d/debian.sources:
      """
      Types: deb
      URIs: http://archive.ubuntu.com/ubuntu
      Suites: noble
      Components: main
      """
    And the manifest:
      """
      version: 2
      packages:
        - name: jq
          spec: "*"
          install:
            linux:ubuntu: {backend: apt}
      """
    When I run `bosun render env . --target linux/amd64 --rootfs target`
    Then it succeeds
    And it says "JQ_VERSION=1.7.1-3build1"
