# State, journal and identifiers

On-disk state for jobs, undo and calibration.

## 1. Layout

State directory: `RETYPE_STATE_DIR` when set, otherwise `dirs::state_dir()` joined with `retype`, falling back to `dirs::data_local_dir()` when the former is absent, which is the case on macOS. Unix socket paths are length-limited, so a state directory that would overflow the limit is a startup error naming the limit and the variable.

```
retype/
  jobs.jsonl                  append-only journal
  jobs.jsonl.corrupt.<ts>     quarantined journal
  lock                        exclusive lock, held by the running daemon
  undo.lock                   serializes quota reservations and store mutations
  undo-job.<id>.lock          OS lease for active execution or undo
  endpoint                    where the running daemon listens, one line
  daemon.log                  output of an auto-started daemon
  daemon.sock                 IPC endpoint on Unix; a named pipe on Windows
  watches.toml                registered directories
  bench.toml                  estimator calibration
  logs/
    <id>.log                  what the job's tool printed
  tools/
    manifest.toml             signed distribution list, as accepted
    manifest.toml.sig         detached signature over the manifest bytes
    bin/                      managed commands, first on the tool search path
    <artifact>/               unpacked tool, named by tool, variant, version and target
  undo/
    <id>/
      meta.toml               record description
      source.<ext>            an independent, content-verified original snapshot
```

`endpoint` exists so that a client never derives the endpoint itself. On Unix the socket path is predictable, but on Windows it is a pipe name that includes the machine identity, and two places computing that name from a file that either of them may create is a race with a silent failure mode: the client waits three seconds for a name nobody listens on. The daemon publishes where it listens after it binds and removes the file when it stops, so a missing file means no daemon rather than an unknown one.

Directory mode is `0700`, files `0600`. The store holds user data in plain form, so nothing here is world-readable.

The toolchain directory is owned by the GUI: it installs, updates and verifies the artifacts, while the CLI and daemon only read what is there. It holds executables, so its mode is `0700` on Unix. `tools/bin` is searched before `PATH`, `docs/advanced/converters.md` §2. An artifact is unpacked into its own directory and only then linked into `bin`, so an interrupted install cannot replace a working tool with a partial one.

## 2. Identifiers

A job id is a ULID: 26 lowercase characters, Crockford base32, 48-bit millisecond timestamp followed by 80 random bits. Properties required:

- Lexicographic order equals creation order
- Unique without coordination, so the job feed needs no id registry
- Lowercase in its canonical form, so an id is typed as it is printed and sorts as it prints
- Case-insensitive on input, because base32 excludes `I`, `L`, `O` and `U`; a handle may be typed in either case and is stored lowercase

The id is generated at job creation, before execution, so a deferred job is addressable by its notification immediately.

Machine identity is a separate per-install random field, stable across restarts, stored in `bench.toml`. It is not part of the id.

User-facing handle: a prefix of at least 4 characters. Resolution requires a unique match; an ambiguous prefix is an error listing the candidates. Listings print the shortest unambiguous prefix so copy-paste is short by default.

The rule is general rather than a property of job ids: every identifier this build mints is lowercase ASCII in its canonical form, and input is normalized to lowercase before parsing. Non-ASCII is rejected rather than case-folded, because Unicode folding is locale-dependent and would give one identifier several spellings. This covers job handles and state filters. It does not cover secrets or values another system defines, which keep their case.

An object type that needs a prefix writes `<type>_<payload>`, with `_` as the separator rather than `-`, so a payload may keep `-` as an internal grouping character. Prefixes are lowercase and reserved by object type. This build mints only job ids, which remain bare ULIDs; the journal schema does not carry a `job_` prefix.

## 3. Journal records

One JSON object per line. Every line is self-contained and carries its own version.

