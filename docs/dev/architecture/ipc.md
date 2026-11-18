# Daemon and client protocol

Contract between `retype-daemon` and its clients. Protocol identifier: `retype-ipc-1`.

## 1. Process model

- One daemon per user session. Never a system service, never privileged.
- The daemon is the only writer of the journal and the undo store. It owns the watch registry whenever it is reachable. Default `watch` starts or reaches the daemon and follows readiness/activity; explicit `watch --register` may write the registry when no daemon answers and returns without starting one. `retype bench` writes the calibration atomically, and the daemon reads it for the next estimate
- `retype daemon` runs in the foreground, which is what a systemd user unit or a launchd agent executes
- A second daemon exits with `another daemon owns <state dir>`. Single instance is enforced by the lock file, never by convention
- The CLI and the GUI are equal clients. No method is available to one and not the other

## 2. Starting and stopping

Clients start the daemon when they need it. Auto-start is the default because a half-running installation is a bug report waiting to happen, and because two writers is the failure it prevents.

Client start sequence:

1. Try to connect
2. On failure, spawn `retype daemon --detached` and retry connecting with backoff for up to 3 s; replace an exited startup child within that same deadline
3. If the socket still does not answer, report `E_IPC_UNAVAILABLE` with the spawn log path
4. Racing clients are harmless: all spawn candidates contend for the lock, one wins, the rest exit, and every client connects to the winner
5. The endpoint is published after the first registration round, or after two seconds if that round is stalled. `watch.list` and `status` separately report per-root watcher readiness; an endpoint does not imply a successful watcher or completed identity scan

Suppression:

| Control | Effect |
|---|---|
| `--no-spawn` on any command | Fail instead of starting a daemon |
| `RETYPE_NO_SPAWN=1` | Global suppression, for scripts and CI |
| `retype daemon stop` | Stops the daemon. Any later client that needs one starts it again |

Supervision:

- The daemon reads how it was started from the environment the supervisor sets
- The variables are inherited by every process in the session, so a daemon the CLI started itself clears them: its supervisor would never restart it
- On Unix, a spawned daemon has its own process group, so a foreground client's interrupt does not stop it
- Under a supervisor, `retype daemon stop` refuses with `E_IPC_SUPERVISED`, telling the user to run the supervisor's stop command, because stopping a supervised daemon is immediately undone by the supervisor
- `--force` overrides the refusal for the case where the unit is being removed
- `retype daemon start` exists for tests, units and scripts that want an explicit start without the foreground

Stop behaviour:

| Trigger | Behaviour |
|---|---|
| `stop` | Refuse new jobs, finish in-flight conversions, flush and fsync the journal, exit 0 |
| `stop --now` | Cancel in-flight jobs, discard partial outputs, restore source extensions, flush, exit 0 |
| `SIGTERM`, `SIGINT` | Draining stop. A second signal escalates to `--now` |
| Client disconnect | Nothing. Clients are disposable |

## 3. Registrations

| Method | Behaviour |
|---|---|
| `watch.add` | Canonicalise the path, reject a missing path, add or update. Idempotent |
| `watch.remove` | Remove by path. Absent is a success |
| `watch.list` | Registrations, directory policy and installed-watcher state, plus any registration-read error |
| Overlap | Registrations are independent; a path inside a registered root is accepted and supplies only its own directory configuration |
| Persistence | The registry is a file, so registrations survive daemon restart and reboot |

Directory configuration is not re-read only on registration. Edits take effect without re-registering. A watch is `ready` only after its filesystem watcher starts and the initial identity scan finishes without an observed error; `pending`, `starting`, `stopping`, `paused`, `unavailable`, `degraded` and `error` are distinct states. An unreadable registration file is reported and the supervisor stops its watchers until it can read the file again.

## 4. Transport

| Platform | Endpoint | Authentication |
|---|---|---|
| Linux | `<state>/daemon.sock` | Directory `0700`; `SO_PEERCRED`, peer uid equals the daemon uid |
| macOS, BSD | `<state>/daemon.sock` | Directory `0700`; `LOCAL_PEERCRED` |
| Windows | `\\.\pipe\retype-<machine>-<user>` | Default DACL, owner only; impersonation must succeed |

A Windows pipe can only be impersonated after its client has written, so the first frame is read before the check and handed to the connection loop rather than read twice. A client that connects and sends nothing holds the daemon's single connection slot until it writes or goes away. A client encountering a busy pipe waits up to three seconds for a connection slot, including the gap between instances; it does not retry a request that was sent or start another daemon when the slot remains busy. An initially missing pipe receives a 150 ms grace for instance turnover before it is treated as absent.

