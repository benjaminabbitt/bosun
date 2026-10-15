Feature: github-release finds releases whose tags aren't v<version>

  Some projects tag a release with more than its version (jq-1.8.2,
  bun-v1.4.3), and some repositories release several tools under one
  (kustomize/v5.8.3 beside kyaml/v0.21.3). The `tag` setting is the tag with
  `{{ version }}` where the version goes; only tags that fit it are versions.

  Background:
    Given a Linux amd64 machine, not elevated
    And bosun installs GitHub releases itself

  Scenario: jq tags its releases jq-<version>
    Given jqlang/jq jq-1.8.2 publishes jq-linux-amd64 holding just the binary
    And jqlang/jq jq-1.8.1 publishes jq-linux-amd64 holding just the binary
    And the manifest:
      """
      version: 1
      targets: [linux/amd64]
      packages:
        - name: jq
          spec: ">=1.8.0|<1.9.0"
          install:
            linux:
              backend: github-release
              package: jqlang/jq
              ext:
                github-release:
                  tag: "jq-{{ version }}"
                  asset: "jq-{{ os }}-{{ arch }}"
      """
    When I run `bosun install . --yes`
    Then it succeeds
    And bosun has recorded jq 1.8.2
    And jq is installed from jq-linux-amd64

  Scenario: kustomize's versions come only from kustomize/ tags
    Given kubernetes-sigs/kustomize kustomize/v5.8.3 publishes kustomize_v5.8.3_linux_amd64.tar.gz with kustomize at its root
    And kubernetes-sigs/kustomize kyaml/v9.9.9 publishes kyaml.tar.gz with kyaml at its root
    And the manifest:
      """
      version: 1
      targets: [linux/amd64]
      packages:
        - name: kustomize
          spec: ">=5.0.0"
          install:
            linux:
              backend: github-release
              package: kubernetes-sigs/kustomize
              ext:
                github-release:
                  tag: "kustomize/v{{ version }}"
                  asset: "{{ binary }}_{{ tag | replace('kustomize/', '') }}_{{ os }}_{{ arch }}.tar.gz"
      """
    When I run `bosun lock .`
    Then it succeeds
    And the lock pins kustomize 5.8.3

  Scenario: a tag template without {{ version }} is an error
    Given jqlang/jq jq-1.8.2 publishes jq-linux-amd64 holding just the binary
    And the manifest:
      """
      version: 1
      targets: [linux/amd64]
      packages:
        - name: jq
          spec: ">=1.8.0"
          install:
            linux:
              backend: github-release
              package: jqlang/jq
              ext:
                github-release: {tag: "jq-latest"}
      """
    When I run `bosun lock .`
    Then it fails
    And it reports "invalid tag template"
    And no lock is written