```json
{"v":"retype-journal-1","kind":"job","id":"01j8z...","created":1690000000123,"updated":1690000000456,
 "source":{"path":"/home/u/Pictures/a.heic","format":"heic","size":2431002,"mtime_ns":1690000000000},
 "intent":"jpg+quality85",
 "outputs":[{"path":"/home/u/Pictures/a.jpg","format":"jpg","state":"done","bytes":812345,"tier":"converted",
              "identity":{"kind":"unix","dev":2049,"ino":5243121,"birth_ns":1690000000000}},
            {"path":"/home/u/Pictures/a.png","format":"png","state":"failed","tier":"converted",
              "error":{"code":"E_TOOL_FAILED","message":"converter exited 1"}}],
 "state":"partial","backend":"magick","machine":"7f3c1a2b","error":null}
```

| Field | Meaning |
|---|---|
| `v` | Record schema identifier, `retype-journal-1` |
| `kind` | Record type; only `job` in version 1 |
| `id` | ULID |
| `created`, `updated` | Unix milliseconds |
| `source` | Path, format, size, mtime in nanoseconds |
| `intent` | Canonical intent string, or null for a plain conversion |
| `outputs` | Resolved outputs with format, per-output state, resulting size, tier, file identity and per-output error. `bytes`, `identity` and `error` are omitted until they exist, so a pending output is a path, a format, a state and a tier |
| `state` | `pending`, `queued`, `running`, `done`, `partial`, `failed`, `cancelled`, `superseded`, `needs-attention` |
| `backend` | Backend name that ran or will run the job: a tool's name, or the codec's, such as `zip`, when this build wrote the file itself |
| `machine` | Per-install identity |
| `error` | Null, or `{code, message}` with a journal-level code such as `E_JOB_INTERRUPTED`, `E_JOB_SUPERSEDED` or `E_TOOL_FAILED` |
| `parked` | Absent, or its reason, deadline, batch, estimate, requested and corrected intent, dropped tokens, source disposition, input fingerprint and confirmed `force` choice |
| `consume_source` | Whether the request permits source removal after safety checks; it is not proof that disposal happened |
| `source_disposition` | Absent for older or unfinished jobs, otherwise `pending`, `kept`, `removed`, `retained` or `unresolved`, with a reason for retention or uncertainty |

Per-output state is one of `pending`, `running`, `done`, `failed`. A `partial` job is one with at least one `done` and at least one not `done`, so the job state is derivable from its outputs and is stored anyway because a reader should not have to recompute it.

A record is a full snapshot of the job at that transition, so replay is last-wins per id and no record depends on another. Snapshots are affordable because a job has a handful of transitions. Per-output publication checkpoints can carry a `done` output inside a `running` job; only the latest top-level job state establishes terminal completion.

Initial publication holds the job-list mutex across journal append, synchronization and cache registration. IPC readers wait through that interval rather than returning `E_JOB_NOT_FOUND` for an already journaled job. Emit the event only after the snapshot is registered in the job list. A failed running checkpoint can produce an explicitly non-durable failure in memory, but it does not start conversion.

## 4. Journal rules

- Records are appended, never edited in place
- Sync every appended snapshot and its parent directory where supported, including running output checkpoints and partial outcomes
- After a journal write or sync failure, refuse subsequent appends until reopening and recovery rather than extending a possibly torn record
- Require a durable recovery manifest and completion snapshot before source disposal
- Line order defines time; wall-clock fields are informational
- A trailing line that does not parse is a torn write and is dropped silently
- A non-trailing line that does not parse is corruption: the file is copied without replacement to `jobs.jsonl.corrupt.<ts>`, then readable records are synced into a replacement journal. The damaged original remains in place until the replacement is ready; the event is reported once and records after the damage are still replayed
- A line whose `v` is not the supported identifier is skipped and reported once, so a downgrade remains usable
- Adding fields is compatible within an identifier. Removing or renaming a field requires a new identifier, and the previous one stays readable for one major release

Compaction rewrites live records into a temporary file and renames it into place. It runs on startup when the journal exceeds 10,000 lines or when more than half the records are superseded by later ones for the same id. The horizon is twice the undo retention with a floor of thirty days. Queued, running, pending, needs-attention, partial and failed jobs remain beyond that horizon until resolved. Cancelled jobs with a warning or completed outputs are also retained, so reconciliation cannot lose an unfinished retention transition.

## 5. Undo records

```
undo/<id>/meta.toml
```