Windows also hands a child every inheritable handle in its parent, and the pipes a caller uses to read the CLI's output are inheritable by construction. The CLI clears that flag on its own standard handles before it spawns a daemon, so a daemon cannot keep a caller's pipe open after the command exits. The daemon is detached and has no console, and every tool it runs is a console program, so each spawn carries `CREATE_NO_WINDOW`: without it Windows would give every conversion a console window of its own.

`<user>` is a short stable per-user token, derived from the user SID where one exists. Windows is usually single-user, and the suffix costs nothing while removing a class of surprises on machines that are not.

Socket paths live in the state directory, with no runtime-directory fallback. Unix socket paths are length-limited (about 104 bytes), so an over-long path is a startup error with a remedy rather than a mystery: the message names the limit and points at `RETYPE_STATE_DIR`.

No TCP listener exists in any configuration.

## 5. Framing

Newline-delimited JSON, UTF-8, one object per line, maximum frame 1 MiB. A frame that exceeds the limit closes the connection.

Handshake, required before any other method:

```json
{"id":1,"method":"hello","params":{"ipc":"retype-ipc-1","client":"retype-cli/0.1.0-alpha","features":["events"]}}
{"id":1,"ok":true,"result":{"ipc":"retype-ipc-1","version":"0.1.0-alpha","pid":41233,
 "machine":"7f3c1a2b","uptime_ms":91000,"features":["events","undo.list"],
 "capabilities":{"backends":["magick"]}}}
```

A different major receives `E_IPC_VERSION` and the connection closes. New fields and new methods do not change the major. A client that does not send `hello` first receives `E_IPC_HANDSHAKE`.

The request timeout defaults to 5 s and covers both writing the request and reading the response. A timed-out connection is discarded; failure to stop its I/O is reported. The client timeout does not imply that a request already received by the daemon was cancelled.

## 6. Methods

| Group | Methods |
|---|---|
| Session | `hello`, `status`, `shutdown` |
| Registry | `watch.add`, `watch.remove`, `watch.list` |
| Jobs | `jobs.list`, `jobs.get`, `jobs.submit`, `jobs.cancel`, `jobs.resume`, `jobs.resolve` |
| History | `undo.list`, `why` |
| Recovery | `recovery.undo`, `recovery.get`, `recovery.list`, `recovery.cancel`, `recovery.cleanup` |
| Live | `events.subscribe` |
| Config | `config.reload` |

Daemons advertise `recovery`. `recovery.undo` accepts the existing undo selector and an optional client-chosen full `operation_id`, records the action before execution and returns a recovery operation. Clients poll `recovery.get` on fresh connections. Final results/errors are persisted under `<state>/recovery/` and remain after successful undo removes its source record. The legacy blocking `undo` method is refused with `E_IPC_VERSION`; current typed clients negotiate `recovery` and expose the same synchronous CLI experience. Offline undo holds the daemon's state lock for the entire operation.

`recovery.cancel` signals a safe stop without waiting for hashing, copying or an already authorized publication. `recovery.cleanup` takes a full job id, requires job ownership, and releases only proven redundant recovery. `recovery.list` exposes operation results and protected records, including incomplete reservation evidence. Queued/running operations found after restart become failed/interrupted; file transactions remain pinned for checked retry rather than being replayed blindly. Recovery has two workers and a bounded admission queue independent of conversions.

`shutdown` reads `force` and `now`. Without `force`, a daemon a supervisor started refuses with `E_IPC_SUPERVISED` and names the supervisor, §2. With `now`, cancellation requests and queued cancellation snapshots are issued promptly, recovery receives cancellation, and the answer carries how many conversions were stopped. Worker joins and final durable recovery outcomes happen after the reply; an already authorized filesystem transition finishes or leaves explicit pinned recovery.

`status` reports undo `retained`, `reserved`, `goal` and `overage` source bytes. Unreadable recovery state is exposed as `undo_error` without ending the daemon connection. Durable pinned reservation manifests count after restart. Storage parks retain the verified input fingerprint and have no deadline; capacity permits a fresh admission check without discarding protected recovery.

`status` reports `supervised`: `systemd`, `launchd`, or null, read from the environment of the process the supervisor actually runs rather than from the process tree. A daemon the CLI started itself has had those variables cleared, so it reports null.

`jobs.submit` takes `mode`:

