# Platform test follow-up

Follow-up fixes start from `b418e39`. The supplied platform transcripts exposed failures that the initial Linux run did not exercise. See [test preparation](../../../testing.md) before repeating the validation.

## Supplied results

| Report | Passed | Failed |
|---|---:|---:|
| `data-safety-tests-fedora-kde.txt` | 712 | 1 |
| `data-safety-tests-macos.txt` | 708 | 2 |
| `data-safety-tests-windows.txt` | 620 | 5 |

The 57 original focused safety cases passed on Linux and macOS. Windows originally excluded all 51 CLI safety cases and ran five core ownership cases. These totals count subprocess helper entry points and optional tool guards, not independent defect scenarios.

## Fixes and regression coverage

| Problem | Fix | Regression evidence |
|---|---|---|
| Fidelity reruns fail with `File exists` on every platform | Private per-invocation measurement workspaces, including benchmark sample/output generation; retain older artifacts rather than overwrite them | Existing stale-claim test reproduced locally with ImageMagick; new repeat-benchmark test fails at `b418e39`; concurrent fidelity runs and repeated measurements pass with the fix |
| macOS supersession test mistakes a running checkpoint for completion | Parse latest top-level job snapshots, not nested output-state substrings | New prepublication-checkpoint predicate test; supersession and deferral integration targets pass locally |
| Windows resolves a journaled pending job as `E_JOB_NOT_FOUND` | Hold the job-list mutex across append, synchronization and cache registration; expose events only after registration | Deterministic test pauses after a real journal append and verifies that IPC's job-list lookup must block; failed checkpoints do not publish jobs |
| Windows FFmpeg run is rejected immediately after its primary process exits | Wait up to five seconds for actual Job Object quiescence, checking cancellation throughout; timeout still rejects the output and closes containment | Three platform-neutral completion tests; two new Windows-native descendant tests plus their Rust helper entry point |
| Windows hard-link observations always report one link | Read identity and `nNumberOfLinks` from the same open Windows file handle | Existing hard-link correlation cases from the failing Windows transcript; native hard-link input protection also runs in the portable transaction suite |

The supplied FFmpeg log does not establish whether the nonzero job count was delayed accounting or a genuinely live descendant. Waiting for the actual zero-count condition handles either without treating a delay itself as proof of completion. Unix's immediate rejection of surviving helpers remains unchanged.

## Coverage and prerequisites

The portable native transaction and archive tests no longer have a blanket Unix-only gate. Windows now selects 25 CLI safety cases, in addition to the core ownership cases and new Job Object tests. Cases involving actual source disposal still require supported directory durability. Windows tests verify source/backup retention and the explicit unsupported-undo result instead of disabling that protection.

`RETYPE_REQUIRE_TEST_TOOLS=1` makes missing codec prerequisites fail the CLI tool-test guards. An intentional empty-`PATH` control verifies that strict mode fails; the ordinary optional mode reports `SKIPPED:` and returns. This corrects the interpretation of the earlier all-pass result: fidelity tests had returned early without ImageMagick and had not exercised the stale-claim path.

No blanket cross-platform sign-off follows from compilation or a successful subset. Twenty-eight cases in the three CLI safety binaries remain Unix-only. The Windows-specific changes still require a fresh native Windows run; the supplied native transcripts predate these fixes.

## Local verification

Available tools: ImageMagick 7.1.2-34 Q16, FFmpeg 9.0.2 with the three required encoders, Pandoc 3.11, and 7-Zip 26.05.

- Focused archive, transaction, fidelity, image-axis, video, deferral and supersession targets: **55 passed** with strict tool mode
- Deterministic publication and process-completion unit tests: **5 passed**
- Benchmark/fidelity workspace tests: **7 passed**, including the fixed stale-claim path
- Full strict-tool workspace run: **721 passed, 2 failed because no PDF engine was installed**; the only failed target was `documents`
- Clippy passed with the existing `clippy::type_complexity` warning waived

The prerequisite failure is explicit: Pandoc needs one of `typst`, `xelatex`, `lualatex`, `pdflatex`, `weasyprint` or `wkhtmltopdf` on `PATH`. No replacement PDF implementation or test bypass was added. Install a supported engine, then repeat `documents` and the full strict run.

Evidence:

- [Fidelity before the fix](fidelity-before.log)
- [Repeat benchmark before the fix](bench-before.log)
- [Focused tool-enabled results](targeted-tests.log)
- [Publication synchronization tests](publication-tests.log)
- [Completion polling tests](process-completion-tests.log)
- [Full strict-tool run, including the missing-PDF-engine failures](workspace-tools-tests.log)

Original supplied transcripts remain untouched in the repository root. Additional execution logs and persistent review backups are in `worktrees/platform-safety-fixes/`.