```toml
version = 'retype-undo-1'
id = '01j8z...'
intent = 'jpg+quality85'
created = 1690000000123
machine = '7f3c1a2b'
source_path = '/home/u/Pictures/a.heic'
source_ext = 'heic'
source_size = 2431002
source_mode = 0o644
source_mtime_ns = 1690000000000
link = 'copy'
pinned = true
consume_source = true

[[outputs]]
path = '/home/u/Pictures/a.jpg'
ext = 'jpg'
```

New records always hold an independent copy. `source_fingerprint` records the original file identity, byte length and SHA-256 digest. The retained copy must match that digest whenever it is read. A hard link is not a snapshot and is no longer created. Old records without a fingerprint remain on disk but cannot authorize automatic recovery or deletion.

`outputs` lists intended non-keep outputs. `commits` records publication intents with staged file identities, content fingerprints and whether those exact bytes passed output validation before each output is published. An intent counts as a committed output only when the final file matches its identity and digest. Disposal additionally requires the validation flag; older intents without it do not authorize disposal. Undo never deletes an intended output merely because its pathname exists.

`consume_source` records whether undo may need to restore the original; it is not proof of disposal. A completed explicit keep does not overwrite later edits to the retained source. If that source is missing, or its recovery record is still pinned, undo verifies or restores the original first. Undo refuses changed or replaced outputs, syncs any restoration, and only then claims and removes verified job outputs. An occupied restoration name is acceptable only when it still holds the recorded original. Successful undo removes the record. Unsupported directory durability refuses destructive cleanup and retains the available files.

Source disposal requires every non-keep output to pass a supported validation check and be durably published, a durable independent backup, a durable completion record, no cancellation, and a successful identity/content check on the claimed source. The validation result identifies the staged file and its digest; the publication checkpoint and the final pre-publication check must match it. Recheck the backup and every committed output immediately before disposal; missing or changed files retain the source. Native archive outputs are reopened and their entry inventory is compared with the source before publication: names, kinds, sizes and content digests must agree, so a writer that drops, duplicates or alters an entry cannot authorize disposal. JPEG output from ImageMagick or FFmpeg must have JPEG start and end markers and decode without errors or warnings using both `magick` and `ffmpeg` from `PATH`. If either validator is unavailable, the source is retained and the job and daemon log name the missing tool. Other external output formats and custom backends cannot yet authorize disposal. These checks do not prove fidelity to the source. Partial, failed, cancelled and superseded jobs retain originals. Explicit keep-source groups retain originals. Missing backup capacity or backup failure also retains originals. A disposal that cannot prove the live source still holds the original leaves the recovery record pinned. Windows currently retains originals because directory-durability support is not implemented.

A `done` checkpoint with source disposition `pending` is persisted before disposal; the resulting disposition is persisted before undo retention starts. On restart a pending disposition becomes `unresolved`, and recovery remains pinned when a record exists. If recording the outcome fails after removal, live status reports that the source was removed and the recovery record remains protected.

Execution and recovery hold an OS-backed per-job lease through publication and disposal. Undo and record removal require the same exclusive lease; an active job returns `E_IO_LOCKED`. Cancellation must finish before undo proceeds. Process exit releases the lease automatically. Lock files persist; their existence is not an activity indicator.

Records are the durable truth for recoverability. The journal is a cache of history and progress: losing it costs listing and progress, never the ability to undo. Reconciliation therefore reads the undo store first.

## 5.1 Watch registrations

```toml
version = 'retype-watches-1'

[[watch]]
path = '/home/u/Documents'
recursive = true
added = 1690000000123
paused = false
```

The daemon owns this file. `watch` appends, `unwatch` removes, and the daemon re-reads it on start so registrations survive restarts and reboots. Hand editing is tolerated and validated like any other configuration file.

## 5.2 Resume

`partial` means at least one output succeeded and at least one failed or never started.

Resume accepts partial jobs and failed jobs marked `E_JOB_INTERRUPTED`. Its input is a private working copy of the job's digest-verified retained original, never an arbitrary file at the old source pathname. Missing or unverifiable retained bytes make resume unavailable.

