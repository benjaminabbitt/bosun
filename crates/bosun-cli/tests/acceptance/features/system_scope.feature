Feature: accept, hold and privilege at system scope (apt)

  Background:
    Given a manifest:
      """
      version: 1
      targets: [linux/amd64]
      packages:
        - name: jq
          spec: ">=1.6"
          install:
            linux:debian: {backend: apt}
      """

  Scenario: a package present outside bosun is accepted, never installed or held
    Given a fixture:
      """
      {"os":"linux","arch":"amd64","distro":{"id":"debian","codename":"trixie"},
       "privilege":"unelevated","user":"ben",
       "backends":[{"id":"apt","version":"2.6.1","purl_type":"deb","vers_type":"deb",
         "platform_independent":false,"scope":"system","supports_hold":true,
         "available":{"jq":["1.7.1-6+deb13u2"]},"installed":{"jq":"1.7.1-6+deb13u1"}}]}
      """
    When I run "--format json install . --plan"
    Then the exit code is 0
    And stdout contains "accept"
    And stdout does not contain "hold"

  Scenario: an absent apt package is planned to install and then hold
    Given a fixture:
      """
      {"os":"linux","arch":"amd64","distro":{"id":"debian","codename":"trixie"},
       "privilege":"unelevated","user":"ben",
       "backends":[{"id":"apt","version":"2.6.1","purl_type":"deb","vers_type":"deb",
         "platform_independent":false,"scope":"system","supports_hold":true,
         "available":{"jq":["1.7.1-6+deb13u2"]}}]}
      """
    When I run "--format json install . --plan"
    Then the exit code is 2
    And stdout contains "install"
    And stdout contains "hold"

  Scenario: a system change without elevation is refused with a remedy
    Given a fixture:
      """
      {"os":"linux","arch":"amd64","distro":{"id":"debian","codename":"trixie"},
       "privilege":"unelevated","user":"ben",
       "backends":[{"id":"apt","version":"2.6.1","purl_type":"deb","vers_type":"deb",
         "platform_independent":false,"scope":"system","supports_hold":true,
         "available":{"jq":["1.7.1-6+deb13u2"]}}]}
      """
    When I run "--format json install . --yes"
    Then the exit code is 1
    And stdout contains "elevation_required"
    And stdout contains "sudo bosun install --scope system"

  Scenario: elevated, the system install applies
    Given a fixture:
      """
      {"os":"linux","arch":"amd64","distro":{"id":"debian","codename":"trixie"},
       "privilege":"elevated","user":"ben",
       "backends":[{"id":"apt","version":"2.6.1","purl_type":"deb","vers_type":"deb",
         "platform_independent":false,"scope":"system","supports_hold":true,
         "available":{"jq":["1.7.1-6+deb13u2"]}}]}
      """
    When I run "--format json install . --yes"
    Then the exit code is 0
    And stdout contains "applied"
