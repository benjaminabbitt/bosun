Feature: claims, frozen installs and the plan's exit codes

  Background:
    Given a fixture:
      """
      {"os":"linux","arch":"amd64","privilege":"unelevated","user":"t",
       "backends":[{"id":"cargo","version":"1.76.0","purl_type":"cargo","vers_type":"cargo",
         "platform_independent":true,"scope":"user","available":{"ripgrep":["14.1.1"]}}]}
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

  Scenario: --plan with nothing to do exits 0
    When I run "install . --yes"
    And I run "--format json install . --plan"
    Then the exit code is 0

  Scenario: --plan with changes to make exits 2 and prints a JSON plan
    When I run "--format json install . --plan"
    Then the exit code is 2
    And stdout is a JSON document

  Scenario: a claim records the manifest by its absolute path, even from a relative one
    When I run "install ./bosun.yaml --yes"
    Then the exit code is 0
    And the recorded claim names this project's manifest by its absolute path
    And the recorded state does not contain "./bosun.yaml"

  Scenario: --frozen refuses to change a package bosun does not own at another version
    Given a fixture:
      """
      {"os":"linux","arch":"amd64","privilege":"unelevated","user":"t",
       "backends":[{"id":"cargo","version":"1.76.0","purl_type":"cargo","vers_type":"cargo",
         "platform_independent":true,"scope":"user","available":{"ripgrep":["14.1.1"]},
         "installed":{"ripgrep":"14.0.0"}}]}
      """
    When I run "lock ."
    And I run "--format text install . --yes --frozen"
    Then the exit code is 1
    And stdout contains "ripgrep: installed 14.0.0, lock pins 14.1.1"
    And the recorded state does not contain "ripgrep"
