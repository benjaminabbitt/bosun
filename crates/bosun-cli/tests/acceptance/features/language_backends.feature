Feature: npm and uv install language tools at user scope

  npm installs Node tools from the npm registry (`npm install -g`) and uv
  installs Python tools from PyPI (`uv tool install`). Both are
  platform-independent, so their entries resolve once, under `any`. npm
  versions compare as SemVer and PyPI versions as PEP 440, where alpha, beta,
  release-candidate and dev releases are pre-releases and post-releases are
  not. Neither keeps a durable hold: bosun asserts the exact version each run.

  Background:
    Given a Linux amd64 machine, not elevated
    And npm 10.9.9 is available
    And uv 0.13.1 is available

  Scenario: add writes a platform-independent npm entry, identified as an npm package
    Given npm publishes cowsay 1.5.0 and 1.6.0
    And the manifest:
      """
      version: 2
      packages: []
      """
    When I run `bosun --format text add cowsay --backend npm`
    Then it succeeds
    And the manifest reads:
      """
      version: 2
      packages:
        - name: cowsay
          spec: ">=1.6.0|<2.0.0"
          install: {backend: npm}
      """
    And the lock pins cowsay 1.6.0
    And the lock identifies cowsay as pkg:npm/cowsay

  Scenario: add writes a uv entry with the lower bound uv add writes
    Given uv publishes ruff 0.6.8, 0.6.9 and 0.7.0rc1
    And the manifest:
      """
      version: 2
      packages: []
      """
    When I run `bosun --format text add ruff --backend uv`
    Then it succeeds
    And the manifest reads:
      """
      version: 2
      packages:
        - name: ruff
          spec: ">=0.6.9"
          install: {backend: uv}
      """
    And the lock pins ruff 0.6.9
    And the lock identifies ruff as pkg:pypi/ruff

  Scenario: a PyPI name is identified, and asked for, as PyPI normalises it
    Given uv publishes django-upgrade 1.20.0
    And the manifest:
      """
      version: 2
      packages: []
      """
    When I run `bosun add Django_Upgrade --backend uv`
    Then it succeeds
    And the lock identifies Django_Upgrade as pkg:pypi/django-upgrade

  Scenario: PEP 440 orders the releases, and pre-releases are skipped unless named
    Given uv publishes black 23.12.1, 24.1a1, 24.1b1.dev2 and 23.12.1.post1
    And the manifest:
      """
      version: 2
      packages:
        - name: black
          spec: ">=23.1"
          install: {backend: uv}
      """
    When I run `bosun lock .`
    Then it succeeds
    And the lock pins black 23.12.1.post1
    When the manifest is replaced by:
      """
      version: 2
      packages:
        - name: black
          spec: ">=24.1a1"
          install: {backend: uv}
      """
    And I run `bosun lock .`
    Then it succeeds
    And the lock pins black 24.1b1.dev2

  Scenario: install puts each tool in place at user scope, with no hold
    Given npm publishes cowsay 1.6.0
    And uv publishes httpie 3.2.3 and 3.2.4
    And the manifest:
      """
      version: 2
      packages:
        - name: cowsay
          spec: ">=1.6.0|<2.0.0"
          install: {backend: npm}
        - name: httpie
          spec: ">=3.2.0|<4.0.0"
          install: {backend: uv}
      """
    When I run `bosun --format json install . --plan`
    Then it reports changes pending
    And it does not say "hold"
    When I run `bosun --format json install . --yes`
    Then it succeeds
    And bosun has recorded cowsay 1.6.0
    And bosun has recorded httpie 3.2.4

  Scenario: export writes the tools npm and uv installed by hand
    Given npm publishes cowsay 1.6.0
    And uv publishes ruff 0.6.9
    And cowsay 1.6.0 is installed by hand
    And ruff 0.6.9 is installed by hand
    When I run `bosun --format text export --yes`
    Then it succeeds
    And it says "Added 2 packages"
    And the manifest reads:
      """
      version: 2
      packages:
        - name: cowsay
          spec: ">=1.6.0|<2.0.0"
          install: {backend: npm}
        - name: ruff
          spec: ">=0.6.9"
          install: {backend: uv}
      """
    And the lock pins cowsay 1.6.0
    And the lock pins ruff 0.6.9
