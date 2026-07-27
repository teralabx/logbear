# Changelog

All notable changes to this project are documented here.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this
project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- Homebrew formula for the CLI, daemon, MCP server and hook, distributed through the
  `teralabx/tap` tap.
- crates.io publishing metadata, so `cargo install runlog runlogd runlog-mcp runlog-hook`
  works.

### Changed

- Licensed under FSL-1.1-MIT. Previous manifests declared MIT with no licence file present.
- Sandboxed queries signal their cancellation timer rather than polling it, removing an
  ~87ms fixed cost from every query the UI issues.
- Tauri commands that talk to the daemon run off the main thread, so a slow reply no longer
  stops the window painting.
- `runlogd` is scheduled as `Adaptive` rather than `Background`, which kept it on efficiency
  cores with throttled I/O on Apple Silicon.
- The UI says "uncommitted" rather than "dirty".

### Fixed

- A command that cannot start is no longer recorded as a run.
- `RUNLOG_DAEMON` is authoritative even when it names a path that does not exist, so tests
  can assert the no-daemon path.
- The entries-only view on the live screen no longer collapses to zero height.
- A derived-schema bump rebuilds the index.
- Runs abandoned by a dead daemon are closed rather than shown as running.

## [0.1.0] — unreleased

Initial release.

[Unreleased]: https://github.com/teralabx/runlog/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/teralabx/runlog/releases/tag/v0.1.0
