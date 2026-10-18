Feature: bosun add and bosun remove edit the manifest, then lock

  `add` and `remove` change one entry of bosun.yaml and leave every other line,
  comments included, as written. Then they run `lock`, so bosun.lock follows
  the manifest. `add` never installs; `remove` uninstalls only what bosun
  installed and no other project still wants.

  Background:
    Given a Linux amd64 machine, not elevated
    And cargo 1.76.0 is available
    And cargo publishes ripgrep 13.0.0, 14.1.1

  Scenario: add without a spec writes a range from the newest version and locks it
    Given the manifest:
      """
      # my tools
      version: 2
      packages: []
      """
    When I run `bosun --format text add ripgrep --backend cargo`
    Then it succeeds
    And it says "Added ripgrep >=14.1.1|<15.0.0"
    And the manifest reads:
      """
      # my tools
      version: 2
      packages:
        - name: ripgrep
          spec: ">=14.1.1|<15.0.0"
          install: {backend: cargo}
      """
    And the lock pins ripgrep 14.1.1
    And ripgrep is not installed

  Scenario: add keeps the spec it is given and the comments around it
    Given the manifest:
      """
      version: 2
      packages:
        # pinned for the CI image
        - name: just
          spec: "1.40.0"
          install:
            linux: {backend: cargo}
      """
    And cargo publishes just 1.40.0
    When I run `bosun add ripgrep --backend cargo --spec ">=13.0.0|<14.0.0"`
    Then it succeeds
    And the manifest reads:
      """
      version: 2
      packages:
        # pinned for the CI image
        - name: just
          spec: "1.40.0"
          install:
            linux: {backend: cargo}
        - name: ripgrep
          spec: ">=13.0.0|<14.0.0"
          install: {backend: cargo}
      """
    And the lock pins ripgrep 13.0.0
    And the lock pins just 1.40.0

  Scenario: add a github release with an asset template, then install it
    Given bosun installs GitHub releases itself
    And muesli/duf v0.8.1 publishes duf_0.8.1_linux_x86_64.tar.gz with duf at its root
    And the manifest:
      """
      version: 2
      """
    When I run `bosun add duf --backend github-release --package muesli/duf --opt "asset={{ binary }}_{{ version }}_{{ os }}_{{ arch_goreleaser }}.tar.gz"`
    Then it succeeds
    And the manifest reads:
      """
      version: 2
      packages:
        - name: duf
          spec: ">=0.8.1|<0.9.0"
          install:
            linux:
              backends:
                github-release: {package: muesli/duf, asset: "{{ binary }}_{{ version }}_{{ os }}_{{ arch_goreleaser }}.tar.gz"}
      """
    And the lock pins duf 0.8.1
    When I run `bosun install . --yes`
    Then it succeeds
    And duf is installed from duf_0.8.1_linux_x86_64.tar.gz

  Scenario: each backend in a fallback list can name its own package
    Given cargo publishes fd-find 10.2.0
    And the manifest:
      """
      version: 2
      """
    When I run `bosun add fd --backend github-release,cargo --package sharkdp/fd --opt cargo.package=fd-find`
    Then it succeeds
    And the manifest reads:
      """
      version: 2
      packages:
        - name: fd
          spec: ">=10.2.0|<11.0.0"
          install:
            linux:
              backends:
                cargo: {package: fd-find}
                github-release: {package: sharkdp/fd}
      """
    And the lock pins fd 10.2.0

  Scenario: add a name the manifest already has changes nothing
    Given the manifest:
      """
      version: 2
      packages:
        - name: ripgrep
          spec: "13.0.0"
          install:
            linux: {backend: cargo}
      """
    When I run `bosun add ripgrep --backend cargo`
    Then it fails
    And it reports "already has a package named \"ripgrep\""
    And the manifest is unchanged
    And no lock is written

  Scenario: add with a spec nothing satisfies changes nothing
    Given the manifest:
      """
      version: 2
      packages: []
      """
    When I run `bosun add ripgrep --backend cargo --spec ">=99.0.0"`
    Then it fails
    And it reports "unsatisfiable"
    And the manifest is unchanged
    And no lock is written

  Scenario: add with an option the backend doesn't read changes nothing
    Given the manifest:
      """
      version: 2
      packages: []
      """
    When I run `bosun add ripgrep --backend cargo --opt features=x`
    Then it fails
    And it reports "unknown key \"features\" in backends.cargo (allowed: package)"
    And the manifest is unchanged

  Scenario: remove takes the entry out, drops it from the lock and uninstalls it
    Given the manifest:
      """
      version: 2
      packages:
        # search
        - name: ripgrep
          spec: ">=14.0.0|<15.0.0"
          install:
            linux: {backend: cargo}
        - name: just
          spec: "1.40.0"
          install:
            linux: {backend: cargo}
      """
    And cargo publishes just 1.40.0
    When I run `bosun install . --yes`
    And I run `bosun --format text remove ripgrep --yes`
    Then it succeeds
    And it says "Removed ripgrep"
    And the manifest reads:
      """
      version: 2
      packages:
        - name: just
          spec: "1.40.0"
          install:
            linux: {backend: cargo}
      """
    And the lock does not mention ripgrep
    And the lock pins just 1.40.0
    And bosun has recorded nothing about ripgrep
    And bosun has recorded just 1.40.0

  Scenario: remove keeps a package another project still wants
    Given the manifest:
      """
      version: 2
      packages:
        - name: ripgrep
          spec: ">=14.0.0|<15.0.0"
          install:
            linux: {backend: cargo}
      """
    And bosun installed ripgrep 14.1.1 for this project wanting ">=14.0.0|<15.0.0"
    And bosun installed ripgrep 14.1.1 for another project wanting ">=14.0.0"
    When I run `bosun remove ripgrep --yes`
    Then it succeeds
    And bosun has recorded ripgrep 14.1.1
    And the manifest reads:
      """
      version: 2
      packages: []
      """

  Scenario: remove --plan shows what it would do and changes nothing
    Given the manifest:
      """
      version: 2
      packages:
        - name: ripgrep
          spec: ">=14.0.0|<15.0.0"
          install:
            linux: {backend: cargo}
      """
    And bosun installed ripgrep 14.1.1 for this project wanting ">=14.0.0|<15.0.0"
    When I run `bosun remove ripgrep --plan`
    Then it succeeds
    And it says "remove"
    And it says "ripgrep"
    And the manifest is unchanged
    And bosun has recorded ripgrep 14.1.1

  # Removing an entry cleans up every scope it touches: a scope left behind
  # would keep a claim no manifest entry stands for any more.
  Scenario: remove refuses --scope before changing anything
    Given the manifest:
      """
      version: 2
      packages:
        - name: ripgrep
          spec: ">=14.0.0|<15.0.0"
          install:
            linux: {backend: cargo}
      """
    And bosun installed ripgrep 14.1.1 for this project wanting ">=14.0.0|<15.0.0"
    When I run `bosun --format text remove ripgrep --scope user --yes`
    Then it fails
    And it reports "remove takes no --scope: removing an entry cleans up every install scope it touches"
    And the manifest is unchanged
    And bosun has recorded ripgrep 14.1.1
    And no lock is written

  Scenario: remove's help says why it takes no --scope, and uninstall's offers it
    When I run `bosun remove --help`
    Then it succeeds
    And it says "every install scope the entry touches, so it takes no --scope"
    And it does not say "--scope <"
    When I run `bosun uninstall --help`
    Then it succeeds
    And it says "--scope <SCOPE>"

  Scenario: remove a name the manifest doesn't have fails
    Given the manifest:
      """
      version: 2
      packages: []
      """
    When I run `bosun remove ripgrep --yes`
    Then it fails
    And it reports "has no package named \"ripgrep\""
    And the manifest is unchanged

  Scenario: add writes the entry but fails when another entry can't be locked
    Given the manifest:
      """
      version: 2
      packages:
        - name: just
          spec: ">=99.0.0"
          install:
            linux: {backend: cargo}
      """
    And cargo publishes just 1.40.0
    When I run `bosun add ripgrep --backend cargo --spec "14.1.1"`
    Then it fails
    And it reports "just: unsatisfiable"
    And no lock is written

  Scenario: remove edits the manifest but fails when what is left can't be locked
    Given the manifest:
      """
      version: 2
      packages:
        - name: just
          spec: ">=99.0.0"
          install:
            linux: {backend: cargo}
        - name: ripgrep
          spec: "14.1.1"
          install:
            linux: {backend: cargo}
      """
    And cargo publishes just 1.40.0
    When I run `bosun remove ripgrep --yes`
    Then it fails
    And it reports "just: unsatisfiable"
    And no lock is written
