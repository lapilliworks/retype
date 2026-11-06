# Test preparation and platform validation

Run with a supported Rust toolchain and native build tools. Use the actual target OS: WSL and a Linux container do not exercise Windows APIs. Tests use synthetic files, private temporary state and test daemons; do not register real folders or install a background service for them.

## One-command platform runners

From the repository root, use the runner for the native operating system:

| Platform | Command |
|---|---|
| Linux | `./scripts/test-linux.sh` |
| macOS | `./scripts/test-macos.sh` |
| Windows PowerShell 7 | `pwsh -NoProfile -File .\scripts\test-windows.ps1` |

Each runner checks formatting and workspace Clippy, runs every workspace test selected for the host with real tools required, and enables the strict native HEIC rename and undo gate. The Unix runners also check and exercise the shell installer against a local fake release; the Windows runner parses the PowerShell installer. A missing prerequisite, skipped test, unrun ignored test, timeout, leaked test process or failed check makes the runner exit nonzero. Windows currently cannot complete destructive undo; the strict gate remains a release blocker there, not a test-runner error.

The macOS runner uses `/usr/bin/perl` and its standard modules for bounded process handling; it does not require `gtimeout` or Homebrew coreutils. The Linux runner requires GNU `timeout`, and its workspace tests also exercise the macOS helper using `/usr/bin/perl`. Windows requires PowerShell 7, CIM process-query support and cargo-nextest 0.9.151 or newer; install nextest with `cargo install cargo-nextest --locked`. Windows PowerShell 5.1 has a reported Git-revision-check failure that is not yet diagnosed. The codec, PDF-engine and Rust prerequisites are listed below. Native Windows and macOS results must be collected on those systems.

The runners print a report directory under `target/platform-test-reports/` with revision, host and tool versions, step logs and a summary. They record working-tree status but not the contents of local modifications; use a clean checkout or preserve and inspect the diffs separately when sharing evidence. Logs can contain local paths; inspect them before sharing. Test files and daemon state are isolated in a new scratch directory, which is retained and printed for inspection. The macOS runner also stops and reports subprocesses left in a completed command's process group. Both Unix runners attempt to stop leaked detached processes whose command lines identify that scratch directory; an unidentifiable detached helper may need manual cleanup. They do not test published release artifacts, power loss, real disk exhaustion, remote filesystems or cloud providers.

`RETYPE_TEST_TIMEOUT_SECONDS` sets the per-step timeout for Clippy and the Unix installer and workspace tests from 1 to 3600 seconds; the default is 300. On Windows, the same limit is shared by the strict workflow, non-CLI workspace tests, doctests and CLI workspace tests. Windows uses cargo-nextest with four workers, no retries and a 60-second timeout for each test, so a hung test is reported without blocking the rest of the phase. It runs ignored tests rather than omitting them. The strict workflow is also included in the CLI workspace phase. A phase not reached before the shared deadline is reported as incomplete, and any timeout still fails the runner.

Formatting has a 60-second timeout and version probes have a 10-second timeout. `RETYPE_TEST_REPORT_DIR` and `RETYPE_TEST_TEMP_DIR` may name absolute locations for reports and scratch data. On Unix, choose a short scratch parent so test daemon sockets stay within the platform's path limit. Use a disposable local filesystem rather than real originals.

## 1. Baseline tests

```sh
cargo test --workspace --locked --no-fail-fast
```

This builds and runs the tests selected for the host. It is not complete encoder validation: optional tool tests can return early when prerequisites are absent, and Rust reports those returns as passes. Use strict tool mode for a verification run.

## 2. External prerequisites

Make the programs available on `PATH` in the same terminal that runs Cargo. Reopen the terminal after installation if the installer changed `PATH`.

| Test target | Required preparation |
|---|---|
| `fidelity`, `fidelity_axes` | ImageMagick 7.1 or newer, with 16-bit image support such as a Q16 build; PNG, JPEG, GIF and TIFF support |
| `keep` real watched workflow and core JPEG validation | ImageMagick 7.1 or newer with HEIC decoding and JPEG encoding; FFmpeg with MJPEG decoding |
| `video` | FFmpeg 6 or newer and FFprobe, with `libx264`, `aac` and `libmp3lame` encoders |
| `documents` | Pandoc 3 or newer and at least one usable PDF engine: `typst`, `xelatex`, `lualatex`, `pdflatex`, `weasyprint` or `wkhtmltopdf` |
| `archives` | Native ZIP/TAR cases need no external archiver; 7z cases require 7-Zip 23 or newer as `7zz`, `7z` or `7za` |
| `data_safety_transactions`, `data_safety_audit_additional`, core `safety` | Native archive and filesystem cases need no external encoder; a few shell/symlink cases are Unix-only |
| `data_safety_audit` | Unix shell and ordinary Unix utilities; currently excluded on Windows |
| Windows Job Object tests | No encoder, PowerShell script or extra runtime: helper processes use the Rust test binary itself |

Pandoc alone is insufficient for the document target. Typst is a straightforward standalone choice. TeX engines also need the packages and fonts used by Pandoc's template; an engine executable alone may not constitute a working installation. Keep Pandoc and the selected engine compatible. The default engine order is the order shown above.

Fedora's restricted FFmpeg builds may lack the required encoders. Check capabilities instead of assuming that an installed `ffmpeg` is sufficient:

```sh
magick -version
ffmpeg -version
ffprobe -version
ffmpeg -hide_banner -encoders
pandoc --version
typst --version
7z i
```

Use the PDF-engine and 7-Zip command names actually installed on the host. In the encoder listing, verify the three encoder names exactly. Native archive/cancellation/recovery tests do not require these programs.

## 3. Strict tool verification

