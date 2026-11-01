# State, journal and identifiers

On-disk state for jobs, undo and calibration.

## 1. Layout

State directory: `RETYPE_STATE_DIR` when set, otherwise `dirs::state_dir()` joined with `retype`, falling back to `dirs::data_local_dir()` when the former is absent, which is the case on macOS. Unix socket paths are length-limited, so a state directory that would overflow the limit is a startup error naming the limit and the variable.

```
retype/
  jobs.jsonl                  append-only journal
  jobs.jsonl.corrupt.<ts>     quarantined journal
  lock                        exclusive lock, held by the running daemon
  endpoint                    where the running daemon listens, one line
  daemon.log                  output of an auto-started daemon
  daemon.sock                 IPC endpoint on Unix; a named pipe on Windows
  watches.toml                registered directories
  bench.toml                  estimator calibration
  tools/
    manifest.toml             signed distribution list, as accepted
    manifest.toml.sig         detached signature over the manifest bytes
    bin/                      managed commands, first on the tool search path
    <artifact>/               unpacked tool, named by tool, variant, version and target
  undo/
    <id>/
      meta.toml               record description
      source.<ext>            the original bytes, hard link or copy
```

`endpoint` exists so that a client never derives the endpoint itself. On Unix the socket path is predictable, but on Windows it is a pipe name that includes the machine identity, and two places computing that name from a file that either of them may create is a race with a silent failure mode: the client waits three seconds for a name nobody listens on. The daemon publishes where it listens after it binds and removes the file when it stops, so a missing file means no daemon rather than an unknown one.

Directory mode is `0700`, files `0600`. The store holds user data in plain form, so nothing here is world-readable.

The toolchain directory is owned by the GUI: it installs, updates and verifies the artifacts, while the CLI and daemon only read what is there. It holds executables, so its mode is `0700` on Unix. `tools/bin` is searched before `PATH`, `docs/tools.md` §2. An artifact is unpacked into its own directory and only then linked into `bin`, so an interrupted install cannot replace a working tool with a partial one.

## 2. Identifiers

A job id is a ULID: 26 lowercase characters, Crockford base32, 48-bit millisecond timestamp followed by 80 random bits. Properties required:

- Lexicographic order equals creation order
- Unique without coordination, so the job feed in needs no id registry
- Lowercase in its canonical form, so an id is typed as it is printed and sorts as it prints
- Case-insensitive on input, because base32 excludes `I`, `L`, `O` and `U`; a handle may be typed in either case and is stored lowercase

The id is generated at job creation, before execution, so a deferred job is addressable by its notification immediately.

Machine identity is a separate per-install random field, stable across restarts, stored in `bench.toml`. It is not part of the id.

User-facing handle: a prefix of at least 4 characters. Resolution requires a unique match; an ambiguous prefix is an error listing the candidates. Listings print the shortest unambiguous prefix so copy-paste is short by default.

## 3. Journal records

One JSON object per line. Every line is self-contained and carries its own version.

```json
{"v":"retype-journal-1","kind":"job","id":"01J8Z...","created":1690000000123,"updated":1690000000456,
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
| `error` | Null, or `{code, message}` with a journal-level code such as `E_INTERRUPTED`, `E_BACKEND_FAILED`, `E_DIAGNOSIS` |
| `parked` | Absent, or why the job is waiting and until when: `reason`, `deadline`, `batch`, `estimate`, `requested`, `corrected` and `dropped`, `docs/deferral.md` §11 |

Per-output state is one of `pending`, `running`, `done`, `failed`. A `partial` job is one with at least one `done` and at least one not `done`, so the job state is derivable from its outputs and is stored anyway because a reader should not have to recompute it.

A record is a full snapshot of the job at that transition, so replay is last-wins per id and no record depends on another. Snapshots are affordable because a job has a handful of transitions.

## 4. Journal rules

- Records are appended, never edited in place
- `fsync` on transitions into `queued`, `pending` and `done`; other transitions are best effort, because reconciliation covers the gap
- Line order defines time; wall-clock fields are informational
- A trailing line that does not parse is a torn write and is dropped silently
- A non-trailing line that does not parse is corruption: the file is moved to `jobs.jsonl.corrupt.<ts>`, a new journal is started, and the event is reported once. Records after the damage are still replayed, because losing jobs that are intact on disk would be a second failure on top of the first
- A line whose `v` is not the supported identifier is skipped and reported once, so a downgrade remains usable
- Adding fields is compatible within an identifier. Removing or renaming a field requires a new identifier, and the previous one stays readable for one major release

Compaction rewrites live records into a temporary file and renames it into place. It runs on startup when the journal exceeds 10,000 lines or when more than half the records are superseded by later ones for the same id. The horizon is twice the undo retention with a floor of thirty days, so raising retention lengthens it and lowering it never shortens the journal below a month.

## 5. Undo records

```
undo/<id>/meta.toml
```

```toml
version = 'retype-undo-1'
id = '01J8Z...'
intent = 'jpg+quality85'
created = 1690000000123
machine = '7f3c1a2b'
source_path = '/home/u/Pictures/a.heic'
source_ext = 'heic'
source_size = 2431002
source_mode = 0o644
source_mtime_ns = 1690000000000
link = 'hard'
consume_source = true