Resume skips only outputs whose recorded identity, size and digest still match. Any other occupied path is refused without `--force`, including paths whose platform identity is unavailable. `--force` reruns outputs but cannot override protection of the job's input. A successful resume retains its backup for normal undo retention and leaves any live source in place.

Unfinished records are pinned against both quota eviction and expiry. A successful completion releases the pin, records `retention_started` in Unix milliseconds and retains the backup. A successful resume starts a new retention period; a failed or interrupted recovery leaves its record pinned. Cancelling a partial job, or running work with completed outputs, records the cancellation before starting retention. The live original, retained original and every completed output must still verify; otherwise the record stays pinned and the job reports a warning. A cancelled job with no completed outputs keeps its record pinned. Records without a known retention start are not assumed expired.

## 5.3 Supersession

A rename chain that spans two batches cannot be coalesced, so it has to be resolved against the job that is already running.

```
job A: source photo.heic, target photo.jpg     (running)
fact B: source photo.jpg,  target photo.webp
```

Rules, in order:

1. If A has already finished, B is an ordinary job from `photo.jpg`. The second encode happens because time passed, and there is nothing to undo
2. If A is queued and has not started, cancel it and retarget: the job becomes `photo.heic` to `photo.webp`, with B's intent. Nothing was spent
3. If A is running, request cancellation and retarget the replacement. Discard only private uncommitted stages; retain already published outputs and their recovery record

Replacement is not only a target change. Two details make it safe, and the first one is not special to supersession:

- **The source name is restored before any conversion**, by §5.4. A had renamed `photo.heic` to `photo.jpg` and not yet committed an output, so cancelling it leaves the source bytes at the wrong name; the replacement starts after that name is back
- **The replacement uses the same disposal checks.** A rename-derived job may remove its source only after the full validation, durability and ownership checks in §5. `consume_source` is eligibility, not permission to bypass those checks

If the restored source name is already taken, by a file the user created or by another rename, the restore is refused and B becomes an ordinary job from A's target instead. A wrong file is worse than a second encode.

Two renames reach the replacement, and the second is the common one. A rename of what A is writing is the decision about the result; a rename of the *source* name is the same decision taken a moment later, and it is what happens in practice, because during a conversion the file shows its source name again, §5.4. Both are the same supersession.

A superseded job's uncompleted outputs are `skipped` rather than `failed`; already published outputs remain `done`. Its record says `superseded` and the replacement carries the chain.

A's recovery record is not removed when cancellation is requested. The replacement creates its own record. Cleanup must not assume that a superseded job published nothing or that its original still exists at the old pathname.

## 5.4 Input naming

A rename leaves the source bytes at the new name, so a converter reading the file directly sees the wrong extension: `photo.jpg` containing HEIC. There are two ways to give it a usable path.

| Strategy | Effect during conversion | Crash leaves | exFAT |
|---|---|---|---|
| Restore the source name first | `photo.heic` (correct) | A correctly named file | No copy needed |
| Independent temporary input when the source name is occupied | Original remains at the requested name | Original plus possible private stages | Supported by copying |

Restore the source name with an atomic no-replace rename and persist it before conversion. If that name is occupied, preserve its occupant and read an independent private copy. Ordinary publication cannot overwrite the original-bearing target; a collision is reported instead.

The cost is visible and worth stating: during a long conversion the file shows its *source* name again, so a twenty minute video transcode looks like the rename was undone and then reapplied. That is honest, and it is preferable to a file that claims to be a JPEG for twenty minutes while holding HEVC.

Every output reads the same verified private working snapshot. Output staging uses an exclusively created private directory on the destination filesystem, with a real output extension and no predictable shared filename. Publication uses atomic no-replace semantics unless replacement was explicitly authorized. Temporary and recovery directories are excluded from watch correlation.

Source removal first atomically claims the live path into a persistent recovery directory, then checks the claimed identity and digest. A changed file is restored without overwriting another file, or left at the reported recovery path. No unchecked public pathname is unlinked.

## 5.5 Quota and overflow

The current store is bounded by age and size. Only unpinned records with a known retention start beyond the configured period are eligible for automatic removal. If the remaining records and the new source do not fit within `quota`, no record is removed and the conversion currently proceeds with its source retained and no new undo record. The `ask`, `evict` and `skip` settings do not override minimum retention; interactive overflow decisions and soft-goal admission are not yet implemented.

