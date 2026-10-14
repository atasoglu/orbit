# Changelog

All notable changes to Orbit are documented here.
The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the project uses [Semantic Versioning](https://semver.org/).

Release notes on GitHub are generated from the section of the released version.

## [Unreleased]

## [0.2.0] - 2026-10-14

### Added
- Disks page: every mounted disk with used, free and total size and a usage bar (green, orange above 75%, red above 90%).
- `orbit-cli disks` prints the same table in the terminal.

## [0.1.0] - 2026-10-14

First release.

### Added
- Parallel disk scanner: allocated sizes, hard links counted once, disk-backed mounts only, live progress.
- Interactive sunburst chart rendered on the CPU: hover details, click to focus, double-click to zoom, small items stay selectable.
- Cleanup page with risk levels for caches, project build artifacts (`node_modules`, Rust `target`, Python venvs), old snaps and kernels; Trash or permanent delete; admin tasks through one `pkexec` prompt.
- Large files page with "Show in Files" and "Move to Trash".
- Light, Dark and Auto theme (follows the GNOME color scheme live).
- Read-only admin scan helper for root-only folders (`.deb` installs only).
- `orbit-cli` for text summaries and cleanup listing.
- `.deb` and tarball packaging, polkit policy, desktop entry and icon set.

[Unreleased]: https://github.com/atasoglu/orbit/compare/v0.2.0...HEAD
[0.2.0]: https://github.com/atasoglu/orbit/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/atasoglu/orbit/releases/tag/v0.1.0
