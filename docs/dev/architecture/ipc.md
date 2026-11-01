# Daemon and client protocol

Contract between `retype-daemon` and its clients. Protocol identifier: `retype-ipc-1`.

## 1. Process model

- One daemon per user session. Never a system service, never privileged.
- The daemon is the only writer of the journal and the undo store. It owns the watch registry whenever it is reachable; `retype watch` writes the file only when no daemon answers, because the command does not start one. `retype bench` writes the calibration atomically, and the daemon reads it for the next estimate
- `retype daemon` runs in the foreground, which is what a systemd user unit or a launchd agent executes
- A second daemon exits with `another daemon owns <state dir>`. Single instance is enforced by the lock file, never by convention
- The CLI and the GUI are equal clients. No method is available to one and not the other

## 2. Starting and stopping

Clients start the daemon when they need it. Auto-start is the default because a half-running installation is a bug report waiting to happen, and because two writers is the failure it prevents.

Client start sequence:

1. Try to connect
2. On failure, spawn `retype daemon --detached` and retry with backoff for up to 3 s
3. If the socket still does not answer, report `E_IPC_UNAVAILABLE` with the spawn log path
4. Racing clients are harmless: all spawn candidates contend for the lock, one wins, the rest exit, and every client connects to the winner

Suppression:

| Control | Effect |
|---|---|
| `--no-spawn` on any command | Fail instead of starting a daemon |
| `RETYPE_NO_SPAWN=1` | Global suppression, for scripts and CI |
| `retype daemon stop` | Stops the daemon. Any later client that needs one starts it again |

Supervision:

- The daemon reads how it was started from the environment the supervisor sets
- The variables are inherited by every process in the session, so a daemon the CLI started itself clears them: its supervisor would never restart it
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
| `watch.list` | Current registrations with their directory configuration summary |
| Overlap | Registrations are independent; a path inside a registered root is accepted and supplies only its own directory configuration |
| Persistence | The registry is a file, so registrations survive daemon restart and reboot |

Directory configuration is not re-read only on registration. The daemon watches every `.retype.toml` under a registered root and the global configuration file, so editing either takes effect without re-registering.

## 4. Transport

| Platform | Endpoint | Authentication |
|---|---|---|
| Linux | `<state>/daemon.sock` | Directory `0700`; `SO_PEERCRED`, peer uid equals the daemon uid |
| macOS, BSD | `<state>/daemon.sock` | Directory `0700`; `LOCAL_PEERCRED` |
| Windows | `\\.\pipe\retype-<machine>-<user>` | Default DACL, owner only; impersonation must succeed |

A Windows pipe can only be impersonated after its client has written, so the first frame is read before the check and handed to the connection loop rather than read twice. A client that connects and sends nothing therefore holds the daemon's single connection slot until it writes or goes away, which is a self-inflicted stall: the DACL admits only the user whose daemon it is.

`<user>` is a short stable per-user token, derived from the user SID where one exists. Windows is usually single-user, and the suffix costs nothing while removing a class of surprises on machines that are not.

Socket paths live in the state directory, with no runtime-directory fallback. Unix socket paths are length-limited (about 104 bytes), so an over-long path is a startup error with a remedy rather than a mystery: the message names the limit and points at `RETYPE_STATE_DIR`.

No TCP listener exists in any configuration.

## 5. Framing

Newline-delimited JSON, UTF-8, one object per line, maximum frame 1 MiB. A frame that exceeds the limit closes the connection.

Handshake, required before any other method:

```json
{"id":1,"method":"hello","params":{"ipc":"retype-ipc-1","client":"retype-cli/0.1.0","features":["events"]}}
{"id":1,"ok":true,"result":{"ipc":"retype-ipc-1","pid":41233,"machine":"7f3c1a2b","uptime_ms":91000,
 "features":["events","notify"],"capabilities":{"backends":["magick"]}}}
```

A different major receives `E_IPC_VERSION` and the connection closes. New fields and new methods do not change the major. A client that does not send `hello` first receives `E_IPC_HANDSHAKE`.

