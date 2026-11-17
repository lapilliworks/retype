# Jobs, undo and history

Every conversion is a job, whether it was typed or performed as a rename. The queue is where they are visible, reversible and sometimes stopped.

## The queue

```
retype jobs list
retype jobs list --state running
retype jobs list --state needs-attention
```

`--limit <n>` and `--since <id>` narrow a long list. A job is recorded when it starts, so `--state running` finds what can still be cancelled.

A conversion typed at the terminal is one job and does not queue behind a folder's renames. It shares the same pool, so parallelism is bounded the same way whether one person or a watcher started it.

`jobs get` answers with the whole record, as JSON: the outputs, their sizes, their tier, their errors. It is what a bug report or a script wants, so it is not rendered into columns. The tier is defined in [Fidelity](../advanced/fidelity.md).

Ids accept any unique prefix. A prefix that matches more than one job is refused, with the candidates listed, and [Command line](../reference/cli.md) has every subcommand and flag.

## Logs

```
retype jobs logs 01J8Z
retype jobs logs 01J8Z --follow
```

That prints what the converter itself printed. The full log is kept in the state directory and written as it arrives; the job record keeps only the last four kilobytes, because a converter can print megabytes and a failure needs the last ten lines.

`--follow` prints as it arrives and stops when the job does, rather than waiting to be interrupted.

## Stopping and continuing

```
retype jobs cancel 01J8Z
retype jobs resume 01J8Z
```

`cancel` requests that waiting or running work stop and can also cancel a settled partial job. A settled or queued cancellation is recorded promptly; any slow verification runs as a separate recovery operation. The CLI waits using short polls and shows the operation handle so it can be inspected after detaching. It leaves the original and completed outputs in place; it is not undo. When a cancelled job has completed outputs, the undo retention period starts after the live original, retained original and those outputs are verified. If verification fails or no undo record exists, cancellation reports a warning and any record stays protected. With no completed outputs, cancellation releases a recovery copy only after proving it redundant: the original is unchanged or safely restored, its metadata verifies, and no publication intent makes the outcome ambiguous. Changed originals, missing legacy evidence and unsupported directory durability keep recovery protected and report a warning. Job history remains available. Parked and queued cancellations are recorded before success is reported. A running cancellation is not durable until its terminal snapshot is recorded; after a crash, check the interrupted job before taking another action. If `daemon stop --now` cannot record a queued cancellation, the daemon log reports it.

`resume` is for a job that stopped part way, usually because a disk filled or a converter died. Outputs that are already finished and unchanged are kept, and the rest are run again from the verified undo copy. An output that has since been replaced by something else is refused rather than overwritten; `--force` replaces it anyway. A completed resume follows the original keep or removal request, subject to the same safety checks, and reports a retained or unresolved source. If work remains incomplete, the command reports the failure and exits nonzero; JSON includes the current job in the error detail.

## Undo

```
retype undo --last
retype undo 01J8Z
```

With a reachable daemon, undo is admitted to a separate bounded recovery lane and returns a durable operation handle internally. The CLI polls that operation without holding the daemon connection. Status, cancellation and shutdown remain available even while all conversion workers are busy. Recovery progress and completion survive removal of the undo record; an interrupted operation reports an explicit uncertain/interrupted result and retains recovery for review. Offline undo still holds the daemon's state lock.

Undo reverses one job: it removes unchanged outputs and restores the original if it was removed or is missing. It never overwrites an unrelated file. If the job explicitly kept a source and completed safely, undo leaves later edits to that source alone. If recovery is pinned because source safety is unresolved, a changed source blocks undo and the verified backup remains. Undo reads a store of source bytes, not the job history, so it still works after the history has aged out.

One record restores one job. Undoing the same job twice is a refusal, not a second restoration. If the daemon cannot be reached, offline undo acquires the state lock and refuses while another process owns it. On Windows, this build reports an error and retains outputs when directory durability is unavailable; it does not claim that undo completed.

| Setting | Default | Effect |
|---|---|---|
| `enabled` | on | Whether records are kept at all |
| `retention` | `1w` | How long a job stays reversible |
| `quota` | `5gb` | Soft storage goal, counted in source bytes held and reserved |
| `admission` | `stop-after` | Finish the job that crosses the goal, then hold new work; optional `stop-before` holds the crossing job |
| `on_overflow` | `ask` | Hold work at the goal; explicit `skip` chooses a retained-source conversion without a new record |

