Feature: install, upgrade and cross-claim resolution at user scope (cargo)

  Background:
    Given a fixture:
      """
      {"os":"linux","arch":"amd64","privilege":"unelevated","user":"t",
       "backends":[{"id":"cargo","version":"1.76.0","purl_type":"cargo","vers_type":"cargo",
         "platform_independent":true,"scope":"user","supports_hold":false,
         "available":{"ripgrep":["14.0.0","14.1.1"]}}]}
      """
    And a manifest:
      """
      version: 1
      targets: [linux/amd64]
      packages:
        - name: ripgrep
          spec: ">=14.0.0|<15.0.0"
          install:
            linux: {backend: cargo}
      """

  Scenario: --plan on an absent package plans an install and exits 2
    When I run "--format json install . --plan"
    Then the exit code is 2
    And stdout contains "install"
    And stdout contains "14.1.1"

  Scenario: apply records the installed version in state
    When I run "--format json install . --yes"
    Then the exit code is 0
    And stdout contains "applied"
    When I run "--format json status ."
    Then stdout contains "14.1.1"

  Scenario: a cargo install needs no hold and warns about none
    When I run "--format json install . --plan"
    Then stdout does not contain "hold"
    And stdout does not contain "not pinned"

  Scenario: an installed-lower package is planned for upgrade
    Given the user state:
      """
      {"stateVersion":1,"packages":[{"name":"ripgrep","targets":{"linux/amd64":
        {"backend":"cargo","purl":"pkg:cargo/ripgrep","installed":"14.0.0","origin":"installed",
         "claimedBy":[{"manifest":"/t/bosun.yaml","spec":"vers:cargo/>=14.0.0|<15.0.0"}]}}}]}
      """
    When I run "--format json install . --plan"
    Then the exit code is 2
    And stdout contains "upgrade"

  Scenario: another project's tighter claim is respected, not upgraded past
    Given the user state:
      """
      {"stateVersion":1,"packages":[{"name":"ripgrep","targets":{"linux/amd64":
        {"backend":"cargo","purl":"pkg:cargo/ripgrep","installed":"14.0.0","origin":"installed",
         "claimedBy":[{"manifest":"/b/bosun.yaml","spec":"vers:cargo/>=14.0.0|<14.1.0"}]}}}]}
      """
    When I run "--format json install . --plan"
    Then the exit code is 0
    And stdout does not contain "upgrade"

  Scenario: conflicting claims are refused rather than flip-flopping
    Given the user state:
      """
      {"stateVersion":1,"packages":[{"name":"ripgrep","targets":{"linux/amd64":
        {"backend":"cargo","purl":"pkg:cargo/ripgrep","installed":"14.0.0","origin":"installed",
         "claimedBy":[{"manifest":"/b/bosun.yaml","spec":"vers:cargo/<14.0.0"}]}}}]}
      """
    When I run "--format json install . --yes"
    Then the exit code is 1
    And stdout contains "jointly satisfied"

  Scenario: --frozen applies the locked version
    When I run "lock ."
    Then a lockfile is written
    When I run "--format json install . --yes --frozen"
    Then the exit code is 0
    And stdout contains "applied"

  Scenario: no terminal and no --yes refuses to apply
    When I run "--format json install ."
    Then the exit code is 1
    And stdout contains "bosun.cli.environment"
