Feature: github-release packages are locked with their release checksum

  Background:
    Given a fixture:
      """
      {"os":"linux","arch":"amd64","privilege":"unelevated","user":"t",
       "backends":[{"id":"github-release","version":"1.0.0","purl_type":"github","vers_type":"semver",
         "platform_independent":false,"scope":"user",
         "available":{"ctxloom/ctxloom":["0.6.4","0.6.5"]},
         "checksums":{"ctxloom/ctxloom":"sha256:4f1c0a9e2b7d"}}]}
      """
    And a manifest:
      """
      version: 1
      targets: [linux/amd64]
      packages:
        - name: ctxloom
          spec: ">=0.6.4|<0.7.0"
          install:
            linux: {backend: github-release, package: ctxloom/ctxloom}
      """

  Scenario: lock records the checksum the release publishes
    When I run "lock ."
    Then the exit code is 0
    And a lockfile is written
    And the lockfile contains "0.6.5"
    And the lockfile contains "sha256:4f1c0a9e2b7d"

  Scenario: a github-release install needs no hold and warns about none
    When I run "--format json install . --plan"
    Then the exit code is 2
    And stdout contains "install"
    And stdout does not contain "hold"
    And stdout does not contain "not pinned"

  Scenario: --frozen accepts a locked version inside a semver range
    When I run "lock ."
    And I run "--format json install . --yes --frozen"
    Then the exit code is 0
    And stdout contains "applied"
    And the recorded state contains "0.6.5"
