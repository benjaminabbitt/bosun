Feature: bosun status reports what is installed against what the manifest wants

  Background:
    Given a manifest:
      """
      version: 1
      targets: [linux/amd64]
      packages:
        - name: ripgrep
          spec: ">=14.0.0|<15.0.0"
          install:
            linux: {backend: cargo}
      """

  Scenario: a package bosun installed is reported up to date
    Given a fixture:
      """
      {"os":"linux","arch":"amd64","privilege":"unelevated","user":"t",
       "backends":[{"id":"cargo","version":"1.76.0","purl_type":"cargo","vers_type":"cargo",
         "platform_independent":true,"scope":"user","available":{"ripgrep":["14.1.1"]}}]}
      """
    When I run "install . --yes"
    And I run "--format text status ."
    Then the exit code is 0
    And stdout contains "14.1.1 (up to date)"

  Scenario: a package that is absent is reported not installed
    Given a fixture:
      """
      {"os":"linux","arch":"amd64","privilege":"unelevated","user":"t",
       "backends":[{"id":"cargo","version":"1.76.0","purl_type":"cargo","vers_type":"cargo",
         "platform_independent":true,"scope":"user","available":{"ripgrep":["14.1.1"]}}]}
      """
    When I run "--format text status ."
    Then the exit code is 0
    And stdout contains "not installed"

  Scenario: a package present outside bosun is shown as present, never "not installed"
    Given a fixture:
      """
      {"os":"linux","arch":"amd64","privilege":"unelevated","user":"t",
       "backends":[{"id":"cargo","version":"1.76.0","purl_type":"cargo","vers_type":"cargo",
         "platform_independent":true,"scope":"user","available":{"ripgrep":["14.1.1"]},
         "installed":{"ripgrep":"14.0.0"}}]}
      """
    When I run "--format text status ."
    Then the exit code is 0
    And stdout contains "14.0.0 present, not managed (would be accepted)"
    And stdout does not contain "not installed"

  Scenario: a package bosun accepted is reported as managed at its present version
    Given a fixture:
      """
      {"os":"linux","arch":"amd64","privilege":"unelevated","user":"t",
       "backends":[{"id":"cargo","version":"1.76.0","purl_type":"cargo","vers_type":"cargo",
         "platform_independent":true,"scope":"user","available":{"ripgrep":["14.1.1"]},
         "installed":{"ripgrep":"14.1.1"}}]}
      """
    And the user state:
      """
      {"stateVersion":1,"packages":[{"name":"ripgrep","targets":{"linux/amd64":
        {"backend":"cargo","purl":"pkg:cargo/ripgrep","installed":"14.1.1","origin":"accepted",
         "claimedBy":[{"manifest":"{manifest}","spec":"vers:cargo/>=14.0.0|<15.0.0"}]}}}]}
      """
    When I run "--format text status ."
    Then the exit code is 0
    And stdout contains "14.1.1 (up to date)"
    And stdout does not contain "not managed"

  Scenario: --format json reports each package's presence and ownership
    Given a fixture:
      """
      {"os":"linux","arch":"amd64","privilege":"unelevated","user":"t",
       "backends":[{"id":"cargo","version":"1.76.0","purl_type":"cargo","vers_type":"cargo",
         "platform_independent":true,"scope":"user","available":{"ripgrep":["14.1.1"]},
         "installed":{"ripgrep":"14.0.0"}}]}
      """
    When I run "--format json status ."
    Then the exit code is 0
    And stdout is a JSON document
    And stdout contains "managed"
    And stdout contains "present"
    And stdout contains "up_to_date"
