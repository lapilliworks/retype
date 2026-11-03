# Changelog

All notable changes to retype are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project uses
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

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
