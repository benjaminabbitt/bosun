Feature: bosun backends lists the backends available on this host

  Background:
    Given a fixture:
      """
      {"os":"linux","arch":"amd64","privilege":"unelevated","user":"t",
       "backends":[{"id":"cargo","version":"1.76.0","purl_type":"cargo","vers_type":"cargo",
         "platform_independent":true,"scope":"user"},
        {"id":"github-release","version":"1.0.0","purl_type":"github","vers_type":"semver",
         "platform_independent":false,"scope":"user"}]}
      """

  Scenario: each registered backend is listed with its version
    When I run "--format text backends"
    Then the exit code is 0
    And stdout contains "cargo"
    And stdout contains "1.76.0"
    And stdout contains "github-release"

  Scenario: --format json reports the registered backends as a JSON document
    When I run "--format json backends"
    Then the exit code is 0
    And stdout is a JSON document
    And stdout contains "registered"
    And stdout contains "skipped"
    And stdout contains "1.76.0"
