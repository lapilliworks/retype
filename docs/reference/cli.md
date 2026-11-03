# Command line

Surface, exit codes and output. JSON envelope identifier: `retype-cli-json-1`.

## 1. Shape

```
retype <command> [arguments] [flags]
```

| Command | Purpose |
|---|---|
| `convert <input> <intent>` | One-shot conversion. Alias `c` |
| `watch [<dir>...] [--non-recursive] [--pause\|--resume]` | Register directories; with none, the current directory |
| `watching` | List registrations |
| `unwatch <dir>...` | Deregister directories |
| `daemon [--detached] [start\|stop [--now] [--force]\|status]` | Foreground by default. `stop` drains, `--now` cancels, `--force` overrides a supervisor's refusal |
| `jobs list\|get\|cancel\|resume\|resolve\|logs` | Queue inspection and control |
| `undo <id\|--last>` | Reverse one job |
| `why <path>` | Explain a file's conversion |
| `explain <source> <intent>` | Parse, plan, backend, estimate and diagnostics without a file |
| `list [--tiers]` | Configured pairs. `--tiers` adds one line per claim with its engine and version, `docs/advanced/fidelity.md` |
| `doctor [--check] [--require <tool>] [--emit\|--write\|--report]` | Tool probing, availability, support bundle |
| `bench [--pair <from>:<to>] [--family <name>]` | Measure this machine, so estimates are about it |
| `fidelity [--pair <from>:<to>] [--family <name>] [--write <path>] [--manifest <path>] [--render <path>] [--fixtures <dir>]` | Earn the fidelity claims for this machine and record the build that earned them |
| `init [--output <path>]` | Write the example configuration |
| `version` | The build and every schema identifier it reads |
| `completions <shell>` | A completion script for bash, zsh, fish, elvish or PowerShell |

Global flags, valid on every command: `--config <path>`, `--state-dir <path>`, `--json`, `--quiet`, `--verbose`, `--no-spawn`, `--help`, `--version`.

`retype uri <retype://job/<id>[?action=undo|cancel|resolve]>` is the scheme handler a notification or desktop entry calls. It is hidden from help because it is not typed by a person, and it is the CLI half of the actions `docs/reference/watch.md` §11 names. The default action is `undo`. The scheme, `job` and `action` are read in either case.

The CLI does not install, update or verify external tools. A CLI user acquires them with the platform's package manager or manually, and the CLI uses the result on `PATH`, an absolute path in configuration, or a managed set the GUI installed, `docs/advanced/converters.md` §2. `retype doctor` reports what the machine provides.

`--` terminates flag parsing, for files whose names begin with a dash.

## 2. Conversion

```
retype convert <input> <intent> [-o <path>] [--force] [--dry-run]
```

`<intent>` is the intent string from `docs/reference/intents.md`, taken as a positional. No `--intent` flag exists, and no output path is required, because the intent already names its outputs.

| Invocation | Result |
|---|---|
| `retype c a.heic jpg` | `a.jpg` |
| `retype c a.heic jpg+quality80` | `a.jpg` at quality 80 |
| `retype c a.heic png,webp` | `a.png` and `a.webp` |
| `retype c a.heic webp+q80+cap1080` | `a.webp`, quality 80, capped at 1080 |
| `retype c a.heic jpg+q80 -o holiday.jpg` | `holiday.jpg` |
| `retype c a.heic png,webp -o x.png` | `E_USAGE_OUTPUT_COUNT`: one path, several outputs |
| `retype c a.heic` | `E_USAGE`: nothing to do |
| `retype c a.heic jpg --dry-run` | Prints what would happen, writes nothing |

Shell quoting is rarely needed: the intent character set excludes every shell metacharacter except `!`, and `!` is never required because `now` exists as a token. `png,webp` and `jpg+q80` pass through unquoted in the common shells.

`-o` overrides the derived name and is the only way to rename and convert in one step. It requires exactly one output group.

`--force` overwrites existing outputs. Without it, an existing target is an error and nothing is written.

`--dry-run` is `explain` with a real file: same output, including the estimate from the file's actual size and the resolved backend.

Under `--json`, the result is `{v, ok, job}`: the envelope identifies the CLI shape, and the job is the journal record with its own `v`, `docs/dev/architecture/state.md` §3.

The command submits an `immediate` job to the daemon and waits for it, `docs/dev/architecture/ipc.md` §6. Immediate means it does not queue behind a directory's renames: it is journalled, visible in `jobs list`, cancellable with `jobs cancel`, and shares the pool, so parallelism is bounded the same way for a person and a watcher.

Everything that can fail before a byte moves fails at submission, with the code and the exit status the failure would have had locally, so a bad intent or a missing tool is reported without a job that only fails later. The client polls `jobs.get` while the job runs, which keeps the daemon free to answer anything else, including the cancel that stops it.

