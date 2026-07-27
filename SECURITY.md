# Security Policy

## Reporting a vulnerability

Please do not open a public issue.

Report privately through
[GitHub Security Advisories](https://github.com/teralabx/runlog/security/advisories/new),
or by email to **security@runlog.sh**.

Include what you were running, what you observed, and a reproduction if you have one. You
can expect an acknowledgement within 72 hours and an assessment within a week. We will keep
you updated through to a fix and credit you in the advisory unless you prefer otherwise.

## What counts as a vulnerability here

runlog captures and stores development output on disk, so the interesting failures are
about data escaping where it should not:

- **Redaction bypass** — a credential shape that reaches disk unmasked. Redaction happens
  on write, before the entry text or the fingerprint template is computed, so anything that
  gets past it is stored permanently.
- **Environment variable values being recorded.** Names only, never values, no exception.
  A leak here is a vulnerability, not a bug.
- **Sandbox escape from the agent SQL endpoint.** Agent queries run behind a read-only
  connection with `PRAGMA query_only`, an authorizer allowlist that denies `ATTACH`,
  `PRAGMA` and extension loading, an interrupt timer, row and byte caps, and one project
  database per connection. Reading anything outside the project database, writing, or
  pinning the daemon are all in scope.
- **Capture of a denylisted interactive program.** `psql`, `ssh`, `vim` and friends are
  sessions rather than runs and must run untouched.
- **Privilege issues in the installer** — it writes to shell profiles, launchd and agent
  config files. Anything that lets those writes be redirected or injected is in scope.

## Out of scope

- Output you deliberately captured being readable by your own user account. runlog stores
  it under `RUNLOG_HOME` (default `~/.runlog`) with your user's permissions by design.
- Anything requiring an attacker who already has code execution as your user.
- There is no network code in this repository, so there is no remote attack surface to
  report against the daemon.

## Supported versions

runlog is pre-1.0. Fixes land on the latest release only.