| Mode | Use | Admission |
|---|---|---|
| `immediate` | `retype convert` | Uses the bounded queue with priority over waiting renames |
| `queued` | A rename | The watcher restores the source name and records the request before queueing it |

`jobs.submit` reads absolute `input` and optional `output` paths, `intent` and `force`. Daemons advertising `jobs.submit.id` also accept a full client-chosen `job_id`. The typed client negotiates this feature, chooses the handle before sending, and includes it in structured errors if an acknowledgement is lost. The daemon refuses reuse of an existing handle. A caller must inspect that job before retrying; a missing acknowledgement is not cancellation. `watch.add` and `watch.remove` also require absolute paths. The typed client resolves relative paths against its own working directory before sending; raw relative paths are rejected rather than interpreted relative to the daemon. Queue capacity is checked before a queued snapshot is appended and synced; a worker cannot take it before that succeeds. Ordinary watched renames, automatic repairs and timed or confirmed parked requests use that ordering. Parked admission, cancellation and supersession are serialized. The initial reply is a durable request receipt, not content-bound acceptance. It records a cheap `admission` stamp containing file identity, size, modification and platform change metadata. The worker checks the stamp, captures a cancellable private snapshot, verifies its digest against the live source, and durably publishes `source.fingerprint` before tool probes. This publication is content-bound acceptance. `retype-client::jobs::submit` waits for it using fresh connections; `Client::jobs_submit` exposes the earlier request handle for status and cancellation during capture. Source I/O never holds the serial connection slot or queue mutex. Metadata alone cannot establish content identity when timestamps coalesce. Changes before capture may prevent acceptance; accepted work is bound to the captured digest, and subsequent live-source changes are checked against it before execution. Watched queues store `source.fingerprint`; parked jobs retain `parked.input_fingerprint`. The worker checks the recorded digest against the private working snapshot before conversion. A CLI explicit ask is prepared by a worker before becoming `needs-attention`, so `jobs.submit` initially returns `queued` even for an ask. Job waiters must stop polling on attention and surface that decision. A full or closed queue rejects an ordinary request without writing a queued job, and leaves a timed job parked. If recording fails, the job does not start and its id is reported: a journal write may have survived despite the error, so its outcome after restart is uncertain and must be checked before retrying. Platform durability limitations still apply. A submission that needs its result polls `jobs.get`, which answers with the whole record: the outputs, their bytes, their tier and their errors.

Ordinary watched and immediate workers also check that the running snapshot was recorded before conversion starts. A failed checkpoint leaves the input untouched by the worker and reports a non-durable failure while the daemon is running. Resume still uses a separate path.

After a daemon restart queued jobs, including those admitted from parked requests, are marked interrupted for manual review and resubmission rather than automatically replayed. The interruption reports whether the recorded input still matches, changed, is unreadable or has no legacy fingerprint. A parked record without a fingerprint also requires review and resubmission; it does not time out into execution. CLI input fingerprinting and probes run in the bounded worker pool. Probes use contained processes with a three-second deadline. Interrupted preparation records without a digest require explicit review; identity metadata is not substituted for a content fingerprint. Watcher-side decision hashing remains on that watcher's path. Undo, queued/settled cancellation verification and startup reconciliation use background recovery workers; startup publishes an endpoint without first hashing every interrupted file.

A running cancellation finishes recovery in its worker after the cancellation snapshot is durable. With completed outputs, it verifies and starts retention. With no outputs, it may release a proven redundant copy after checked restoration or verification of the original; no output is removed. Legacy evidence gaps, changed sources, publication ambiguity and unsupported directory durability retain recovery and report a warning. Queued/settled cancellation and startup reconciliation use the separate recovery lane and the same checks. Undo hashes/copies outside the global accounting lock while holding its per-job lease; only accounting and manifest mutations take the global lock.

`jobs.resume` admits a partial or interrupted job to the bounded worker queue and returns its id with state `queued`. Duplicate admission is refused. The worker verifies retained bytes and skips outputs that still match their recorded identity and digest; unavailable recovery reports `E_JOB_RESUME_UNAVAILABLE` in the job. Resume shares ordinary cancellation and shutdown. A completed resume applies the recorded source disposition only when every output has a validated publication record. `jobs.get` exposes the resulting disposition and any reason the original was retained or requires review.

`jobs.get`, `undo` and `jobs.resume` accept a full ULID or a unique prefix. A prefix matching two jobs is `E_JOB_AMBIGUOUS` with the candidates; a prefix shorter than four characters is `E_USAGE_ARGS`.