The settings are host-level in configuration; a folder cannot set them, so a repository cannot decide every contributor's disk budget. They are read from the `[undo]` table in [Configuration schema](../reference/config.md).

Unexpired records are not automatically evicted. By default, a job admitted below the goal keeps its backup even if it crosses the goal. Subsequent work waits visibly with reason `storage` until capacity becomes available. `stop-before` holds a job that would exceed the goal. A deliberate `on_overflow = 'skip'` keeps the original visible and omits the new record when it cannot fit; `ask` and legacy `evict` never authorize that downgrade. Interactive early-eviction prompts are not implemented. An error creating an input snapshot or undo record stops the job before conversion and is reported. A recognized storage-exhaustion error or a failed output recovery checkpoint stops the remaining outputs; completed outputs are kept. If the final state cannot be journalled, live status says it is not durable. External tools do not always provide a machine-readable cause, so an encoder's disk-full failure may be reported as a general tool failure.

Undo is also the button on the notification a watched folder produces, and it reverses every job of that batch that has a record. [Watching a folder](watch.md) describes the batching that makes it one notification.

## Recovery operations and protected reservations

```
retype recovery                         # operations and retained records
retype recovery <operation-id>          # state and durable result
retype recovery <operation-id> --cancel # request a safe stop
retype recovery <job-id> --cleanup      # verify and release redundant recovery
```

Recovery operations have their own full ids and `queued`, `running`, `done`, `failed` or `cancelled` states. Cancelling recovery never claims a partially restored job has been undone: the original/outputs and pinned record remain inspectable, and a checked retry can finish later. A client disconnect does not cancel the operation. A lost acknowledgement includes its known operation handle; inspect it before retrying.

Incomplete backup reservations count toward usage. Before copying, the manifest records original identity/content and metadata evidence. Cleanup may release a private partial copy only when the live original still verifies and no publication or restoration intent exists. A changed or missing original, legacy missing evidence, incomplete publication or unsupported durability keeps recovery protected and reports why. `--cleanup` never removes an output or an unexpired completed undo record. Startup attempts this same safe transition for interrupted known reservations in the background. Reservations without matching history remain listed for explicit inspection.

## What made this file

```
retype why photo.webp
```

The answer names what it knows and how it knows it:

| Answer | Meaning |
|---|---|
| `journal-identity` | This is the file a record produced. Survives the rename that usually follows |
| `journal-path` | Only the name matched a record |
| `unknown` | Nothing matched. Not an error: a file may have been made elsewhere |

An identity match also requires the size to agree, because file identities are reused by the filesystem after a delete. Where the platform lets retype write the intent string onto the file as an attribute, that is a second, independent record, and turning that off affects future conversions only. [Provenance](../advanced/provenance.md) describes both records and their limits.

Records expire at twice the undo retention. `why` past that point says the file was not produced here.

## Jobs that wait for an answer

A rename that would cost more than a threshold waits for its timer, unless the intent says `+now`. A rename that cannot run as asked is `needs-attention`, and so is one whose source is above the automatic size limit or whose intent explicitly says `+ask` or ends in `+`. An `ask` or `size` job has no timer and runs only after a person confirms it. While it waits, the source name is restored.

```
retype jobs --interactive
```

That walks the waiting jobs oldest first and reads one key each: `n` runs it as asked, `c` runs the corrected form where there is one, `r` refuses it, `s` skips it, `q` stops. It refuses to read a script's input rather than guessing.

The same decisions are available one job at a time:

```
retype jobs resolve 01J8Z --now        # run it as asked
retype jobs resolve 01J8Z --convert    # run the corrected form
```

`--now` accepts `--force` for an output that appeared while the job waited. `--convert` does not: a reduced request is not the place to decide about a file somebody else wrote. A job that cannot run as asked refuses `--now` and says why, except one parked for `size`: there `--now` is the confirmation.

The thresholds, the corrected form and the timer that eventually makes the decision are in [Deferral](../advanced/deferral.md).

## Next

- Which record a file carries: [Provenance](../advanced/provenance.md)
- Every flag and exit status: [Command line](../reference/cli.md)
