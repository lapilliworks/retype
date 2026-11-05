# Errors

One error type, one set of codes, one mapping to every surface.

## 1. Layers

| Layer | Produces | Examples |
|---|---|---|
| Invocation | Usage errors | Missing argument, unknown flag, conflicting flags |
| Grammar | Diagnostics with spans | `E_ANCHOR`, `E_INAPPLICABLE` |
| Plan | Registry and capability errors | No backend implements the plan, tier unavailable |
| Execute | Backend errors | Tool failed, encoder refused, output unreadable |
| Persist | Filesystem and state errors | Permission denied, no space, lock held, journal corrupt |
| Transport | IPC errors | Version mismatch, daemon unavailable, ambiguous prefix |
| Internal | Bugs | Invariant violations, panics caught at the boundary |

## 2. Anatomy

Every error carries:

- `code`, stable and never reused
- `category`, which decides the exit status and the recovery action
- `message`, one sentence naming the thing and what to do next
- `hint`, when a remedy exists
- `detail`, machine-readable context for `--json`
- `span`, for grammar diagnostics, covering the offending part of the intent
- `report`, for an internal failure, which is what `retype doctor --report` bundles

## 3. Codes

Grammar diagnostics keep their unprefixed names, fixed by `retype-grammar-1`: `E_ANCHOR`, `E_CHARSET`, `E_INTENT_EMPTY`, `E_TOO_LONG`, `E_TOO_MANY`, `E_GROUP_EMPTY`, `E_TOKEN_EMPTY`, `E_FORMAT_UNKNOWN`, `E_FORMAT_FOLDER`, `E_NAME_UNKNOWN`, `E_VALUE_MISSING`, `E_VALUE_UNEXPECTED`, `E_VALUE_SYNTAX`, `E_VALUE_RANGE`, `E_DUPLICATE`, `E_EXCLUSIVE`, `E_META_CONFLICT`, `E_NOOP`, `E_INAPPLICABLE`, `E_UNSUPPORTED`. `E_META_CONFLICT` is the one grammar code that can be a warning rather than a diagnosis, `docs/reference/intents.md` §6.

A client that receives a code from a later version reports it rather than substituting a nearer one.

Everything else is namespaced, and the table lists every code in full so that a search finds it.

| Group | Codes |
|---|---|
| Invocation | `E_USAGE_ARGS`, `E_USAGE_CONFLICT`, `E_USAGE_OUTPUT_COUNT` |
| Configuration | `E_CFG_PARSE`, `E_CFG_VERSION`, `E_CFG_VALIDATION`, `E_CFG_MISSING`, `E_CFG_UNKNOWN_KEY`, `E_CFG_NAME_CLASH`, `E_CFG_SHADOWED` |
| Watches | `E_WATCH_PAUSED_BY_POLICY` |
| Tools | `E_TOOL_MISSING`, `E_TOOL_VERSION`, `E_TOOL_FAILED`, `E_TOOL_TIMEOUT` |
| Filesystem and state | `E_IO_FAILED`, `E_IO_PERMISSION`, `E_IO_SPACE`, `E_IO_NOT_FOUND`, `E_IO_EXISTS`, `E_IO_LOCKED`, `E_IO_CORRUPT_JOURNAL` |
| Jobs | `E_JOB_STATE`, `E_JOB_CANCELLED`, `E_JOB_INTERRUPTED`, `E_JOB_PARTIAL`, `E_JOB_SUPERSEDED`, `E_JOB_RESUME_UNAVAILABLE`, `E_JOB_NOT_FOUND`, `E_JOB_AMBIGUOUS` |
| Transport | `E_IPC_VERSION`, `E_IPC_HANDSHAKE`, `E_IPC_FRAME`, `E_IPC_METHOD`, `E_IPC_UNAVAILABLE`, `E_IPC_AMBIGUOUS`, `E_IPC_RESYNC`, `E_IPC_SUPERVISED`, `E_IPC_AUTH`, `E_IPC_INTERNAL` |
| Bugs | `E_INTERNAL_INVARIANT`, `E_INTERNAL_PANIC`, `E_INTERNAL_UNEXPECTED` |

