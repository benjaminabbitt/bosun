Feature: lock, lock --check, and resolution errors

  Scenario: lock writes the platform's versions and --check then passes
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
    When I run "lock ."
    Then a lockfile is written
    When I run "--format json lock . --check"
    Then the exit code is 0
    And stdout contains "fresh"

  Scenario: lock --check reports a missing target before the lock exists
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
    When I run "--format json lock . --check"
    Then the exit code is 2
    And stdout contains "missing"

  Scenario: an invalid constraint version is a parse error, not a silent miss
    Given a fixture:
      """
      {"os":"linux","arch":"amd64","privilege":"unelevated","user":"t",
       "backends":[{"id":"github-release","version":"1.0.0","purl_type":"github","vers_type":"semver",
         "platform_independent":false,"scope":"user","available":{"ctxloom/ctxloom":["0.6.4"]}}]}
      """
    And a manifest:
      """
      version: 1
      targets: [linux/amd64]
      packages:
        - name: ctxloom
          spec: ">=0.6"
          install:
            linux: {backend: github-release, package: ctxloom/ctxloom}
      """
    When I run "--format json lock ."
    Then the exit code is 1
    And stdout contains "SemVer"
    And no lockfile is written

  Scenario: an unsatisfiable entry makes lock write nothing and exit 1
    Given a fixture:
      """
      {"os":"linux","arch":"amd64","privilege":"unelevated","user":"t",
       "backends":[{"id":"github-release","version":"1.0.0","purl_type":"github","vers_type":"semver",
         "platform_independent":false,"scope":"user","available":{"ctxloom/ctxloom":["0.6.4"]}}]}
      """
    And a manifest:
      """
      version: 1
      targets: [linux/amd64]
      packages:
        - name: ctxloom
          spec: ">=99.0.0"
          install:
            linux: {backend: github-release, package: ctxloom/ctxloom}
      """
    When I run "--format json lock ."
    Then the exit code is 1
    And no lockfile is written