Request timeout defaults to 5 s. No method blocks on work that can outlive it.

## 6. Methods

| Group | Methods |
|---|---|
| Session | `hello`, `status`, `shutdown` |
| Registry | `watch.add`, `watch.remove`, `watch.list` |
| Jobs | `jobs.list`, `jobs.get`, `jobs.submit`, `jobs.cancel`, `jobs.resume`, `jobs.resolve` |
| History | `undo`, `why` |
| Live | `events.subscribe` |
| Config | `config.reload` |

`shutdown` reads `force` and `now`. Without `force`, a daemon a supervisor started refuses with `E_IPC_SUPERVISED` and names the supervisor, §2. With `now`, running jobs are cancelled and waiting ones are recorded as cancelled, the runners are given a moment to write those records, and the answer carries how many were stopped.

`status` reports `supervised`: `systemd`, `launchd`, or null, read from the environment of the process the supervisor actually runs rather than from the process tree. A daemon the CLI started itself has had those variables cleared, so it reports null.

`jobs.submit` takes `mode`:

| Mode | Use | Admission |
|---|---|---|
| `immediate` | `retype convert` | Bypasses the queue and starts now, still cancellable and still journalled |
| `queued` | A rename | Normal queue admission, bounded parallelism, interactive priority |

`jobs.submit` reads `input`, `intent`, an optional `output` and `force`, and answers with the job id once the job is planned and journalled. Everything that can fail before a byte moves fails here, so a caller gets a diagnosis rather than a job that will only fail later. A submission that needs its result polls `jobs.get`, which answers with the whole record: the outputs, their bytes, their tier and their errors.

`jobs.resume` continues a `partial` job, skipping outputs that still match their recorded identity, and reports `E_JOB_RESUME_UNAVAILABLE` when neither the source path nor the undo record holds the source bytes.

`jobs.get`, `undo` and `jobs.resume` accept a full ULID or a unique prefix. A prefix matching two jobs is `E_JOB_AMBIGUOUS` with the candidates; a prefix shorter than four characters is `E_USAGE_ARGS`.

## 7. Events

```json
{"event":"job","seq":91,"job":{...}}
```

```
events {"since": 90}  ->  {"events": [...], "seq": 91}
```

- `seq` is monotonic per daemon lifetime, shared across connections, and starts at one. `since` is the last sequence the client saw, so `since = 0` asks for everything retained
- The last 1024 events are retained in memory, one per job snapshot, in the order the journal recorded them
- The answer carries the newest sequence, so a client knows where it now stands without a second call
- A `since` older than the retention window cannot be filled: the call fails with `E_IPC_RESYNC` and the client reloads with `jobs.list`. A `since` ahead of the daemon is not an error, it is an empty answer
- Events are read by polling `since`, not by holding a connection open. One connection at a time is therefore still enough, and a client that would rather stream asks again as soon as an answer arrives
- Events are advisory. State is authoritative only in `jobs.list` and `jobs.get`

## 8. Errors

```json
{"id":7,"ok":false,"error":{"code":"E_JOB_STATE","message":"job 01j8z is done","detail":{"state":"done"}}}
```

One error type in `retype-core`, shared by daemon, CLI and GUI. IPC codes are namespaced `E_IPC_*`; domain codes keep their specification names.

IPC codes: `E_IPC_VERSION`, `E_IPC_HANDSHAKE`, `E_IPC_UNAVAILABLE`, `E_IPC_SUPERVISED`, `E_IPC_AMBIGUOUS`, `E_IPC_RESYNC`, `E_IPC_FRAME`, `E_IPC_METHOD`, `E_IPC_AUTH`, `E_IPC_INTERNAL`.

## 9. Client obligations

- Send `hello` first, and close on a version mismatch rather than guessing
- Treat events as hints and reconcile with `jobs.list` on connect and on resync
- Never write the journal, the undo store or the watch registry directly while a daemon is reachable
- Use the same methods the GUI uses. A client-specific back door is a protocol bug
