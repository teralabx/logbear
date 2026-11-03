# Changelog

All notable changes to this project are documented here.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this
project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- The sidebar collapses to a narrow strip (⌘\), remembered between launches, with
  tooltips on every item while collapsed.
- Squashing: a finished run's stored output can be shrunk to the first occurrence of each
  distinct entry, dropping repeats that differ only in timestamps, ids or numbers. The
  run's counts and signatures still describe it as captured. Retention squashes the oldest
  runs before evicting any; `logbear squash`, a "squash run" button and a per-project
  "squash finished runs" button in Settings do it on demand.
- `FingerprintHistories` and `RunErrorCounts` protocol requests, and `fingerprint_id`,
  `unobserved` and `tail_lines` fields on existing ones.
- Homebrew formula for the CLI, daemon, MCP server and hook, distributed through the
  `teralabx/tap` tap.
- crates.io publishing metadata, so `cargo install logbear logbeard logbear-mcp logbear-hook`
  works.

### Changed

- Reads run on pooled read-only SQLite connections instead of the capture writer's, so a
  large UI read no longer stalls ingest.
- A page of entries reads each stored chunk once rather than once per entry.
- The desktop app keeps a small pool of daemon connections, so one slow command no longer
  blocks every other one, the health poll included.
- Terminal replay streams the last 20,000 lines as raw bytes instead of building the whole
  run's output into one string.
- Run, Live, Runs and Errors screens page their data instead of loading up to 8,000
  entries on open; error instances load when a signature is expanded, and sparklines come
  from one request instead of one per signature.
- Off-screen entry rows skip layout and paint.
- Licensed under MIT, with the licence text in `LICENSE.md` and the SPDX identifier in
  every manifest.
- Sandboxed queries signal their cancellation timer rather than polling it, removing an
  ~87ms fixed cost from every query the UI issues.
- Tauri commands that talk to the daemon run off the main thread, so a slow reply no longer
  stops the window painting.
- `logbeard` is scheduled as `Adaptive` rather than `Background`, which kept it on efficiency
  cores with throttled I/O on Apple Silicon.
- The UI says "uncommitted" rather than "dirty".

### Fixed

- Project databases never returned freed space to disk: `auto_vacuum` was set after WAL
  mode, when it can no longer take effect, so files only grew and retention, reading them
  as permanently over the cap, evicted run after run. Existing databases are converted
  once on open (a 1 GB file of mostly free pages shrinks in well under a second), and
  `incremental_vacuum` is now run to completion rather than stepped once.
- Viewing errors in the desktop app no longer marks them as seen for agent sessions, which
  could stop the hook from surfacing them.
- The Live screen opens on a run's newest entries rather than its oldest.
- A command that cannot start is no longer recorded as a run.
- `LOGBEAR_DAEMON` is authoritative even when it names a path that does not exist, so tests
  can assert the no-daemon path.
- The entries-only view on the live screen no longer collapses to zero height.
- A derived-schema bump rebuilds the index.
- Runs abandoned by a dead daemon are closed rather than shown as running.

## [0.1.0] — unreleased

Initial release.

[Unreleased]: https://github.com/teralabx/logbear/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/teralabx/logbear/releases/tag/v0.1.0