Accounting is retained source bytes, `docs/reference/config.md` §9. Quota decisions, expiry and record creation are one locked transaction. Every record selected for expiry is leased first, then the new record is written, then the old ones are removed, so a failed save leaves the old records in place rather than an empty store and no new record. A process mutex and an operating-system file lock serialize store writers, including undo clients. Pins are persisted in the manifest and survive process death.

## 6. Startup reconciliation

1. Acquire the lock. Failure means another daemon owns the state directory, and the process exits with that message
2. Load `bench.toml`, then the journal, then the undo record index
3. For every journal job in `queued`: mark it `failed` with `E_JOB_INTERRUPTED` and ask the user to review the input and resubmit the recorded intent. The queued record does not yet include a content fingerprint, so size and modification time cannot establish that the input is unchanged; do not automatically replay it
4. For every journal job in `running`: verify publication intents from its recovery manifest against final files. Reconstruct the intended outputs and mark verified publications done. Mark the job interrupted, with state `partial` when any publication survived and `failed` otherwise. Both outcomes can resume using verified retained bytes, §5.2
5. For every journal job in `pending`: re-arm its timer. When due, require its recorded input fingerprint, recheck state under the admission lock and journal the queued transition; older records without a fingerprint wait for review and resubmission
6. For a cancelled job with completed outputs and a pinned record, verify the live original, retained original and completed outputs before starting retention. Retain recovery records without journal entries for direct undo; do not infer completion from publication intents alone
7. For every journal job whose record directory is missing, leave the state as recorded and mark undoing it as unavailable
8. Drop undo records past retention, then compact if the thresholds are met

Nothing is deleted during reconciliation. Anything inconsistent is reported and left in place for inspection.

## 7. Calibration

`bench.toml` holds per-machine measurements and the machine identity.

```toml
version = 'retype-bench-1'
machine = '7f3c1a2b'

[[throughput]]
source = 'heic'
target = 'jpg'
backend = 'magick'
family = 'image'
bytes_per_second = 2400000.0
size_ratio = 0.28
sample_bytes = 4800000
tool_version = 'ImageMagick 7.1.2-34'
measured = 1793380889377
```

A measurement is keyed by the pair it was taken on, not by one format: HEIC decodes slowly and JPEG encodes quickly, so `heic` to `jpg` and `png` to `jpg` are different jobs and no per-format figure describes both. `family` is kept beside the pair because it is what an unmeasured pair falls back to.

The two numbers are the source bytes the machine converts per second, and the output bytes per source byte. Time follows from the first and size from the second, and both are per pair because a ratio is a property of the pair: the same photograph is 291 KB as WebP and 807 KB as AVIF.

`sample_bytes` records what the rate was measured on, since a rate taken on a 240 KB file and one taken on a 2 MB file are not the same evidence. `tool_version` is the version string of the backend that ran, because a rate is evidence about a build, `docs/advanced/fidelity.md` §1.

`retype bench` writes this file. It generates one sample per family in the format `[bench.seed]` names, using an argv `[bench.sample]` declares, and then measures every target that compiles from that source. `--pair` and `--family` narrow it. Measuring the same pair twice replaces the earlier answer rather than leaving two for one question.

Estimates use a measurement when the pair, backend and machine match, and a family constant otherwise. Every constant carries its provenance, with the measurement it came from.

## 8. Provenance

Mechanisms, guarantees and failure modes are specified in `docs/advanced/provenance.md`. This document owns only the field: the journal records the identity of every output at completion.

```json
"outputs":[{"path":"/home/u/Pictures/a.jpg","format":"jpg","bytes":812345,"tier":"converted",
            "identity":{"kind":"unix","dev":2049,"ino":5243121,"birth_ns":1690000000000}}]
```

```json
"identity":{"kind":"windows","volume_serial":"1A2B3C4D","file_id":"0000000000000000000000000001F4A3"}
```

`kind` is `unix` or `windows`. A record without `identity` cannot answer by identity, and `retype why` then falls back to path matching or reports unknown.