`RETYPE_REQUIRE_TEST_TOOLS=1` makes the CLI image, fidelity, video, document and archive suites and core JPEG validation tests fail their prerequisite guards instead of returning early. It does not enable tests excluded by `cfg`, and is not a promise that every codec supported by an installed program has been exercised.

Linux/macOS:

```sh
RETYPE_REQUIRE_TEST_TOOLS=1 cargo test --workspace --locked --no-fail-fast -- --nocapture
```

PowerShell:

```powershell
$env:RETYPE_REQUIRE_TEST_TOOLS = '1'
cargo test --workspace --locked --no-fail-fast -- --nocapture
```

To check only the real-tool targets after installing their prerequisites:

```sh
cargo test -p retype-cli --locked --test keep --test fidelity --test fidelity_axes --test video --test documents --test archives --no-fail-fast -- --nocapture
```

Set strict tool mode in that shell before the targeted command too. Without it, `SKIPPED:` messages still identify missing prerequisites, but the test harness can report those tests as passed. A missing PDF engine is an error once Pandoc is present, including in an ordinary run.

### Strict native rename and undo gate

Run these on each release OS to require real watched and direct HEIC-to-JPEG conversions, normal source disposition and complete undo that restores the exact original bytes and removes the output. Strict mode fails rather than skipping if ImageMagick, its HEIC decoder or JPEG encoder, or FFmpeg is unavailable. The tests copy a checked-in synthetic HEIC fixture into private state and start their own daemons.

Linux/macOS:

```sh
RETYPE_REQUIRE_NATIVE_WORKFLOW=1 cargo test -p retype-cli --locked --test keep a_real_watched_heic_conversion_and_undo_preserve_the_original -- --nocapture
RETYPE_REQUIRE_NATIVE_WORKFLOW=1 cargo test -p retype-cli --locked --test keep a_real_direct_heic_conversion_and_undo_follow_watched_disposition -- --nocapture
```

PowerShell:

```powershell
$env:RETYPE_REQUIRE_NATIVE_WORKFLOW = '1'
cargo test -p retype-cli --locked --test keep a_real_watched_heic_conversion_and_undo_preserve_the_original -- --nocapture
cargo test -p retype-cli --locked --test keep a_real_direct_heic_conversion_and_undo_follow_watched_disposition -- --nocapture
```

This gate currently fails when the validator retains the visible source or the platform cannot complete destructive undo. A passing ordinary tool suite is not a substitute for this gate.

## 4. Filesystem and OS preparation

Use a normal, non-administrator account and a local filesystem supporting hard links and atomic renames. Prefer ext4/Btrfs on Linux, APFS on macOS and NTFS on Windows. Do not use a cloud-placeholder directory, network share, FAT/exFAT volume or synchronized folder as the test temporary directory.

Tests create their own temporary subdirectories. If Windows `TEMP`/`TMP` points at an unsuitable filesystem, redirect it to a user-writable NTFS directory before testing. Do not delete or relocate existing user data to do this. Current Windows tests do not require Developer Mode or administrator rights for symlinks; the Unix symlink cases remain excluded there.

Use a clean/default retype configuration for validation. Avoid exporting a custom `RETYPE_CONFIG` for the whole run; fixtures that need an override supply their own. Keep the desktop GUI and any manually registered real directories out of the test setup.

On Windows, destructive original cleanup remains disabled until directory-entry durability is implemented. A successful native conversion therefore retains its source, and destructive undo reports unsupported cleanup while preserving the original, outputs and recovery bytes. The portable tests assert that fallback; do not disable it merely to make Unix-style deletion assertions pass.

## 5. Focused safety and Windows process tests

```sh
cargo test -p retype-cli --locked --test data_safety_transactions --test data_safety_audit_additional
cargo test -p retype-core --locked --test safety
cargo test -p retype-cli --locked --lib daemon::watcher::tests
cargo test -p retype-core --locked --lib process_completion
```

On native Windows, also run:

```sh
cargo test -p retype-core --locked --lib execute::windows_tests -- --nocapture
cargo test -p retype-core --locked --lib fsx::windows_tests::directory_flush_probe_is_not_durability_evidence -- --exact --nocapture
cargo test -p retype-core --locked --lib correlate::tests::a_hard_link -- --nocapture
cargo test -p retype-cli --locked --test deferral --test video --no-fail-fast -- --nocapture
```

The Windows process tests create controlled descendants, require the process tree to finish before accepting completion, and verify kill-on-close after a timeout. The `process_worker` entry point does nothing in an ordinary parent test run. The real FFmpeg target is a separate check of the installed executable and any wrapper it uses.

The directory-flush probe logs whether a temporary directory can be opened and synced with read, write and combined access. The Windows runner includes it in `workspace-libraries` output. This is diagnostic only: a successful call does not prove directory-entry or power-loss durability and does not enable destructive cleanup. Record the scratch filesystem type with the results.

Twenty-five CLI archive/transaction safety tests now run on Windows instead of being excluded with their entire files. Twenty-eight cases in the three CLI safety binaries remain Unix-only: the main shell-based audit, two additional shell/symlink cases, and actual post-disposal crash recovery. Cross-platform ownership tests and Windows-specific process tests provide further coverage, not substitutes for the excluded cases.

## 6. Interpreting results

Record the Git revision, `rustc -vV`, tool versions, complete output and Cargo exit status. With `--no-fail-fast`, inspect all failed targets rather than only the last summary.

A successful `cargo check --target ...` verifies foreign-target compilation only. It does not execute Windows Job Objects, macOS filesystem/event APIs or native recovery.

Process-crash tests do not simulate power loss or a storage device that fails to honor synchronization. Power-loss, storage-fault, remote-filesystem and cloud-provider validation are separate release work and must use disposable virtual machines or scratch storage, not real originals.
