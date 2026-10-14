Feature: uninstall, prune and crash-safety

  Background:
    Given a fixture:
      """
      {"os":"linux","arch":"amd64","privilege":"unelevated","user":"t",
       "backends":[{"id":"cargo","version":"1.76.0","purl_type":"cargo","vers_type":"cargo",
         "platform_independent":true,"scope":"user","supports_hold":false,
         "available":{"ripgrep":["14.1.1"]}}]}
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

  Scenario: uninstall removes a package bosun installed on its last claim
    When I run "install . --yes"
    And I run "--format json uninstall . --yes"
    Then the exit code is 0
    And stdout contains "remove"
    And the recorded state does not contain "ripgrep"

  Scenario: prune removes an orphan whose manifest is gone
    When I run "install . --yes"
    And the manifest is deleted
    And I run "--format json prune --yes"
    Then the exit code is 0
    And stdout contains "remove"

  Scenario: uninstall never removes an accepted package, only drops the claim
    Given a fixture:
      """
      {"os":"linux","arch":"amd64","distro":{"id":"debian","codename":"trixie"},
       "privilege":"unelevated","user":"ben",
       "backends":[{"id":"apt","version":"2.6.1","purl_type":"deb","vers_type":"deb",
         "platform_independent":false,"scope":"system","supports_hold":true,
         "available":{"jq":["1.7.1-6+deb13u2"]}}]}
      """
    And a manifest:
      """
      version: 1
      targets: [linux/amd64]
      packages:
        - name: jq
          spec: ">=1.6"
          install:
            linux:debian: {backend: apt}
      """
    And the system state:
      """
      {"stateVersion":1,"packages":[{"name":"jq","targets":{"linux/amd64":
        {"backend":"apt","purl":"pkg:deb/debian/jq?arch=amd64&distro=trixie","installed":"1.7.1-6+deb13u1",
         "origin":"accepted","claimedBy":[{"manifest":"{manifest}","spec":"vers:deb/>=1.6"}]}}}]}
      """
    When I run "--format json uninstall . --plan"
    Then stdout contains "drop-claim"
    And stdout does not contain "remove"

  Scenario: a failed step mid-apply keeps the earlier package
    Given a fixture:
      """
      {"os":"linux","arch":"amd64","privilege":"unelevated","user":"t","fail_step":"two",
       "backends":[{"id":"cargo","version":"1.76.0","purl_type":"cargo","vers_type":"cargo",
         "platform_independent":true,"scope":"user","supports_hold":false,
         "available":{"one":["1.0.0"],"two":["2.0.0"]}}]}
      """
    And a manifest:
      """
      version: 1
      targets: [linux/amd64]
      packages:
        - name: one
          spec: ">=1.0.0|<2.0.0"
          install:
            linux: {backend: cargo}
        - name: two
          spec: ">=2.0.0|<3.0.0"
          install:
            linux: {backend: cargo}
      """
    When I run "--format json install . --yes"
    Then the exit code is 1
    And stdout contains "failed"
    And the recorded state contains "one"
    And the recorded state does not contain "two"
