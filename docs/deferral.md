# Deferral

When a job is parked instead of run, what it waits for, and what ends the wait.

## 1. Two parked states

A parked job has written nothing: no tool started, no output staged, no source bytes
removed. It holds the request it was given, the plan for it, what it is expected to
cost, and the reason it is waiting.

| State | Meaning | Ends by |
|---|---|---|
| `pending` | The job can run as asked, once the timer expires or the user says so | The deadline, `jobs resolve --now`, `jobs cancel`, or the source being renamed or removed |
| `needs-attention` | The job cannot run as asked, and only the user can decide what to do instead | `jobs resolve --convert`, `jobs cancel`, or the source being renamed or removed |

Both are journal states, `docs/state.md` §3. `needs-attention` has no deadline: there
is nothing to expire into.

## 2. The reason

One reason per parked job, recorded with it and shown by `jobs get` and `jobs list`.

| Reason | State | Meaning |
|---|---|---|
| `ask` | `pending` | The name carried `+ask`, so the user asked to be asked |
| `cost` | `pending` | The estimate is above the threshold that applies to this job, §4 |
| `intent` | `needs-attention` | The intent in the new name does not parse |
| `inapplicable` | `needs-attention` | An operation does not apply to the target format, `E_INAPPLICABLE` |
| `backend` | `needs-attention` | No usable backend carries the pair, `E_UNSUPPORTED` or `E_TOOL_MISSING` |

A `needs-attention` job keeps the diagnosis it was given: code, message, and the span
of the offending part where there is one. It also keeps two strings, and the report
of the job shows both:

- `requested`, the intent exactly as the new name carried it
- `corrected`, the same intent with the offending tokens removed, when the target
  format survives that, which is what `jobs resolve --convert` would run

Nothing is carried out on a guess. An intent that says `png+quality80` does not
produce a PNG at quality 80 and it does not produce a PNG at some other quality
either: the job stops, says which token could not be carried, and hands back the
string to correct. A conversion whose request was silently reduced is worse than no
conversion, because the user judges the result by what they asked for.

## 3. Triggers, in order

The first that applies decides. A later trigger never overrides an earlier one, so a
string that cannot be carried is never quietly run because it happened to be cheap.

1. The job cannot run as asked:
   - `[deferral] repair = 'convert'`, the job would keep an undo record, the estimate
     is at or below `above`, and the target format survives dropping the tokens that
     could not be carried → run it, record the dropped tokens, and notify with what
     happened, §6
   - otherwise → `needs-attention`
2. The intent carries `ask` → `pending`, reason `ask`
3. The job is derived from a watched rename, no undo record can be kept for it, the
   estimate exceeds `above_without_undo`, and neither `+now` nor `mode = 'now'`
   suppresses deferral → `pending`, reason `cost`
4. The job is derived from a watched rename, the estimate exceeds `above`, and none of
   `+now`, `mode = 'now'` or the directory's `confirm = false` suppresses deferral →
   `pending`, reason `cost`
5. Otherwise the job runs

Two rules follow from the list rather than from a special case:

- An explicit `retype convert` is never parked by cost. The command is the consent,
  and deferring it would answer a question with a question. It still parks on the
  `needs-attention` reasons, under the repair policy like everything else, and it
  still parks when the intent says `ask`
- "No undo record can be kept" is decided by `docs/state.md` §5.5 before the job is
  parked: `[undo] enabled = false`, `on_overflow = skip` with a full quota, or a
  source larger than the whole quota. That is the "destructive target" of: consuming a source with no way back, and it is why the threshold is lower
 in that case 

## 4. Thresholds and the repair policy

`[deferral]`, host-level, `docs/config.md` §10.

| Key | Default | Meaning |
|---|---|---|
| `mode` | `ask` | `ask` defers by §3; `now` skips the two cost triggers, and still parks for `needs-attention` and for `+ask`, §5 |
| `above` | `10s` | Estimated duration above which a watched rename is deferred |
| `above_without_undo` | `2s` | The same, for a job whose source cannot be kept |
| `pause` | `1h` | How long a `pending` job waits before it runs anyway |
| `repair` | `ask` | What to do with a job that cannot run as asked: `ask` parks it, `convert` runs it without the tokens that could not be carried |

Durations are the grammar's time class, so they are spelled the way an intent spells
one, `docs/grammar.md` §7. Thresholds are compared against the whole job: the sum of
the per-output estimates, since one job's outputs run one after another.

`repair = 'convert'` is the convenience the user opts into. It is deliberately the
non-default, it never applies without an undo record, and it is bounded by `above`
rather than by a threshold of its own: a dropped token is a small cut on a job that
takes a second, and a guess that spends minutes on work nobody asked for is not.
Whatever it drops is recorded with the job and reported with the result, together with
the intent string the user meant.

An estimate that does not exist is not a reason to park. A family with no constant
and no measurement has an unknown cost, and turning "unknown" into "you must answer"
is worse than running. Deferral is a courtesy for expensive jobs, never a gate.

A directory file may set `confirm = false`, which suppresses deferral for that
directory's renames only, §5. It cannot raise a threshold, change a pause or turn on
repair, because those describe the machine and the user's tolerance for a guess.

## 5. Who decides

Three places can ask for a confirmation or suppress one, and they are not equal: one
of them was typed a moment ago about this file, and one may have arrived with a
directory from somewhere else.

