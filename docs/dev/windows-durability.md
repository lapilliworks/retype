# Windows NTFS durability qualification

Windows source disposal and destructive undo require fixed local NTFS paths. Each operation qualifies its participating directories, propagates synchronization failures and uses handle-owned removal with cancellable recovery, per-job ownership and short store locks. Unsupported paths retain originals and recovery. Successful API calls or process-crash tests alone are not durability qualification.

## Supported cleanup scope

- Fixed local NTFS volumes with volume GUID paths
- Regular files and directories without reparse points, offline attributes or recall attributes
- Directory ancestors held against rename during synchronization and handle-owned removal
- File identity and content checked through the same handle used to claim and remove the file
- Write, delete, synchronization and permission failures propagated without a pathname-deletion fallback

SMB, removable media, other filesystems, junction ancestors and cloud placeholders are outside this scope. Directory synchronization and cleanup fail explicitly on unsupported paths. Recovery storage and committed output directories are checked before source disposal; record deletion also qualifies the recovery directory before changing it. This is not qualification of every storage device or synchronization provider.

Directory creation persists newly created ancestor names. Publication flushes the staged file and its directory before rename, then flushes the final file and destination directory. Cleanup moves the checked file into persistent recovery space, flushes both affected directories, removes the claimed object by handle, then flushes the removal. Undo flushes the restored original before removing outputs, and synchronizes record removal before reporting completion.

## Automatic process-crash and error tests

Run in a non-elevated terminal on disposable local NTFS storage. Keep real originals and manually registered watches out of the test environment.

```powershell
cargo test -p retype-core --locked --lib fsx::windows::tests -- --nocapture
cargo test -p retype-core --locked --lib durability_tests:: -- --nocapture --test-threads=1
```

The matrix covers twenty-two operation/checkpoint pairs, cancellation at recovery boundaries, read-only outputs and denied-delete permissions. Each separately bounded test injects an error, then terminates a worker process at that checkpoint and retries recovery. Both paths check original and backup bytes, untouched unrelated files and cleanup outcomes. The Windows process test also attempts to write and rename a held claim. Same-content replacements and junctions in the source/output or recovery paths must refuse cleanup without removing the original or recovery. Checkpoint injection is confined to its thread and selected publication.

Checkpoint controls exist only in the core unit-test build; the release executable does not read `RETYPE_DURABILITY_*`. `supports_durable_cleanup()` reports implementation availability, not permission for every Windows path. The normal strict runner requires the real watched/direct HEIC disposition and undo workflows to complete on supported local NTFS.

| Operation | Checkpoints |
| --- | --- |
| Publish | `publication.intent-durable`, `publication.prepared`, `publication.renamed`, `publication.durable` |
| Dispose source | `cleanup.claimed`, `cleanup.claim-durable`, `cleanup.unlinked`, `cleanup.complete`, `disposition.recorded`, `disposition.complete` |
| Undo | `undo.started`, `undo.intent-durable`, the three staged-file publication checkpoints, `undo.source-durable`, all four cleanup checkpoints, `undo.before-forget`, `undo.complete` |

`prepared` follows the durable publication intent and staged-file flushes. `renamed` precedes final publication synchronization. `claimed` precedes claim synchronization. `unlinked` precedes removal synchronization. A recovery-directory orphan may remain after interruption; it is retained rather than guessed safe to delete. Inspect it before manual cleanup.


## Actual disk exhaustion

`tests/windows-storage-faults.ps1` creates a new 64 MiB virtual NTFS disk, runs the core test worker with a limited interactive token, fills only that volume, verifies a storage error preserves recovery and outputs, then frees the filler and checks exact-byte recovery. It detaches the disk and retains the image, hashes and reports. The worker refuses a volume larger than 128 MiB. The orchestrator requires administrator rights only to create and attach the synthetic disk; no existing disk is selected or formatted.

Build the core test worker as an ordinary user, then supply its exact executable path to the elevated orchestrator:

```powershell
pwsh -NoProfile -File .\tests\windows-storage-faults.ps1 -Worker '<core test executable>' -Report '<new evidence directory>'
```

Use a logged-in qualification user. A failed test retains its evidence; inspect it before retrying in a new directory. The test does not power off the VM or qualify cache-loss durability.

## Disposable VM interruption

A process termination does not discard the Windows filesystem cache. Use a disposable VM for guest-interruption evidence. See [Hyper-V collection](hyperv.md) for read-only host, guest and disk identification. The collector does not execute this procedure or change VM power state.