A submitted conversion does not consume its source. Its recovery record holds an independent source snapshot and verified publication identities. Undo removes only unchanged outputs produced by the job and leaves the live source untouched. Input aliases remain protected even with `--force`.

Each output is staged in a private directory on the target filesystem. Publication requires a fresh, regular, nonempty result and checked finalization. A failed output is not published; already completed sibling outputs and required recovery records are retained. Backend success alone never permits deleting the original.

## 3. Explanation

```
retype explain <name> <intent>
```

Two positionals, the same two `convert` takes, so `convert --dry-run` and `explain` print the same thing for the same arguments. There is no flag that decomposes the intent into parts, and no flag that supplies a stem or a format: the name carries both, exactly as a rename does.

| Argument | Purpose |
|---|---|
| `<name>` | The path being replaced. It need not exist, because only its stem and suffix are read |
| `<intent>` | The intent string, `docs/reference/intents.md` |

Prints the canonical intent, the same intent in words, the derived output names, the resolved plan in pipeline order, the chosen backend, the argv it would run, the tier and every diagnostic:

```
intent    webp+quality80+cap1080+rotate90
source    holiday.heic
says      convert from heic to webp with quality at 80, then cap to 1080, then rotate 90 degrees
output    holiday-quality80-cap1080-rotate90.webp
  backend magick
  argv    magick {input} -quality 80 -filter Lanczos -resize 1080x1080> -rotate 90 {output}
  tier    converted
```

The sentence is fixed rather than generated: each operation declares how it reads, in `say`, with `{value}` where its parameter goes, so the wording is the same every time and a user-declared operation describes itself. Actions appear in the order they run, outputs are separated by a semicolon, `ask` and `now` close the sentence, and an operation without a phrase falls back to its canonical token rather than leaving a gap.

The source is read two ways, in this order:

| Source | Read as |
|---|---|
| `holiday.heic` | A file, stem `holiday`, format `heic` |
| `heic` | A source format with no file, stem `input`, which is how a user asks about a pair without having a file in hand |

A source that has a suffix this build has no format for is a diagnosis, because retype does not convert what it does not know. A directory is not a conversion source.

`source` echoes the argument as written. The sentence and the outputs use the format's canonical spelling, so `explain photo.jpeg png` says `convert from jpg to png` and writes `photo.png`.

The estimate is a line per output, under the plan:

```
  estimate 0.4s  291.42kb  (measured)
```

It needs a file to size, so `explain` prints none when the name does not exist rather than guessing, and `convert --dry-run` prints one because it has the file. `measured` means this machine converted this pair when `retype bench` last ran; `family` means no measurement exists and the family constant is standing in, which is a guess with a provenance and is labelled as one, `docs/dev/architecture/state.md` §7.

A backend that the plan needs and the machine lacks is reported in the plan and exits 4, rather than failing without an explanation: the string was understood, the machine cannot carry it out.

`explain` never touches the daemon and never writes anything. It exists to answer "what does this string mean", which is why it takes no file. "What would happen to this file" is `convert --dry-run`.

## 4. Registry

```
retype list [--tiers]
retype watch [<dir>...] [--non-recursive] [--pause|--resume]
retype watching
retype unwatch <dir>...
```

`list` prints what the configuration declares: every format with its family, whether it is lossy and carries alpha, and the tier a pair targeting it claims; every operation by slot with its parameter class and the formats it applies to; every preset with its tokens; and the backend order with the installed version of each. It reads configuration and the filesystem only, so it never starts a daemon.

`--tiers` prints one line per pair instead: the tier the pair claims and the backend that would carry it, with that backend's installed version. A pair nothing can carry prints the diagnostic code where a backend would go, in brackets, because a blank column would read as "not declared" rather than "not possible here". A tier here is a declaration, not evidence: a claim is earned by running the conversion against the installed tool, `docs/advanced/fidelity.md` §4, and until that has happened the version beside it is what is installed now rather than what the claim was earned with.

`--json` writes one object per line with a `kind` of `format`, `operation`, `preset`, `backend` or `pair`. A pair that cannot be carried has a null `backend` and a separate `error` field naming the code; the human line brackets the same code where a backend would go, because a blank column would read as "not declared" rather than "not possible here".

`watching --json` writes one object per registration with a `kind` of `watch`; a registration result and an `unwatch` result carry the envelope and a `kind`. The `added` field is Unix milliseconds.

`watching` prints registrations as `<path>  <recursive>  <policy summary>`. `watch` registers each directory it is given, and with none the current directory, so `watch` and `watch .` register the same thing. Registration is idempotent, and the daemon re-reads the file so a running one picks the directory up within a second. `--non-recursive` registers only that directory rather than its subtree. `--pause` stops converting renames in a registration until `--resume`, and applies to the current directory when none is named; a directory file with `paused = true` outranks a resume and is refused with `E_WATCH_PAUSED_BY_POLICY`. Neither command starts anything itself, so both can be scripted.