`jobs.list` returns a summary per job, not the full record: `id`, `state`, `source`, `created`, `updated`, `intent`, `parked_reason`, `error`, `outputs_done` and `outputs_total`. Requests accept `offset` and an optional page `limit`, capped at 256. Responses stay below the frame limit and include `next_offset` when another page remains. Clients repeat the request with that offset; the typed client's `jobs_list` reads all pages in the existing order. CLI filters and total limits remain local so displayed handles stay unambiguous across the full history. `jobs.get` remains the full journal record.

`undo.list` returns `{"records": [...]}` with `id`, `intent`, `created`, `source_path`, `source_ext`, `source_size`, `outputs`, `pinned`, `consume_source`, `reserved` and a protection reason. `status` also reports `recovery_active` and the current verified session policy. Session-query failure is explicitly inactive and does not authorize automatic watching.

`config.reload` answers `{"reloaded": true, "warnings": [...]}`. A warning is the same diagnostic shape an error carries, with its code, message and optional span and hint.

## 7. Events

```json
{"event":"job","seq":91,"job":{...}}
```

```
events {"since": 90}  ->  {"events": [...], "seq": 91}
```

- `seq` is monotonic per daemon lifetime, shared across connections, and starts at one. `since` is the last sequence the client saw. `since = 0` requests history from the beginning and can require resync after eviction. Asking ahead with `u64::MAX` returns the current cursor without history; clients use that cursor before refreshing authoritative jobs
- The last 1024 events are retained in memory, one per job snapshot, in the order the journal recorded them
- Batches stay below the frame limit; `seq` is the last sequence returned, or the daemon's newest sequence when no event follows `since`. Clients continue from that sequence until caught up
- A `since` older than the retention window cannot be filled: the call fails with `E_IPC_RESYNC` and the client reloads with `jobs.list`. A `since` ahead of the daemon is not an error, it is an empty answer
- Events are read by polling `since`, not by holding a connection open. One connection at a time is therefore still enough, and a client that would rather stream asks again as soon as an answer arrives
- Events are advisory. State is authoritative only in `jobs.list` and `jobs.get`

## 8. Errors

```json
{"id":7,"ok":false,"error":{"code":"E_JOB_STATE","message":"job 01j8z is done","detail":{"state":"done"}}}
```

One error type in `retype-core`, shared by daemon, CLI and GUI. IPC codes are namespaced `E_IPC_*`; domain codes keep their specification names. An error carries `code`, `message`, optional `detail`, and, where the producer knows them, `span` and `hint`. The category is derived from the code, so a client never trusts a category that disagrees with it.

IPC codes: `E_IPC_VERSION`, `E_IPC_HANDSHAKE`, `E_IPC_UNAVAILABLE`, `E_IPC_SUPERVISED`, `E_IPC_AMBIGUOUS`, `E_IPC_RESYNC`, `E_IPC_FRAME`, `E_IPC_METHOD`, `E_IPC_AUTH`, `E_IPC_INTERNAL`.

## 8.1 Runtime lifecycle ownership

Runtime lifecycle representations do not change request or response payloads.

- The client connection is ready with an owned stream, unavailable, or timed out; request exchange temporarily transfers the stream to its worker, and a timed-out connection is never reused
- The queue is accepting or draining; accepted work becomes `QueuedWork` only after durable admission under the queue lock, then transfers to `RunningWork` with its cancellation registration
- A parked admission owns the current job snapshot and its admission lock until queueing succeeds, fails or becomes stale; waiting conditions distinguish deadlines, confirmation and review
- Daemon lifecycle is serving, draining, cancelling or stopped; stopping closes admission, draining may escalate to cancellation, and stopped is recorded after thread cleanup and endpoint removal
- Watcher supervisor entries are running workers or delayed retries; worker stop consumes and joins its thread, and each generation retains its own health handle
- Held directory batches always contain a deadline, restarted by each new fact

The existing `stopped()` accessor means stop has been requested, including draining and cancelling. Running-work drop removes its in-flight registration; it does not fabricate a terminal journal snapshot. Cancellation, recovery and publication checks remain required after ownership transfers.

## 9. Client obligations

- Send `hello` first, and close on a version mismatch rather than guessing
- Treat events as hints and reconcile with `jobs.list` on connect and on resync
- Never write the journal, the undo store or the watch registry directly while a daemon is reachable
- Use the same methods the GUI uses. A client-specific back door is a protocol bug
