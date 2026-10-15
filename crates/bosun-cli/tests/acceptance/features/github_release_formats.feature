Feature: github-release installs from every common archive format

  Releases pack their binary as a tar under several compressions, as a zip,
  as one compressed file, or as the binary itself. bosun reads the format from
  the asset's name; a file that is not a binary (a .deb, a signature) is
  refused when bosun resolves, before anything downloads.

  Background:
    Given a Linux amd64 machine, not elevated
    And bosun installs GitHub releases itself

  Scenario Outline: a <format> archive with the binary in a wrapping directory
    Given o/tool v1.2.0 publishes <asset> with tool in tool-1.2.0/
    And the manifest:
      """
      version: 1
      targets: [linux/amd64]
      packages:
        - name: tool
          spec: ">=1.0.0|<2.0.0"
          install:
            linux:
              backend: github-release
              package: o/tool
              ext:
                github-release: {asset: "{{ binary }}-{{ version }}-{{ arch_uname }}-linux.<format>"}
      """
    When I run `bosun install . --yes`
    Then it succeeds
    And tool is installed from <asset>
    And it warns "installing unverified"

    Examples:
      | format  | asset                           |
      | tar.xz  | tool-1.2.0-x86_64-linux.tar.xz  |
      | tar.zst | tool-1.2.0-x86_64-linux.tar.zst |
      | tar.bz2 | tool-1.2.0-x86_64-linux.tar.bz2 |
      | tar     | tool-1.2.0-x86_64-linux.tar     |
      | zip     | tool-1.2.0-x86_64-linux.zip     |

  Scenario: restic ships one bzip2-compressed binary
    Given restic/restic v0.18.1 publishes restic_0.18.1_linux_amd64.bz2 holding just the binary
    And the manifest:
      """
      version: 1
      targets: [linux/amd64]
      packages:
        - name: restic
          spec: ">=0.18.0|<0.19.0"
          install:
            linux:
              backend: github-release
              package: restic/restic
              ext:
                github-release: {asset: "{{ binary }}_{{ version }}_{{ os }}_{{ arch }}.bz2"}
      """
    When I run `bosun install . --yes`
    Then it succeeds
    And restic is installed from restic_0.18.1_linux_amd64.bz2

  Scenario: kind ships the bare binary
    Given kubernetes-sigs/kind v0.30.0 publishes kind-linux-amd64 holding just the binary
    And the manifest:
      """
      version: 1
      targets: [linux/amd64]
      packages:
        - name: kind
          spec: ">=0.30.0|<0.31.0"
          install:
            linux:
              backend: github-release
              package: kubernetes-sigs/kind
              ext:
                github-release: {asset: "{{ binary }}-{{ os }}-{{ arch }}"}
      """
    When I run `bosun install . --yes`
    Then it succeeds
    And kind is installed from kind-linux-amd64
    When I run `bosun uninstall . --yes`
    Then it succeeds
    And kind is not installed

  Scenario: a template naming a .deb is refused at lock
    Given sharkdp/bat v0.26.1 publishes bat_0.26.1_amd64.deb holding just the binary
    And the manifest:
      """
      version: 1
      targets: [linux/amd64]
      packages:
        - name: bat
          spec: ">=0.26.0|<0.27.0"
          install:
            linux:
              backend: github-release
              package: sharkdp/bat
              ext:
                github-release: {asset: "{{ binary }}_{{ version }}_{{ arch }}.deb"}
      """
    When I run `bosun lock .`
    Then it fails
    And it reports "not a binary bosun can install"
    And no lock is written
