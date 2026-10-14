Feature: real backends against a disposable container

  Scenario: apt installs and holds a package, then prune removes and unholds it
    Given the apt index is refreshed
    And the system state is clean
    And a manifest:
      """
      version: 1
      targets: [linux/amd64]
      packages:
        - name: hello
          spec: ">=2.0"
          install:
            linux:debian: {backend: apt}
      """
    When I run "install . --yes --scope system"
    Then the exit code is 0
    And dpkg reports "hello" installed
    And apt-mark reports "hello" held
    When the manifest is deleted
    And I run "prune --yes --scope system"
    Then the exit code is 0
    And dpkg reports "hello" not installed
    And apt-mark reports "hello" not held

  # github-release is user scope, so it runs as a non-root user (§5.5).
  Scenario: github-release verifies the checksum and installs an executable
    Given a manifest:
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
    When I run "--format json install . --plan" as a non-root user
    Then the exit code is 2
    And stdout contains "89a356c697c9458929ecf851bc85da3a9548d19aa978837b54c0b0b27100810f"
    When I run "install . --yes" as a non-root user
    Then the exit code is 0
    And the binary "ctxloom" exists and is executable
    And the bin dir has no temp files
