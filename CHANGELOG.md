# Changelog

All notable changes to retype are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project uses
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Changed

- Linux support targets current Debian stable (Debian 13/trixie, glibc 2.41). Linux CI uses Debian packages and a separately pinned older Typst for Debian's older Pandoc; the Debian 11/glibc 2.31 build floor and Fedora-only verification job are retired. macOS and Windows remain separate platform targets.
- `watch` starts or reaches the daemon and follows readiness and activity in the foreground. Scripts that only register directories must use `watch --register`. Interrupting the follower leaves the daemon and registrations active.
- CLI submissions acknowledge durable jobs before tool probes and input hashing. Preparation runs in the worker pool with cancellable snapshots and bounded, cancellable probes. Negotiated client-chosen ids keep submissions traceable if an acknowledgement is lost.
- Undo storage defaults to a five-gigabyte soft goal with `admission = 'stop-after'`; `stop-before` is available. Durable reservations prevent competing jobs from independently crossing the goal. Later jobs wait visibly without losing minimum retention. Explicitly disabled undo and legacy `on_overflow = 'skip'` retain their no-backup behavior.
- Daemon status includes undo usage, reservations, the storage goal and overage. Unreadable undo state reports an error without closing the daemon connection.
- Native verification earns the selected PNG-to-JPEG and PNG-to-WebP fidelity claims, and release archives require the resulting platform manifest. A fidelity run fails when any selected pair cannot be checked.

- Undo, settled/queued cancellation cleanup and startup reconciliation run on a separate bounded recovery lane. Recovery requests/results have durable handles, can be inspected or safely stopped, and remain visible after an undo record is removed.
- `retype recovery` lists protected reservations and operation results; `--cleanup` releases only recovery proven redundant against the unchanged original and publication ledger.
- Restart budgets are persisted per daemon boot and cause, shared across followers, and respect explicit stop intent. Verified login queries gate automatic watching and queued work; unavailable session information is reported as inactive.
- Debian x86-64 and ARM64 qualification run independently, build their own release executable/evidence and validate each exact archive through the real installer before upload.

### Fixed

- Connection loss during follower state refresh now reconnects through the persisted budget; event-ring overflow refreshes authoritative state without requiring unavailable history. Terminal EOF and interrupts do not confirm pending work; stale prompts do not apply a second decision.
- Cancellation during storage admission now settles as cancelled instead of leaving a waiting job. Parked-state publication is coordinated with cancellation, and resolution waits for the previous worker to release ownership.
- An expired undo record leased by another operation no longer rejects an unrelated backup. Expiry rechecks retention after copying so a concurrent resume cannot lose its renewed recovery.

Windows destructive cleanup and the signed installed tier remain unavailable. Native release qualification is still required.

## [0.1.0-alpha] - 2026-11-03

The first public release. Alpha: some documented behaviour is still being built,
conversions depend on external tools, and the release artifacts are not
code-signed.

### Added

- Rename-triggered conversion: a watched rename such as `photo.heic` to
  `photo.jpg` runs the conversion, and a rename to `photo.webp+q80` carries the
  quality setting in the name.
- The intent grammar, shared by file names, the command line and the extended
  attribute: formats, quality, size and geometry dials, presets and derived
  output names.
- The `retype` command line: `convert`, `explain`, `why`, `watch`, `unwatch`,
  `list`, `jobs`, `undo`, `daemon`, `doctor`, `bench`, `fidelity`, `init`,
  `version` and `completions`, with JSON output and stable exit codes.
- A per-user daemon over a Unix domain socket or a Windows named pipe, with a
  priority queue, cancellation, restart recovery, job logs and a journal.
- Undo and recovery built on durable snapshots and staged output publication:
  an interrupted or failed job cannot remove a source or leave a partial output.
- Watch registrations that survive restarts, a per-directory policy file,
  deferral of slow conversions, and a parked size threshold for oversized
  sources. Dropped events trigger a rescan, and file identity survives the
  watcher's own restore operations.
- Conversion verticals for images, audio, video, documents and archives, driven
  by ImageMagick, FFmpeg, pandoc and 7-Zip; `zip` and `tar` are native. Archive
  contents are verified before publication.
- Fidelity tiers and per-pair claims, earned by fixtures rather than declared.
- Windows integration: file identity from a handle, named-pipe impersonation,
  console-less tool launches and path separators.
- The external tools are resolved from the machine rather than managed by the
  CLI. On Unix, user configuration lives under `~/.config`.
- Release archives for Linux, macOS and Windows on x86_64 and aarch64, with
  SHA-256 checksums and GitHub build attestations.
- The `install.sh` and `install.ps1` installers, which verify the archive
  checksum and update an existing installation in place.
- The product site and the user manual, built from `docs/`.