| Statement | Written where | Effect |
|---|---|---|
| `+ask` | the new name, now | Parks the job with reason `ask`, whatever anything else says |
| `+now` | the new name, now | Skips both cost triggers for this job |
| `mode = 'now'` | the user config | Skips both cost triggers on this machine |
| `confirm = false` | the directory file | Skips the `above` trigger. The no-undo trigger of §3 still applies |
| `mode = 'ask'` | the user config | The default: defer by §3 |

Two rules produce the table:

- **An ask beats a suppression.** Asking is the safe direction: the cost of a prompt
  nobody wanted is one keystroke, and the cost of a conversion nobody saw is a file
  they have to notice and undo. So a `+ask` in the name parks the job even when the
  host, the directory and `now` mode all say not to
- **A directory file cannot remove the brake on an irreversible job.** A `.retype.toml`
  can arrive with a repository, and `confirm = false` in it would otherwise be enough
  to let a job delete its source without a record and without asking. It suppresses the
  ordinary cost trigger, which is a statement about a build directory; it does not
  suppress the threshold that exists because there is no way back. Only the machine's
  own setting and a `+now` typed by the person can do that

Both tokens print in the canonical form so the journal records what was asked,
`docs/grammar.md` §10, and neither changes what the job does: they decide whether the
user is asked first.

## 6. The report of a job that did not run as asked

A parked `needs-attention` job and a repaired job say the same four things, so that a
user can act on either without opening a log:

- The source and the requested target
- `requested`, the intent as written
- `corrected`, the intent that would run, when one exists
- What could not be carried, in the tool's own words, with the span of the token

A repaired job adds what it dropped and reaches the user through the same notification
as an ordinary job, with Undo available. The undo removes the
result, so a guess the user disagrees with costs one click.

## 7. The timer

A `pending` job carries a `deadline`, and expiry runs the job. Expiry is
unconditional: it does not depend on a notification, a client, a terminal or a window.

- The deadline is `parked + pause`, stored in the journal record
- The daemon owns the timer. Nothing else has to be running for it to fire
- At startup, a `pending` job whose deadline has passed runs immediately, and one
  whose deadline is ahead has its timer re-armed, `docs/state.md` §6
- `pause = '0s'` is refused at load rather than read as "never" or "at once"
- The timer is a chance to regret and to act first. It is not a promise that the job
  will be wanted: refusing it with `jobs cancel` before the deadline is the whole
  point of having one

## 8. Acting on a parked job

```
retype jobs resolve <id|prefix> --now [--force]
retype jobs resolve <id|prefix> --convert
retype jobs --interactive
retype jobs cancel <id|prefix>
```

| Command | Effect |
|---|---|
| `--now` | Run it as asked, ignoring the deferral. `--force` overwrites an output that appeared during the pause, exactly as it does for `convert`. A `needs-attention` job is a state error here: it cannot run as asked |
| `--convert` | Run it with the tokens that could not be carried dropped, from the `corrected` string. Only for `needs-attention`, and only when a corrected string exists; a `pending` job is a usage error, and so is a job whose target format did not survive |
| `--interactive` | Walk the parked jobs oldest first and read one key each: now, convert where it applies, refuse, skip, quit. Without a terminal it refuses rather than reading a script's stdin |
| `jobs cancel` | Refuse it. Nothing was written, so nothing is undone |

All four accept a unique prefix, `docs/state.md` §2. A job that is no longer parked
answers `E_JOB_STATE`, so a second `--now` on the same id is an error rather than a
second conversion.

## 9. The file while it is parked

The source name is restored before the job is parked, `docs/state.md` §5.4. A path
never holds a format its name denies, and that rule does not bend because a decision
is pending.

The consequence is worth stating: rename `a.heic` to `a.jpg`, and the file shows
`a.heic` again until the job runs. The rename is not lost, it is remembered as the
job's requested target, and the journal says so.

- The requested target name and the intent are recorded with the job, and the derived
  outputs are worked out again when it runs, because the request is read from the record
  rather than from a file name that has since changed
- An output whose name appeared during the pause is overwritten, exactly as a
  rename-derived job overwrites an output of the same name today. The undo record puts
  the source back; the file that was there is not recoverable, which is the risk a
  rename already carries and which deferral lengthens rather than introduces
- A second rename of the same source supersedes the parked job: the chain
  continues and the park is replaced rather than run later against a file that has
  moved on
- A source that is gone when the timer fires ends the job `cancelled` with
  `E_IO_NOT_FOUND` and the message "the source is gone". Nothing was done and there
  is nothing left to do
- A source whose bytes changed during the pause is converted as it is then, not as it
  was. The job converts a file, not a version of one, and matching a timestamp to
  decide otherwise would be a guess about what the user meant

## 10. Batches

Jobs parked in one daemon tick share a `batch` ULID and are answered together: one
notification, one prompt page, one line per job in the summary. The batch is recorded
with the journal entry so a later reader can tell which jobs were decided together.

How a notification is delivered, and what its buttons do, is and not
part of this document. Deferral is complete without it: the timer fires, and
`jobs --interactive` is the path that needs a person.

## 13. Not built yet

- `jobs --interactive`, the terminal path for a person with no notifications
- `jobs resolve --force`, which the `--now` form names for the case where an output has
  appeared during the pause
- Notification delivery, capability tiers and click routing
- `retype://job/<id>` actions beyond `resolve` and `cancel`
- The editor's view of a parked job

