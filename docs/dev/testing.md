# Test preparation and platform validation

Run from the repository root with a supported Rust toolchain and native build tools. Use the actual target OS: WSL and a Linux container do not exercise Windows APIs. Tests use synthetic files, private temporary state and test daemons; do not register real folders or install a background service for them.

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
| `keep` real watched workflow | ImageMagick 7.1 or newer with HEIC read/write and JPEG support |
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

`RETYPE_REQUIRE_TEST_TOOLS=1` makes the CLI image, fidelity, video, document and archive suites fail their prerequisite guards instead of returning early. It does not enable tests excluded by `cfg`, and is not a promise that every codec supported by an installed program has been exercised.

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

Run this on each release OS to require a real watched HEIC-to-JPEG conversion, normal source disposition and a complete undo that restores the exact original bytes and removes the output. The strict mode fails rather than skipping if ImageMagick or its HEIC delegate is unavailable. The test uses private generated files and starts its own daemon.

Linux/macOS:

```sh
RETYPE_REQUIRE_NATIVE_WORKFLOW=1 cargo test -p retype-cli --locked --test keep a_real_watched_heic_conversion_and_undo_preserve_the_original -- --nocapture
```

PowerShell:

```powershell
$env:RETYPE_REQUIRE_NATIVE_WORKFLOW = '1'
cargo test -p retype-cli --locked --test keep a_real_watched_heic_conversion_and_undo_preserve_the_original -- --nocapture
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
cargo test -p retype-core --locked --lib correlate::tests::a_hard_link -- --nocapture
cargo test -p retype-cli --locked --test deferral --test video --no-fail-fast -- --nocapture
```

The Windows process tests create controlled descendants, require the process tree to finish before accepting completion, and verify kill-on-close after a timeout. The `process_worker` entry point does nothing in an ordinary parent test run. The real FFmpeg target is a separate check of the installed executable and any wrapper it uses.

Twenty-five CLI archive/transaction safety tests now run on Windows instead of being excluded with their entire files. Twenty-eight cases in the three CLI safety binaries remain Unix-only: the main shell-based audit, two additional shell/symlink cases, and actual post-disposal crash recovery. Cross-platform ownership tests and Windows-specific process tests provide further coverage, not substitutes for the excluded cases.

## 6. Interpreting results

Record the Git revision, `rustc -vV`, tool versions, complete output and Cargo exit status. With `--no-fail-fast`, inspect all failed targets rather than only the last summary.

A successful `cargo check --target ...` verifies foreign-target compilation only. It does not execute Windows Job Objects, macOS filesystem/event APIs or native recovery.

Process-crash tests do not simulate power loss or a storage device that fails to honor synchronization. Power-loss, storage-fault, remote-filesystem and cloud-provider validation are separate release work and must use disposable virtual machines or scratch storage, not real originals.
