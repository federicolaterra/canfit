# Contributing to canfit

Thank you for helping improve canfit. canfit is a young project, so open an issue before a large
change to agree on scope first — especially for anything that reads system data, estimates memory,
or closes processes.

## Good first issues

Issues labelled [`good first issue`](https://github.com/federicolaterra/canfit/labels/good%20first%20issue)
are small, self-contained and already agreed in scope; comment on one before you start so two
people don't work on the same thing. The easiest way in is teaching canfit about an app:

- **A catalog entry** for a common app canfit has never seen: measure it with the monitor open
  (`canfit history --apps`) or with `canfit watch -- <app>`, then change only
  [`catalog.toml`](../src/estimate/apps/catalog.toml). The format, how to measure and the review
  rules are in [Apps](../docs/open-source/apps.md#the-seed-catalog).
- **An app profile** for an app that declares its memory in a settings file (a VM manager, a
  container runtime, a JVM app): a block in [`profiles.toml`](../src/estimate/apps/profiles.toml),
  a fixture settings file under `tests/fixtures/apps/` and a test. See
  [Add a profile](../docs/open-source/apps.md#add-a-profile). An app whose settings need a new
  reader is worth discussing first.

Propose either one with the *New app profile or catalog entry* issue form. Other good first issues
are documentation fixes and clearer wording of an answer.

## Development workflow

1. Read the public [architecture](../docs/open-source/architecture.md) and
   [development](../docs/open-source/development.md) guides.
2. Create a focused branch and keep each change to one concern.
3. Preserve canfit's principles: answers before numbers, nothing closed without confirmation, every
   value labelled measured / read from config / estimated, a single binary with no runtime
   dependencies, and no telemetry.
4. Add tests for changed behaviour. Platform code must compile on macOS, Windows and Linux.
5. Run `make check` (format check, clippy with warnings denied, tests).
6. Update documentation and add user-visible changes under `Unreleased` in
   [`CHANGELOG.md`](../docs/open-source/CHANGELOG.md).
7. In the pull request, describe what you verified and on which OS, and list any check you could
   not run.

Readings of memory, pressure and processes must be verified on a real machine or VM. Containers
report the container, not the host, and are not valid evidence for system-reading changes.

Commit titles follow [Conventional Commits](https://www.conventionalcommits.org/) (`feat:`, `fix:`,
`docs:`, `refactor:`, `test:`, `build:`, `ci:`, `chore:`, `perf:`).

## License

canfit is licensed under the [MIT License](../LICENSE). By contributing, you agree that your
contributions are licensed under the same terms. Preserve third-party headers and attribution.