[[outputs]]
path = '/home/u/Pictures/a.jpg'
ext = 'jpg'
```

The source is a hard link when the source and the store share a filesystem, detected by device id, and a copy otherwise. `link` records which.

`consume_source` is true for a rename-derived job and false for `retype convert`, whose source never moves. Undo restores the source path with `source_ext`, `source_mode` and `source_mtime_ns`, removes every output that was produced, and deletes the record. A record whose `consume_source` is false removes its outputs and leaves the source untouched. Undoing a `partial` job therefore removes the outputs that exist and leaves nothing behind. Restoring a copy sets mtime explicitly, so undo does not depend on the journal.

Records are the durable truth for recoverability. The journal is a cache of history and progress: losing it costs listing and progress, never the ability to undo. Reconciliation therefore reads the undo store first.

## 5.1 Watch registrations

```toml
version = 'retype-watches-1'

[[watch]]
path = '/home/u/Documents'
recursive = true
added = 1690000000123
```

The daemon owns this file. `watch` appends, `unwatch` removes, and the daemon re-reads it on start so registrations survive restarts and reboots. Hand editing is tolerated and validated like any other configuration file.

## 5.2 Resume

`partial` means at least one output succeeded and at least one failed or never started.

Resume input, in order:

1. The source path, if it still carries the recorded source identity, which is the case when no output has replaced it
2. The undo record for the job, which holds the original bytes as a hard link or a copy
3. Nothing, which is `E_JOB_RESUME_UNAVAILABLE`

Resume skips outputs whose recorded identity matches a file that still exists with the recorded size, and runs the rest. `--force` ignores the recorded identities and runs every output again. Identity is `(device, inode)` on Unix; on Windows the stable metadata API exposes no file id, so a resume there runs the output again rather than treating a file with the right name and size as its own work.

Resume is therefore available for as long as the undo record survives retention, and unavailable afterwards. That coupling is deliberate rather than incidental: it makes retention a single, documented knob instead of two.

## 5.3 Supersession

A rename chain that spans two batches cannot be coalesced, so it has to be resolved against the job that is already running.

```
job A: source photo.heic, target photo.jpg     (running)
fact B: source photo.jpg,  target photo.webp
```

Rules, in order:

1. If A has already finished, B is an ordinary job from `photo.jpg`. The second encode happens because time passed, and there is nothing to undo
2. If A is queued and has not started, cancel it and retarget: the job becomes `photo.heic` to `photo.webp`, with B's intent. Nothing was spent
3. If A is running, cancel it, discard its partial outputs, and replace it the same way. The spent work is lost, and that is the intended outcome: the user kept changing their mind, and finishing the old job would produce a file they no longer want

Replacement is not only a target change. Two details make it safe, and the first one is not special to supersession:

- **The source name is restored before any conversion**, by §5.4. A had renamed `photo.heic` to `photo.jpg` and not yet committed an output, so cancelling it leaves the source bytes at the wrong name; the replacement starts after that name is back
- **The replacement consumes its source.** It reads `photo.heic`, writes `photo.webp`, and removes `photo.heic` on commit, because the rename that started the chain moved the file. A job therefore carries `consume_source`, true for rename-derived jobs and false for `retype convert`, which never deletes what it was given

If the restored source name is already taken, by a file the user created or by another rename, the restore is refused and B becomes an ordinary job from A's target instead. A wrong file is worse than a second encode.

Two renames reach the replacement, and the second is the common one. A rename of what A is writing is the decision about the result; a rename of the *source* name is the same decision taken a moment later, and it is what happens in practice, because during a conversion the file shows its source name again, §5.4. Both are the same supersession.

A superseded job's outputs are `skipped` rather than `failed`: nothing went wrong with the file, the work was overtaken. Its record says `superseded` and the replacement carries the chain.

The undo record follows the bytes, not the job, and A's record is removed rather than adopted: it is a name for bytes still where A left them, so the replacement records its own when it starts. Adopting it would mean rewriting an id and an intent to say what the next record will say anyway. One record, one restore target, either way.

## 5.4 Input naming

A rename leaves the source bytes at the new name, so a converter reading the file directly sees the wrong extension: `photo.jpg` containing HEIC. There are two ways to give it a usable path.

| Strategy | Effect during conversion | Crash leaves | exFAT |
|---|---|---|---|
| Restore the source name first | `photo.heic` (correct) | A correctly named file | No copy needed |
| Temporary link to the old name | `photo.jpg` (wrong) | A mislabelled file plus a hidden link | Needs a full copy |

Restore first, always. The file is renamed back to its source name before the conversion starts, so the name matches the bytes at every instant rather than only after commit. The temporary link survives as a fallback for the one case that defeats this: the source name being occupied by something else.

The cost is visible and worth stating: during a long conversion the file shows its *source* name again, so a twenty minute video transcode looks like the rename was undone and then reapplied. That is honest, and it is preferable to a file that claims to be a JPEG for twenty minutes while holding HEVC.

Because the file is at its source name, every rename-derived job has the same shape: read the source path, write each output, remove the source on commit. The source is opened once per job and read by every output, because a job with two outputs must not lose its input to the first one. There is no in-place special case. A crash anywhere leaves either the correctly named source or a complete output, which is what reconciliation expects.

## 5.5 Quota and overflow

The store is bounded by age first and by size second. A record is kept only when it fits, and a conversion is never blocked or delayed by the bookkeeping: the answer can be "no record", and the conversion proceeds.

Order of decisions, for a record that costs `s` bytes:

1. Drop records past retention. This is policy rather than overflow, so it happens whatever the answer below
2. If the records kept plus `s` fit within `quota`, keep the record
3. If `s` alone is larger than `quota`, nothing is evicted, because evicting the whole store would still not make room. The source goes to the operating system's trash instead, so the user can still get the file back
4. Otherwise `on_overflow` decides:
   - `evict` drops the oldest records until the new one fits
   - `skip` keeps no record and the source is removed, because a quota that is full is a choice about the machine rather than a shortage
   - `ask` asks; until notifications exist this build reads it as `evict` and says so once at startup

Accounting is source bytes, which is what a hard link actually costs, `docs/config.md` §9. Nothing is dropped for a record that will not exist, and no record is dropped while a conversion is waiting for it.

## 6. Startup reconciliation

1. Acquire the lock. Failure means another daemon owns the state directory, and the process exits with that message
2. Load `bench.toml`, then the journal, then the undo record index
3. For every journal job in `queued`: nothing ran, so nothing was written. If the source still exists and its ext and mtime match, put it back in the queue with the request the journal recorded, including the output path, which is the only place an explicit `-o` survives. Otherwise mark it `failed` with `E_INTERRUPTED`
4. For every journal job in `running`: mark it `failed` with `E_INTERRUPTED`. A job that was running may have committed some of its outputs, so running it again would either overwrite them or fail on them: `retype jobs resume` is the path that skips the outputs whose recorded identity still matches, §5.2
5. For every journal job in `pending`: it is parked, so it keeps its deadline and its timer is re-armed rather than run, `docs/deferral.md` §7
6. For every undo record without a journal entry, keep it and rebuild a `done` record from `meta.toml`
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

`sample_bytes` records what the rate was measured on, since a rate taken on a 240 KB file and one taken on a 2 MB file are not the same evidence. `tool_version` is the version string of the backend that ran, because a rate is evidence about a build, `docs/tools.md` §5.1.

`retype bench` writes this file. It generates one sample per family in the format `[bench.seed]` names, using an argv `[bench.sample]` declares, and then measures every target that compiles from that source. `--pair` and `--family` narrow it. Measuring the same pair twice replaces the earlier answer rather than leaving two for one question.

Estimates use a measurement when the pair, backend and machine match, and a family constant otherwise. Every constant carries its provenance, with the measurement it came from.

## 8. Provenance

Mechanisms, guarantees and failure modes are specified in `docs/provenance.md`. This document owns only the field: the journal records the identity of every output at completion.

```json
"outputs":[{"path":"/home/u/Pictures/a.jpg","format":"jpg","bytes":812345,"tier":"converted",
            "identity":{"kind":"unix","dev":2049,"ino":5243121,"birth_ns":1690000000000}}]
```

```json
"identity":{"kind":"windows","volume_serial":"1A2B3C4D","file_id":"0000000000000000000000000001F4A3"}
```

`kind` is `unix` or `windows`. A record without `identity` cannot answer by identity, and `retype why` then falls back to path matching or reports unknown.