The summary column is the directory's own `.retype.toml` policy, where it has one, because "what will happen here" is the question a listing is asked.

`unwatch` removes registrations by canonical path. Removing something absent succeeds.

```
retype init [--output <path>]
```

`init` prints a starting configuration, or writes it where `--output` says and creates the directories on the way. Every key in it is one this build reads, so the file works as it stands and can be pruned key by key; a file of commented examples for features that do not exist would be documentation pretending to be configuration. It refuses to replace a file that is already there, because a configuration may be the only record of how a machine was set up, and the way to start over is to remove it deliberately.

`doctor --emit` is the other half: it prints a `[tools]` snippet pinning the tools this machine resolved, which is what makes a result reproducible across machines. Only tools that are present are pinned, and a tool whose path has a space is skipped with a note, because a command with a space cannot be named in configuration. `--write` puts the snippet in the user configuration file and refuses when that file already has content, since merging by hand is the only thing that cannot silently lose a line.

## 5. Jobs and history

```
retype jobs list [--state <state>] [--limit <n>] [--since <id>]
retype jobs get <id|prefix>
retype jobs cancel <id|prefix>
retype jobs resume <id|prefix> [--force]
retype jobs logs <id|prefix> [--follow]
retype jobs resolve <id|prefix> --now [--force] | --convert
retype jobs --interactive
retype undo <id|prefix|--last>
retype why <path>
```

`jobs list`, `jobs get`, `jobs cancel`, `jobs resume`, `jobs logs`, `undo` and `why` exist today. A job is recorded when it starts, so `jobs list --state running` finds what can still be cancelled. `--state` accepts `pending`, `queued`, `running`, `done`, `partial`, `failed`, `cancelled`, `superseded` or `needs-attention`; any other value is a usage error. `--since` takes a full id, because a prefix is not an ordering boundary.

`jobs get` answers with the whole record: its own `v`, the outputs, their bytes, their tier and their errors. It is the form a bug report or a script wants, so it prints JSON rather than a rendering of it. The record is not enveloped, because its `v` is `retype-journal-1` and an envelope would overwrite it.

`why` takes a file and answers what produced it, naming the store that answered: `journal-identity` when the file is the one a record produced, which survives the rename that usually follows a conversion, `journal-path` when only the name matched, `xattr` when the intent came from the file's attribute, and `unknown` when nothing did. Its `confidence` is `exact`, `path-only` or `none`, `docs/advanced/provenance.md` §8. Identity alone is not trusted, because file ids are recycled, so the size has to agree as well. A file nothing produced is not an error: the command says so, and the record expires at twice the undo retention.

`undo` reads the undo store rather than the journal, so it answers even when the journal is gone, and it removes the record it used: one record, one restore target. An active conversion or recovery returns `E_IO_LOCKED`; cancel it and wait for it to stop before undoing.

`jobs resume` continues a partial or interrupted job from a private copy of its verified retained original. An output whose identity, size and digest still match is skipped. Any other occupied path is `E_IO_EXISTS`; `--force` reruns outputs but cannot replace the input itself. Successful resume retains the backup for normal undo retention. Jobs without verified recovery bytes cannot resume.

Ids accept a unique prefix of at least four characters; an ambiguous prefix is `E_JOB_AMBIGUOUS` with the candidates printed, and a shorter one is a usage error. Listings print the shortest unambiguous prefix so copy-paste is short by default. Ids and `--state` values are read in either case and printed lowercase.

`jobs resolve` acts on a parked job, and `jobs --interactive` walks the parked ones with one key each; both are specified in `docs/advanced/deferral.md` §8.

`jobs --interactive` walks the parked jobs oldest first and reads one key each: `n` runs it as asked, `c` runs the corrected intent where there is one, `r` refuses it, `s` skips it and `q` stops. It refuses to run without a terminal rather than reading a script's stdin. `jobs resolve` is the same decisions for a script, one id at a time.

`jobs resolve` acts on a parked job: `--now` runs it as asked, `--convert` runs the corrected intent with the tokens that could not be carried dropped. `--force` belongs to `--now` and overwrites an output that appeared while the job waited, which without it fails as `E_IO_EXISTS`. It is not accepted with `--convert`: a reduced request is not the place to decide about a file somebody else wrote. A job that cannot run as asked refuses `--now` with `E_JOB_STATE` and says why, except one parked for `size`, where `--now` is the confirmation. A job whose target format does not survive the reduction has nothing for `--convert` to run. Exactly one of the two is required; the states, the reasons and the timer are `docs/advanced/deferral.md`.

