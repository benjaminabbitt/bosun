Feature: an install block can name the machine's C library

  A release binary linked against glibc doesn't run on Alpine, whose C library
  is musl, and one linked against a newer glibc fails on an older one. So the
  selectors `linux:gnu` and `linux:musl` name the C library, spelled as Rust
  target triples spell it, and `linux:gnu@>=2.31` adds a glibc floor (a vers
  generic range). bosun reads the C library from the loader /bin/sh uses. A
  distro selector still wins over a libc one: it names a package source, libc
  only a binary's ABI. The github-release asset template can spell the C
  library too, as `{{ libc }}` or the whole `{{ rust_target }}`.

  Background:
    Given bosun installs GitHub releases itself
    And astral-sh/uv v0.9.2 publishes uv-x86_64-unknown-linux-gnu.tar.gz with uv in uv-x86_64-unknown-linux-gnu/
    And astral-sh/uv v0.9.2 publishes uv-x86_64-unknown-linux-musl.tar.gz with uv in uv-x86_64-unknown-linux-musl/
    And astral-sh/uv v0.9.2 publishes checksums.txt
    And the manifest:
      """
      version: 2
      packages:
        - name: uv
          spec: ">=0.9.0|<0.10.0"
          install:
            linux:
              backends:
                github-release:
                  package: astral-sh/uv
                  asset: "{{ binary }}-{{ arch_uname }}-unknown-linux-musl.tar.gz"
            linux:gnu@>=2.28:
              backends:
                github-release:
                  package: astral-sh/uv
                  asset: "{{ binary }}-{{ arch_uname }}-unknown-linux-gnu.tar.gz"
      """

  Scenario: a glibc machine above the floor installs the gnu build
    Given a Debian bookworm amd64 machine, not elevated
    And the machine's C library is glibc 2.36
    When I run `bosun install . --yes`
    Then it succeeds
    And uv is installed from uv-x86_64-unknown-linux-gnu.tar.gz

  Scenario: a musl machine installs the musl build
    Given an Alpine amd64 machine, not elevated
    And the machine's C library is musl 1.2.5
    When I run `bosun install . --yes`
    Then it succeeds
    And uv is installed from uv-x86_64-unknown-linux-musl.tar.gz

  Scenario: a glibc older than the floor falls back to the plain linux block
    Given a Linux amd64 machine, not elevated
    And the machine's C library is glibc 2.17
    When I run `bosun install . --yes`
    Then it succeeds
    And uv is installed from uv-x86_64-unknown-linux-musl.tar.gz

  Scenario: a machine whose C library is unknown matches no libc selector
    Given a Linux amd64 machine, not elevated
    When I run `bosun install . --yes`
    Then it succeeds
    And uv is installed from uv-x86_64-unknown-linux-musl.tar.gz

  Scenario: the asset template spells the C library itself
    Given an Alpine amd64 machine, not elevated
    And the machine's C library is musl 1.2.5
    And the manifest:
      """
      version: 2
      packages:
        - name: uv
          spec: ">=0.9.0|<0.10.0"
          install:
            linux:
              backends:
                github-release:
                  package: astral-sh/uv
                  asset: "{{ binary }}-{{ rust_target }}.tar.gz"
      """
    When I run `bosun install . --yes`
    Then it succeeds
    And uv is installed from uv-x86_64-unknown-linux-musl.tar.gz

  Scenario: the same template on a glibc machine names the gnu build
    Given a Debian trixie amd64 machine, not elevated
    And the machine's C library is glibc 2.41
    And the manifest:
      """
      version: 2
      packages:
        - name: uv
          spec: ">=0.9.0|<0.10.0"
          install:
            linux:
              backends:
                github-release:
                  package: astral-sh/uv
                  asset: "{{ binary }}-{{ arch_uname }}-unknown-linux-{{ libc }}.tar.gz"
      """
    When I run `bosun install . --yes`
    Then it succeeds
    And uv is installed from uv-x86_64-unknown-linux-gnu.tar.gz

  Scenario: a template that spells the C library fails where it is unknown
    Given a Linux amd64 machine, not elevated
    And the manifest:
      """
      version: 2
      packages:
        - name: uv
          spec: ">=0.9.0|<0.10.0"
          install:
            linux:
              backends:
                github-release:
                  package: astral-sh/uv
                  asset: "{{ binary }}-{{ rust_target }}.tar.gz"
      """
    When I run `bosun install . --yes`
    Then it fails
    And it reports "the host's C library is unknown"
    And uv is not installed

  Scenario: a distro block wins over a libc block
    Given a Debian bookworm amd64 machine, not elevated
    And the machine's C library is glibc 2.36
    And apt 2.6.1 is available
    And apt publishes jq 1.6-6
    And github-release publishes jqlang/jq 1.7.1
    And the manifest:
      """
      version: 2
      packages:
        - name: jq
          spec: "*"
          install:
            linux:gnu: {backend: github-release, package: jqlang/jq}
            linux:debian: {backend: apt}
      """
    When I run `bosun lock .`
    Then it succeeds
    And the lock pins jq 1.6-6

  Scenario: a libc range that is not a vers range is refused
    Given a Linux amd64 machine, not elevated
    And the manifest:
      """
      version: 2
      packages:
        - name: uv
          install:
            "linux:gnu@>=2.31|<2.28": {backend: github-release, package: astral-sh/uv}
      """
    When I run `bosun lock .`
    Then it fails
    And it reports "invalid selector"
    And it reports "invalid libc range"

  Scenario: bosun backends shows the machine's C library
    Given an Alpine amd64 machine, not elevated
    And the machine's C library is musl 1.2.5
    When I run `bosun --format text backends`
    Then it succeeds
    And it says "libc: musl 1.2.5"

  Scenario: bosun backends reports the C library as JSON
    Given an Alpine amd64 machine, not elevated
    And the machine's C library is musl 1.2.5
    When I run `bosun --format json backends`
    Then it succeeds
    And the JSON reports libc musl 1.2.5

  Scenario: bosun backends says when the C library is unknown
    Given a Linux amd64 machine, not elevated
    When I run `bosun --format text backends`
    Then it succeeds
    And it says "libc: unknown"
    When I run `bosun --format json backends`
    Then the JSON reports no libc

  Scenario: bosun status shows the C library selectors were matched against
    Given a Debian bookworm amd64 machine, not elevated
    And the machine's C library is glibc 2.36
    When I run `bosun --format text status .`
    Then it succeeds
    And it says "libc: gnu 2.36"