Use a new PowerShell session inside the guest, with no Retype service or client configured to run automatically after boot. Preserve the same virtual disks between interruption and inspection; do not revert a checkpoint. Record the revision, tracked diff, binary SHA-256, filesystem, elevation status, chosen checkpoint and VM/storage configuration outside the guest before interruption.

### Build the worker

```powershell
$artifacts = @(cargo test -p retype-core --locked --lib --no-run --message-format json |
	ForEach-Object { $_ | ConvertFrom-Json })
if ($LASTEXITCODE -ne 0) { throw 'Worker build failed' }
$test = @($artifacts | Where-Object {
	$_.reason -eq 'compiler-artifact' -and $_.profile.test -and $_.target.name -eq 'retype_core' -and $_.executable
} | Select-Object -ExpandProperty executable)
if ($test.Count -ne 1) { throw 'Expected one core unit-test executable' }
$worker = $test[0]
Get-FileHash -LiteralPath $worker -Algorithm SHA256
git rev-parse HEAD
```

### Prepare one new synthetic fixture

The worker refuses an existing preparation directory. Do not select a directory containing real files. This example selects undo immediately after output unlinking; repeat with the operation/checkpoint pairs above.

```powershell
$env:RETYPE_DURABILITY_ROOT = Join-Path $env:TEMP ('retype-durability-' + [guid]::NewGuid().ToString('N'))
$env:RETYPE_DURABILITY_OPERATION = 'undo'
$env:RETYPE_DURABILITY_CHECKPOINT = 'cleanup.unlinked'
$env:RETYPE_DURABILITY_ACTION = 'prepare'
& $worker --exact durability_tests::recovery_worker --nocapture
if ($LASTEXITCODE -ne 0) { throw 'Fixture preparation failed' }
$env:RETYPE_DURABILITY_ROOT
```

Record the printed absolute fixture path outside the guest. The fixture contains its request, ownership marker, synthetic files, recovery record and journal. Preparation flushes it before the interrupted operation starts.

### Interrupt at the checkpoint

```powershell
$env:RETYPE_DURABILITY_ACTION = 'run'
& $worker --exact durability_tests::recovery_worker --nocapture
```

Wait for `RETYPE_CHECKPOINT <name>`. The worker pauses for up to 120 seconds, then fails if not interrupted. The guest marker is deliberately not flushed: flushing it could harden the operation being tested. The observed checkpoint must be recorded outside the guest.

From the Hyper-V host, explicitly turn off only the disposable VM after observing that checkpoint. Do not shut down the guest cleanly. Do not automate power operations against a VM containing real data.

### Inspect before recovery

Boot the same VM and disks. Before starting Retype or recovering the fixture, preserve the entire fixture directory, including hidden staging and recovery directories. Restore `$worker` and `$env:RETYPE_DURABILITY_ROOT` to their recorded paths in a new guest PowerShell session.

```powershell
$env:RETYPE_DURABILITY_ACTION = 'inspect'
& $worker --exact durability_tests::recovery_worker --nocapture
if ($LASTEXITCODE -ne 0) { throw 'Pre-recovery inspection failed; retain all evidence' }
$env:RETYPE_DURABILITY_ACTION = 'recover'
& $worker --exact durability_tests::recovery_worker --nocapture
if ($LASTEXITCODE -ne 0) { throw 'Recovery failed; retain all evidence' }
```

Inspection validates synthetic originals, retained bytes, visible outputs, unrelated files and record paths without performing recovery. Recovery restores the original and removes only matching outputs. Repeating recovery is harmless. No automatic fixture deletion is performed by the standalone worker.

Close the dedicated PowerShell session after testing so its worker environment does not affect a later Cargo run. Keep failed fixtures and transcripts in persisted storage.

## Qualification boundary

Windows 11 x86-64 qualification covered all twenty-two operation/checkpoint pairs using forced guest interruption, unchanged virtual disks, pre-recovery snapshots, non-administrator inspection and repeat recovery. The matrix completed across separate runs; two earlier wrapper/OS-startup failures were preserved, and their original interrupted fixtures later recovered with the qualified worker. Native cancellation, permission refusal, mutation/replacement, actual tiny-volume ENOSPC and unsupported-path checks are separate requirements.

Before any further guest interruption, verify that startup has settled and that servicing, setup and restart work are not pending. A reachable PowerShell Direct endpoint is not readiness. Keep the observed checkpoint outside the guest without adding a guest flush that could harden the tested operation. A reported OS-wide implementation capability never substitutes for the participating-directory checks.

A forced VM power-off tests interruption of the guest. It does not remove host power, bypass every virtualization cache or prove physical-device flush compliance. Passing this procedure must not be described as universal power-loss or storage-fault safety.
