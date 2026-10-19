Feature: under sudo, user-scope backends are asked as the invoking user

  An elevated run asks each user-scope backend what the user installed as
  that user: cargo, npm and uv run as them, with their HOME and a minimal
  environment, and github-release looks in their ~/.local/bin, as them, so
  `sudo bosun status` sees the user's own installs, not root's. Changing a
  user-scope package still refuses elevation. The seam's other backends
  run no programs; the runner that becomes the user is checked in
  bosun-host and in the real tier.

  Background:
    Given a Linux amd64 machine, not elevated
    And bosun installs GitHub releases itself
    And muesli/duf v0.8.1 publishes duf_0.8.1_linux_x86_64.tar.gz with duf at its root
    And muesli/duf v0.8.1 publishes checksums.txt
    And the manifest:
      """
      version: 2
      packages:
        - name: duf
          spec: ">=0.8.0|<0.9.0"
          install:
            linux:
              backends:
                github-release:
                  package: muesli/duf
                  asset: "{{ binary }}_{{ version }}_{{ os }}_{{ arch_goreleaser }}.tar.gz"
      """
    And duf is in ~/.local/bin, put there outside bosun

  Scenario: status under sudo finds a release binary in the user's ~/.local/bin, as them
    When later runs are elevated
    And I run `bosun -vv --format text status .`
    Then it succeeds
    And it says "present, not managed"
    And it does "exists" on duf as the invoking user

  Scenario: status in root's own login looks in root's own ~/.local/bin
    When later runs are root's own login
    And I run `bosun --format text status .`
    Then it succeeds
    And it says "not installed"