[Daemon and client protocol](../dev/architecture/ipc.md) §8 defines the transport codes; they are listed here as well so that one table answers "what can this build emit". `E_CFG_SHADOWED` is the one configuration code that is a warning rather than a failure: a user preset replacing a shipped one is allowed and reported.

## 4. Categories

| Category | Meaning | Exit | Retryable | Notification action | Journal state |
|---|---|---|---|---|---|
| `usage` | The invocation itself is malformed | 2 | No | Correct | none |
| `diagnosis` | The request cannot be satisfied as written | 3 | No | Tweak | `needs-attention` |
| `environment` | This machine cannot satisfy it | 4 | After a fix | Open doctor | `failed` |
| `transient` | Try again | 5 | Yes | Retry | `failed` |
| `daemon` | The daemon cannot be reached | 6 | Yes | Start retype | none |
| `state` | The job or the store is in a state that refuses this | 7 | No | Open jobs | unchanged |
| `internal` | A bug | 1 | No | Report | `failed`, with a report id |

Exit codes are the ones in `docs/reference/cli.md` §6. The category is text on the wire and the exit code is derived from it, so a script can branch on either without parsing messages. A code belongs to exactly one category; `docs/reference/cli.md` §6 lists the codes behind each number.

Rules:

- A usage error never reaches the domain; it is refused before a job exists
- A diagnosis never retries on its own. Retrying an invalid intent produces the same invalid intent
- A transient failure is retried at most once, with backoff, and then reported
- An environment failure is not retried automatically. It needs a human to install something
- A state failure is not retried; the job or the lock needs a decision first
- The daemon being unreachable is retried by the next invocation rather than by the one that failed
- An internal failure is not retried, always carries a report id, and is logged at error level

## 5. Partial completion and resume

Outputs are independent units of work. A job does not roll back, because re-running a multi-minute conversion because another output failed is worse for the user than continuing from where it stopped.

Rules:

- Each output has its own state: `pending`, `running`, `done`, `failed`, `skipped`
- A job whose outputs are mixed is `partial`
- A failed output leaves the others in place and is retried by `retype jobs resume`
- Resume skips outputs whose recorded identity and content digest still match; other occupied paths are refused without `--force`
- Resume reads a private working copy of the verified retained original, never an arbitrary replacement at the old source path
- Partial, failed, cancelled and superseded jobs retain originals and required recovery records
- A queued job interrupted by a restart is marked `E_JOB_INTERRUPTED` for manual review and resubmission; its journal entry does not yet include a content fingerprint that would make automatic replay safe
- Successful resume retains its backup for normal undo retention

Source naming, output publication and disposal follow `docs/dev/architecture/state.md` §5. Source restoration refuses occupied destinations. A backend exit status is not permission to delete the original: every requested output must be validated and durably committed, a verified backup and completion record must be durable, and the source must still be the object the job read. Unvalidated external backends, unavailable directory durability and backup failures retain originals.

## 6. Diagnostics

- Grammar diagnostics accumulate, up to 8, ordered by span offset
- The CLI prints the first diagnostic in full plus a count of the rest; `--json` prints all of them
- `retype explain` always prints all of them
- Suggestions appear as `hint`, generated from edit distance 2 against registered names

## 7. Surfaces

| Surface | What it shows |
|---|---|
| CLI stdout | Success lines only |
| CLI stderr | Errors, warnings, progress |
| `--json` | `{v, ok, error:{code, category, message}}`, plus a `diagnostics` array when the failure carries more than one; each diagnostic adds `span` and `hint` |
| Journal | `state` plus `error:{code, message}` |
| Notification | Category-specific action, one sentence |
| IPC | The same `code`, `category` and `detail` |
| GUI | Category decides between a dialog, an inline badge and a toast |

No surface invents a message. The daemon is the only place a message is composed, and every client renders what it was given.

## 8. Message rules

- Name the file or token that caused it: `"unsupported operation 'colors' for png"` and not `"invalid input"`
- State the remedy when one exists: `"install ImageMagick or drop '+quality80'"`
- No stack traces for `internal` unless `--verbose`; the report id is enough
- No trailing punctuation, no exclamation
- Never blame the user. "not supported" is a fact, "you asked for something impossible" is not
