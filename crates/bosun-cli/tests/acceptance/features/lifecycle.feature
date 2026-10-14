Feature: uninstall, prune and crash-safety

  Scenario: uninstall removes a package bosun installed on its last claim
    Given a Linux amd64 machine, not elevated
    And cargo 1.76.0 is available
    And cargo publishes ripgrep 14.1.1
    And the manifest:
      """
      version: 1
      targets: [linux/amd64]
      packages:
        - name: ripgrep
          spec: ">=14.0.0|<15.0.0"
          install:
            linux: {backend: cargo}
      """
    When I run `bosun install . --yes`
    And I run `bosun --format json uninstall . --yes`
    Then it succeeds
    And it says "remove"
    And bosun has recorded nothing about ripgrep

  Scenario: prune removes an orphan whose manifest is gone
    Given a Linux amd64 machine, not elevated
    And cargo 1.76.0 is available
    And cargo publishes ripgrep 14.1.1
    And the manifest:
      """
      version: 1
      targets: [linux/amd64]
      packages:
        - name: ripgrep
          spec: ">=14.0.0|<15.0.0"
          install:
            linux: {backend: cargo}
      """
    When I run `bosun install . --yes`
    And the manifest is deleted
    And I run `bosun --format json prune --yes`
    Then it succeeds
    And it says "remove"

  Scenario: uninstall never removes an accepted package, only drops the claim
    Given a Debian trixie amd64 machine, not elevated
    And apt 2.6.1 is available
    And apt publishes jq 1.7.1-6+deb13u2
    And the manifest:
      """
      version: 1
      targets: [linux/amd64]
      packages:
        - name: jq
          spec: ">=1.6"
          install:
            linux:debian: {backend: apt}
      """
    And bosun accepted jq 1.7.1-6+deb13u1 for this project wanting ">=1.6"
    When I run `bosun --format json uninstall . --plan`
    Then it says "drop-claim"
    And it does not say "remove"

  Scenario: a failed step mid-apply keeps the earlier package
    Given a Linux amd64 machine, not elevated
    And cargo 1.76.0 is available
    And cargo publishes one 1.0.0
    And cargo publishes two 2.0.0
    And installing two fails
    And the manifest:
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
    When I run `bosun --format json install . --yes`
    Then it fails
    And it says "failed"
    And bosun has recorded one 1.0.0
    And bosun has recorded nothing about two
