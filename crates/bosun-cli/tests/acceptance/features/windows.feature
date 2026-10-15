Feature: github-release on Windows

  On Windows a release ships a .zip, or the .exe itself. bosun installs the
  binary as <name>.exe into %USERPROFILE%\.local\bin, the template's
  {{ binary }} staying the bare name the release uses. (This tier runs on any
  OS through the fixture; a real Windows run is separate.)

  Background:
    Given a Windows amd64 machine, not elevated
    And bosun installs GitHub releases itself

  Scenario: a zip with tool.exe in a wrapping directory
    Given o/tool v1.2.0 publishes tool_1.2.0_windows_amd64.zip with tool.exe in tool_1.2.0_windows_amd64/
    And the manifest:
      """
      version: 1
      targets: [windows/amd64]
      packages:
        - name: tool
          spec: ">=1.0.0|<2.0.0"
          install:
            windows:
              backend: github-release
              package: o/tool
              ext:
                github-release: {asset: "{{ binary }}_{{ version }}_{{ os }}_{{ arch }}.zip"}
      """
    When I run `bosun install . --yes`
    Then it succeeds
    And tool.exe is installed from tool_1.2.0_windows_amd64.zip
    And bosun has recorded tool 1.2.0
    When I run `bosun uninstall . --yes`
    Then it succeeds
    And tool.exe is not installed

  Scenario: jq ships jq-windows-amd64.exe
    Given jqlang/jq jq-1.8.2 publishes jq-windows-amd64.exe holding just the binary
    And the manifest:
      """
      version: 1
      targets: [windows/amd64]
      packages:
        - name: jq
          spec: ">=1.8.0|<1.9.0"
          install:
            windows:
              backend: github-release
              package: jqlang/jq
              ext:
                github-release:
                  tag: "jq-{{ version }}"
                  asset: "jq-{{ os }}-{{ arch }}.exe"
      """
    When I run `bosun install . --yes`
    Then it succeeds
    And jq.exe is installed from jq-windows-amd64.exe

  Scenario: a Linux-only entry does not apply on Windows
    Given the manifest:
      """
      version: 1
      targets: [windows/amd64]
      packages:
        - name: jq
          spec: ">=1.7"
          install:
            linux: {backend: apt}
      """
    When I run `bosun --format text install . --plan`
    Then it succeeds
    And it says "not applicable"
