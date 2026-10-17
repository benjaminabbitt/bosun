Feature: render gives a container build the versions this machine locked

  A Dockerfile takes the versions it installs as build arguments or
  environment variables, and `bosun render` supplies them: `render args` as
  `--build-arg` flags for `docker build`, `render env` as `NAME=value` lines
  for an env file, and `render json` as a versions file shaped like the lock,
  one entry per target platform. Each package gives two variables named after
  its logical name, upper-cased with every other character an underscore:
  `FD_PACKAGE`, the package manager's own name for it, and `FD_VERSION`. An
  entry's `var` names them instead. For this machine the versions are the ones
  its lock pins, so a lock that is missing or older than the manifest is an
  error that says to run `bosun lock`.

  Background:
    Given a Debian trixie amd64 machine, not elevated
    And apt 2.9.8 is available
    And cargo 1.76.0 is available
    And apt publishes fd-find 9.0.0-1
    And cargo publishes ripgrep 14.1.1
    And the manifest:
      """
      version: 2
      packages:
        - name: fd
          spec: "*"
          install:
            linux:debian: {backend: apt, package: fd-find}
        - name: ripgrep
          spec: ">=14.0.0|<15.0.0"
          install: {backend: cargo}
      """

  Scenario: render args gives docker build one --build-arg per variable
    When I run `bosun lock .`
    And I run `bosun render args .`
    Then it succeeds
    And it prints:
      """
      --build-arg FD_PACKAGE=fd-find --build-arg FD_VERSION=9.0.0-1 --build-arg RIPGREP_PACKAGE=ripgrep --build-arg RIPGREP_VERSION=14.1.1
      """

  Scenario: render env gives one NAME=value line per variable
    When I run `bosun lock .`
    And I run `bosun render env .`
    Then it succeeds
    And it prints:
      """
      FD_PACKAGE=fd-find
      FD_VERSION=9.0.0-1
      RIPGREP_PACKAGE=ripgrep
      RIPGREP_VERSION=14.1.1
      """

  Scenario: render json lists each package under this machine's platform
    When I run `bosun lock .`
    And I run `bosun render json .`
    Then it succeeds
    And it prints a JSON document
    And the versions file gives fd 9.0.0-1 as fd-find from apt for linux/amd64
    And the versions file gives ripgrep 14.1.1 as ripgrep from cargo for linux/amd64

  Scenario: render gives the version the lock pins, not the newest published
    When I run `bosun lock .`
    And apt publishes fd-find 9.0.0-1, 10.2.0-1
    And I run `bosun render env .`
    Then it succeeds
    And it says "FD_VERSION=9.0.0-1"

  Scenario: render without a lock for this manifest says to lock it
    When I run `bosun render args .`
    Then it fails
    And it reports "run `bosun lock`"

  Scenario: render refuses a lock the manifest has moved on from
    When I run `bosun lock .`
    And the manifest is replaced by:
      """
      version: 2
      packages:
        - name: fd
          spec: ">=9|<10"
          install:
            linux:debian: {backend: apt, package: fd-find}
      """
    And I run `bosun render args .`
    Then it fails
    And it reports "changed since it was locked"
    And it reports "run `bosun lock`"

  Scenario: an entry with no block for this machine is left out
    Given the manifest:
      """
      version: 2
      packages:
        - name: fd
          spec: "*"
          install:
            linux:debian: {backend: apt, package: fd-find}
        - name: winfetch
          spec: "*"
          install:
            windows: {backend: winget}
      """
    When I run `bosun lock .`
    And I run `bosun render env .`
    Then it succeeds
    And it prints:
      """
      FD_PACKAGE=fd-find
      FD_VERSION=9.0.0-1
      """

  Scenario: two names that make the same variable are an error naming both
    Given the manifest:
      """
      version: 2
      packages:
        - name: fd-find
          spec: "*"
          install:
            linux:debian: {backend: apt}
        - name: fd_find
          spec: "*"
          install:
            linux:debian: {backend: apt, package: fd-find}
      """
    When I run `bosun lock .`
    And I run `bosun render args .`
    Then it fails
    And it reports "FD_FIND"
    And it reports "fd-find"
    And it reports "fd_find"

  Scenario: an entry's var names its variables
    Given the manifest:
      """
      version: 2
      packages:
        - name: fd-find
          spec: "*"
          install:
            linux:debian: {backend: apt}
        - name: fd_find
          spec: "*"
          var: FD
          install:
            linux:debian: {backend: apt, package: fd-find}
      """
    When I run `bosun lock .`
    And I run `bosun render env .`
    Then it succeeds
    And it prints:
      """
      FD_FIND_PACKAGE=fd-find
      FD_FIND_VERSION=9.0.0-1
      FD_PACKAGE=fd-find
      FD_VERSION=9.0.0-1
      """

  Scenario: a var that is not a variable name is a manifest error
    Given the manifest:
      """
      version: 2
      packages:
        - name: fd
          spec: "*"
          var: fd-tool
          install:
            linux:debian: {backend: apt, package: fd-find}
      """
    When I run `bosun lock .`
    Then it fails
    And it reports "var"
    And it reports "fd-tool"

  Scenario: a name that would start a variable with a digit needs a var
    Given apt publishes 7zip 23.01+dfsg-11
    And the manifest:
      """
      version: 2
      packages:
        - name: 7zip
          spec: "*"
          install:
            linux:debian: {backend: apt}
      """
    When I run `bosun lock .`
    And I run `bosun render env .`
    Then it fails
    And it reports "7ZIP"
    And it reports "set `var`"
