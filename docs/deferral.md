# Deferral

When a job is parked instead of run, what it waits for, and what ends the wait.

## 1. Two parked states

A parked job has written nothing: no tool started, no output staged, no source bytes
removed. It holds the request it was given, the plan for it, what it is expected to
cost, and the reason it is waiting.

| State | Meaning | Ends by |
|---|---|---|
| `pending` | The job can run as asked, once the timer expires or the user says so | The deadline, `jobs resolve --now`, `jobs cancel`, or the source being renamed or removed |
| `needs-attention` | The job cannot run as asked, and only the user can decide what to do instead | `jobs resolve --as-is`, `jobs cancel`, or the source being renamed or removed |

Both are journal states, `docs/state.md` §3. Neither has a deadline when it is
`needs-attention`: there is nothing to expire into.

## 2. The reason

One reason per parked job, recorded with it and shown by `jobs get` and
`jobs list`.

| Reason | State | Meaning |
|---|---|---|
| `ask` | `pending` | The name carried `+ask`, so the user asked to be asked |
| `cost` | `pending` | The estimate is above the threshold that applies to this job, §4 |
| `intent` | `needs-attention` | The intent in the new name does not parse |
| `inapplicable` | `needs-attention` | An operation does not apply to the target format, `E_INAPPLICABLE` |
| `backend` | `needs-attention` | No usable backend carries the pair, `E_UNSUPPORTED` or `E_TOOL_MISSING` |

`intent`, `inapplicable` and `backend` keep the diagnosis: code, message and span
where there is one. `--as-is` means "carry what can be carried", §7.

## 3. Triggers, in order

The first that applies decides. A later trigger never overrides an earlier one, so a
string that cannot be parsed is never quietly run because it happened to be cheap.

1. The job cannot run as asked → `needs-attention`
2. The intent carries `ask` → `pending`, reason `ask`
3. The job is derived from a watched rename, no undo record can be kept for it, and
   the estimate exceeds `above_without_undo` → `pending`, reason `cost`
4. The job is derived from a watched rename and the estimate exceeds `above` →
   `pending`, reason `cost`
5. Otherwise the job runs

Two rules follow from the list rather than from a special case:

- An explicit `retype convert` is never parked by cost. The command is the consent,
  and deferring it would be answering a question with a question. It still parks on
  the three `needs-attention` reasons, and it still parks when the intent says `ask`
- "No undo record can be kept" is decided by `docs/state.md` §5.5 before the job is
  parked: `[undo] enabled = false`, `on_overflow = skip` and a full quota, or a source
  larger than the whole quota. This is the "destructive target" of:
  consuming a source with no way back, and it is why the threshold is lower in that
 case 

## 4. Thresholds

`[deferral]`, host-level, `docs/config.md` §9.

| Key | Default | Meaning |
|---|---|---|
| `mode` | `ask` | `ask` defers by §3; `now` runs immediately and parks only for `needs-attention` |
| `above` | `10s` | Estimated duration above which a watched rename is deferred |
| `above_without_undo` | `2s` | The same, for a job whose source cannot be kept |
| `pause` | `1h` | How long a `pending` job waits before it runs anyway |

Thresholds are the grammar's time class, so they are spelled the way an intent spells
a duration, `docs/grammar.md` §7. Both are compared against the whole job: the sum of
the per-output estimates, since one job's outputs run one after another.

An estimate that does not exist is not a reason to park. A family with no constant
and no measurement has an unknown cost, and turning "unknown" into "you must answer"
is worse than running. Deferral is a courtesy for expensive jobs, never a gate.

A directory file may set `confirm = false`, which is `mode = 'now'` for that
directory's renames only, `docs/config.md` §11. It cannot raise a threshold or change
a pause, because those describe the machine.

## 5. `now` and `ask` in the name

| Token | Effect |
|---|---|
| `+now` | Skips the `ask` and `cost` triggers for this job. `needs-attention` still parks, because it cannot run |
| `+ask` | Parks this job with reason `ask`, whatever the mode, the directory's `confirm` or the estimate says |

Both are one job's worth of override, and both print in the canonical form so the
journal records what was asked, `docs/grammar.md` §10.

## 6. The timer

A `pending` job carries a `deadline`, and expiry runs the job. Expiry is
unconditional: it does not depend on a notification, a client, a terminal or a
window.

- The deadline is `parked + pause`, stored in the journal record
- The daemon owns the timer. Nothing else has to be running for it to fire
- At startup, a `pending` job whose deadline has passed runs immediately, and one
  whose deadline is ahead has its timer re-armed, `docs/state.md` §6
- `pause = '0s'` is refused at load rather than read as "never" or "at once"

## 7. Acting on a parked job

```
retype jobs resolve <id|prefix> --now [--force]
retype jobs resolve <id|prefix> --as-is
retype jobs --interactive
retype jobs cancel <id|prefix>
```

| Command | Effect |
|---|---|
| `--now` | Run it now, ignoring the reason. `--force` overwrites an output that appeared during the pause, exactly as it does for `convert` |
| `--as-is` | Run it with what can be carried, dropping the tokens that could not be. Only for `needs-attention`; a `pending` job is a usage error, because there is nothing to drop |
| `--interactive` | Walk the parked jobs oldest first and read one key each: run, as-is where it applies, refuse, skip, quit. Without a terminal it refuses rather than reading a script's stdin |
| `jobs cancel` | Refuse it. Nothing was written, so nothing is undone |

All four accept a unique prefix, `docs/state.md` §2. A job that is no longer parked
answers `E_JOB_STATE`, so a second `--now` on the same id is an error rather than a
second conversion.

## 8. The file while it is parked

The source name is restored before the job is parked, `docs/state.md` §5.4. A path
never holds a format its name denies, and that rule does not bend because a decision
is pending.

The consequence is worth stating: rename `a.heic` to `a.jpg`, and the file shows
`a.heic` again until the job runs. The rename is not lost, it is remembered as the
job's requested target, and the journal says so.

- The requested target name and the intent are recorded with the job; the derived
  outputs are recomputed when it runs, so a name taken during the pause is numbered
  rather than failing the job
- A second rename of the same source supersedes the parked job: the chain
  continues and the park is replaced rather than run later against a file that has
  moved on
- A source that is gone when the timer fires ends the job `cancelled` with
  `E_IO_NOT_FOUND` and the message "the source is gone". Nothing was done and
  nothing is left to do
- A source whose bytes changed during the pause is converted as it is then, not as it
  was. The job converts a file, not a version of one

## 9. Batches

Jobs parked in one daemon tick share a `batch` ULID and are answered together: one
notification, one prompt page, one line per job in the summary. The batch is recorded
with the journal entry so that a later reader can tell which jobs were decided
together.

How a notification is delivered, and what its buttons do, is and
not part of this document. Deferral is complete without it: the timer fires, and
`jobs --interactive` is the path that needs a person.

