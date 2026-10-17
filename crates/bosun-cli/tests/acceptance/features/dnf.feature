Feature: dnf installs and holds RPM packages on Fedora and the RHEL family

  Scenario: the lock names the package by its rpm identity on this release
    Given a Fedora 41 amd64 machine, not elevated
    And dnf 5.2.17.0 is available
    And dnf publishes jq 1.7.1-8.fc41
    And the manifest:
      """
      version: 2
      packages:
        - name: jq
          spec: ">=1.6"
          install:
            linux:fedora: {backend: dnf}
      """
    When I run `bosun lock .`
    Then it succeeds
    And the lock identifies jq as pkg:rpm/fedora/jq?arch=x86_64&distro=fedora-41

  Scenario: versions order as rpm orders them: epoch first, and a tilde build is chosen like any other
    Given a Fedora 41 amd64 machine, not elevated
    And dnf 5.2.17.0 is available
    And dnf publishes jq 2.0-1.fc41, 1:1.6-1.fc41 and 1:1.7~rc1-1.fc41
    And the manifest:
      """
      version: 2
      packages:
        - name: jq
          spec: ">=1.6"
          install:
            linux:fedora: {backend: dnf}
      """
    When I run `bosun lock .`
    Then it succeeds
    And the lock pins jq 1:1.7~rc1-1.fc41

  Scenario: bosun add writes a range over any release of the newest version
    Given a Fedora 41 amd64 machine, not elevated
    And dnf 5.2.17.0 is available
    And dnf publishes jq 1.7.1-8.fc41
    And the manifest:
      """
      version: 2
      packages: []
      """
    When I run `bosun --format text add jq --backend dnf`
    Then it succeeds
    And it says "Added jq >=1.7.1|<1.7.2~"
    And the lock pins jq 1.7.1-8.fc41

  Scenario: an absent package is planned to install and then hold
    Given a Fedora 41 amd64 machine, not elevated
    And dnf 5.2.17.0 is available
    And dnf publishes jq 1.7.1-8.fc41
    And the manifest:
      """
      version: 2
      packages:
        - name: jq
          spec: ">=1.6"
          install:
            linux:fedora: {backend: dnf}
      """
    When I run `bosun --format json install . --plan`
    Then it reports changes pending
    And it says "install"
    And it says "hold"

  Scenario: on Rocky a linux:rhel block applies; install, then prune
    Given a Rocky 9.8 amd64 machine, elevated
    And dnf 4.14.0 is available
    And dnf publishes jq 1.6-19.el9_8.2
    And the manifest:
      """
      version: 2
      packages:
        - name: jq
          spec: ">=1.6"
          install:
            linux:rhel: {backend: dnf}
      """
    When I run `bosun --format json install . --yes`
    Then it succeeds
    And it says "applied"
    And bosun has recorded jq 1.6-19.el9_8.2
    And the lock identifies jq as pkg:rpm/rocky/jq?arch=x86_64&distro=rocky-9.8
    When the manifest is deleted
    And I run `bosun --format json prune --yes --scope system`
    Then it succeeds
    And it says "remove"
    And bosun has recorded nothing about jq