`jobs logs` prints what the tool printed. The full output is in `<state>/logs/<id>.log`, which is written as it arrives, and the journal keeps the last four kilobytes, because a converter can print megabytes while a failure needs the last ten lines. A job whose log file is gone answers from the record instead, and says which it used. `--follow` prints what arrives while the job runs and stops when the job reaches a terminal state, so it ends by itself rather than waiting to be interrupted.

`undo --last` selects the most recent retained job record. An active job returns `E_IO_LOCKED` rather than racing its worker.

## 6. Exit codes

| Code | Meaning |
|---|---|
| 0 | Success |
| 1 | Unexpected internal error |
| 2 | Usage error, including anything the argument parser rejects |
| 3 | Diagnosis: intent invalid, pair unsupported, operation inapplicable |
| 4 | Backend missing or failed |
| 5 | Filesystem error: permission, space, vanished path |
| 6 | Daemon unavailable: spawn failed, or `--no-spawn` |
| 7 | State conflict: lock held, ambiguous handle, cancelled or interrupted job, wrong job state |

3 means the request cannot be satisfied. 4 means the machine cannot satisfy it. 5 means try again. 6 means the daemon is not there. 7 means the job or the store is in a state the request cannot move.

## 7. Output

- Results go to stdout, diagnostics and progress to stderr, so a successful conversion in a pipeline emits one line and nothing else
- Human output is column-aligned. Nothing colours it yet: `NO_COLOR`, `CLICOLOR` and `--no-color` arrive with the first coloured output, and are not accepted as flags until then, so a script cannot pass one that does nothing
- `--json` writes one object per result to stdout; list commands write one object per line. A parser error and `--version` honour it too
- Envelope responses carry `"v": "retype-cli-json-1"`. Individual list items do not; they carry a `kind` instead
- `jobs get` is the exception: it prints the journal record as it is, so the record keeps `"v": "retype-journal-1"`
- `--quiet` suppresses everything below errors
- `--verbose` is a count, not a filter language: `-v` reports what the command decided, such as which configuration file it read and which daemon it reached; `-vv` adds what it asked and got. `RETYPE_LOG` sets the same number, as a digit or as `error`, `warn`, `info`, `debug`, `trace`, so a wrapper can turn it on without rewriting arguments

## 8. Examples

```
retype c photo.heic webp+quality80+cap1080
retype c clip.mov mp4+cap1080+mute
retype c photo.heic jpg,png
retype watch ~/Pictures ~/Documents
retype watching
retype jobs list --state needs-attention
retype jobs get 01j8z
retype undo --last
retype why photo.webp
retype explain clip.mov 'mp4+cap1080+at10s+mute+now'
retype explain photo.heic 'webp+q80+cap1080+rotate90'
retype doctor --emit
retype fidelity --pair heic:webp
```

## 9. Fidelity claims

```
retype fidelity [--pair <from>:<to>] [--family <name>] [--write <path>] [--manifest <path>] [--render <path>] [--fixtures <dir>]
```

The command runs the conversion each pair declares, records the tier, the backend, the resolved engine, the tool version, the build fingerprint and what the pair kept, and reports the state of every claim: `new` when no manifest had one, `earned` when the installed build is the one it was earned with, `unverified` when it is not, `docs/advanced/fidelity.md` §6.

Alongside the sample conversion, a set of small property files is converted through the same plan and probed on both sides, so the PROPERTIES column says which properties survived: `alpha,depth,icc` when all three did, `alpha; !depth` when one did not. `--fixtures` names the directory those files are read from, `docs/advanced/fidelity.md` §4.

`--write` reads a manifest, records what this run earned and writes it back. `--manifest` compares against one without rewriting it. `--render` replaces the table between the `claims` markers in a document, which is how `docs/advanced/fidelity.md` §8 is filled.

A claim that declares `exact` and is `unverified` exits 4 rather than silently downgrading a promise of no loss. So does an `exact` claim whose source has no fixture, and one whose fixtures lost a property: `exact` means nothing was lost, and a run that cannot show it has not earned the word. Nothing earned at all exits 4 for the same reason.

## 10. Support report

```
retype doctor --report [<path>]
```

One file, written locally, that a user can read and send: the build and every schema identifier it reads, the machine, the configuration in use with its contents, the tool table with each build's fingerprint, the watch registrations, what the state directory holds, the last forty journal records and the last three job logs.

It is generated on the machine and goes nowhere unless the user sends it. The header says the file holds the configuration and the last records, because a log can hold anything a converter printed. Without a path it writes `retype-report-<UTC timestamp>.txt` in the current directory and says where it went.

`--report` cannot be combined with `--emit` or `--write`: a report and a pinning snippet answer different questions, and one command that wrote both would write a file nobody asked for.
