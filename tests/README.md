# Native test harness

Use a native Linux, macOS or Windows checkout. WSL does not exercise Windows APIs. Rust unit and integration tests remain in their Cargo crates; this directory contains preparation, runners and standalone diagnostics. Production release assembly remains in `scripts/`.

## Prepare

Install a supported Rust toolchain with `rustfmt`, Clippy and native build tools. The strict runner also needs cargo-nextest:

```sh
cargo install cargo-nextest --locked --version 0.9.151
```

Building that nextest version from source needs Rust 1.91 or newer, independently of Retype's Rust 1.85 floor. On Debian stable with its packaged Rust 1.85.1, use a compatible prebuilt nextest binary or a newer build toolchain. A nextest executable built on unstable may require a newer glibc and is not automatically portable to stable.

Install ImageMagick 7.1+ with HEIC decoding, FFmpeg/FFprobe, Pandoc, a PDF engine and 7-Zip. These helpers install external tools using package managers already on the machine; they do not install Rust, bootstrap a package manager, enable repositories or configure a Retype service:

| Host | Preparation |
| --- | --- |
| Current Debian stable | `./tests/prepare.sh`: APT converters and fonts; install a compatible Typst separately. Uses sudo only for a non-root caller |
| macOS with Homebrew | `./tests/prepare.sh` |
| Windows PowerShell 7 with Chocolatey | `./tests/prepare.ps1`: Run in the same PowerShell session as testing |

Current Debian stable is the supported Linux baseline. Other distributions and Debian unstable can be used for development without implying support. Stable's Pandoc 3.1.11.1 does not work with Typst 0.15.1; use an older Typst and verify it with Pandoc. [Older Pandoc and Typst](../docs/advanced/converters.md#older-pandoc-and-typst) shows a side-by-side installation and path pin. CI selects Typst 0.11.1 for that older Pandoc; the default engine preference remains Typst first. Do not remove protected desktop packages or replace codecs just to make a report green. See `docs/dev/testing.md` for exact tool, codec and filesystem requirements.

## Check and run

From the repository root:

| Host | Check prerequisites only | Full strict validation |
| --- | --- | --- |
| Linux/macOS | `./tests/run.sh --check` | `./tests/run.sh` |
| Windows PowerShell 7 | `pwsh -NoProfile -File ./tests/run.ps1 -Check` | `pwsh -NoProfile -File ./tests/run.ps1` |

Check mode records bounded tool probes, validates the nextest version requirement and decodes `fixtures/native-watch/photo.heic` with ImageMagick. An advertised `heic` delegate is insufficient; the checked-in HEIC fixture must decode successfully. Check mode does not build tests or establish the full conversion or undo workflow.

Full validation checks formatting and Clippy, checks installer syntax, runs the entire host-selected workspace with nextest, and runs doctests separately. A separate debug build under `target/test-checkpoints/` runs all fourteen real-daemon interruption cases with the `test-checkpoints` feature. That feature grants no cleanup authority and refuses release builds. Unix also exercises the shell installer against a local fake release. The shared nextest profile starts the two strict HEIC workflows first; they run once, not again in a separate CLI phase. Four workers, zero retries, per-test deadlines and failed leak detection apply on all three platforms.

Missing tools, probe errors, empty format listings, failed HEIC decoding, failed or absent strict workflows, an incomplete daemon interruption matrix, skipped tests, timeouts and leaked test processes fail validation. Windows source disposal and destructive undo require qualified fixed local NTFS paths. The core unit suite and separate daemon matrix exercise publication, cleanup and undo, including interruption, replacement refusal and retained orphan claims; see [Windows durability qualification](../docs/dev/windows-durability.md). No gate is suppressed to obtain a green report.

For a contributor baseline without mandatory external tools, use `cargo test --workspace --locked --no-fail-fast`. Optional guards can return early and still count as Rust passes; that command is not release evidence.

## Reports and limits

Each invocation prints a unique report directory under `target/test-reports/`. The console shows phase status and log locations; full test output stays in files. `summary.txt` records probe/phase failures, limits, elapsed times and the final result. Metadata includes host/tool information, revision, working-tree status and the tracked diff. Untracked file contents are not copied. Windows keeps probe stdout/stderr under `metadata/` and separate phase stdout/stderr logs. Windows records phase starts and fatal runner errors in the summary; a run stopped before completion is failed, never passed.

Debug test runners use controlled login fixtures; they do not prove native login/logout behavior. Production watching requires a verified login and remains inactive if the session query is unavailable.

Private scratch data and isolated daemon state are retained for inspection. Cleanup stops only identifiable processes associated with that run and treats leaks or cleanup failures as errors. Inspect logs and diffs for sensitive paths or data before sharing. A successful runner is not published-artifact, physical power-loss, real disk-exhaustion, network-filesystem or every-architecture evidence.

| Variable | Meaning |
| --- | --- |
| `RETYPE_TEST_TIMEOUT_SECONDS` | 1 to 3600 seconds, default 300: Per-step budget and a separate shared workspace/doctest budget |
| `RETYPE_TEST_REPORT_DIR` | Absolute report parent; defaults to `target/test-reports` |
| `RETYPE_TEST_TEMP_DIR` | Absolute writable scratch parent; use a short local path on Unix |

Compilation has its own deadline before the shared execution budget starts. Each nextest test is terminated after 60 seconds; a phase not reached within the remaining suite budget is incomplete. Linux uses GNU `timeout`; macOS uses system Perl without `gtimeout`. Bounded commands have closed stdin; tests requiring a terminal create one themselves.

## Standalone checks and diagnostics

- `tests/install.sh`: Local Unix installer/update/rollback checks
- `tests/installed.py <archive> --report <dir>`: Exact Debian or Windows archive installation, executable architecture on Windows, conversions, fidelity discovery, undo, update and checksum refusal
- `tests/storage-faults.py --binary <path> --report <dir>`: Isolated Linux restoration ENOSPC, responsive control and safe exact-byte retry
- `tests/windows-storage-faults.ps1 -Worker <core-test-exe> -Report <new-dir>`: Actual disk exhaustion on a new 64 MiB virtual NTFS disk; requires an elevated orchestrator and active ordinary-user desktop, retains evidence and detaches the disk
- `tests/install.ps1 -Binary <path>`: Local Windows installer/update/rollback checks, normally invoked by Cargo
- `tests/release.sh`: Synthetic release-assembly and checksum failures, intended for Linux release CI
- `tests/hyperv-info.ps1`: Read-only Hyper-V host/guest collection; see `docs/dev/hyperv.md`
- `tests/hyperv-check.ps1`: Synthetic collector checks without a VM
- `tests/probe-supersede.sh`: Detailed Unix watcher/supersession investigation, outside the release gate
- `tests/support/run-bounded.pl`: Process-group supervision used by the macOS runner and its regression tests
- `tests/support/windows-user.ps1 -Architecture X64|Arm64 -Command <trusted-code>`: Bounded native qualification with a verified non-administrator token and retained identity/log reports
