# Command line

Surface, exit codes and output. JSON envelope identifier: `retype-cli-json-1`.

## 1. Shape

```
retype <command> [arguments] [flags]
```

| Command | Purpose |
|---|---|
| `convert <input> <intent>` | One-shot conversion. Alias `c` |
| `watch [<dir>...]` | Register directories, or list registrations with no arguments |
| `unwatch <dir>...` | Deregister directories |
| `daemon [start\|stop\|status]` | Foreground by default |
| `jobs list\|get\|cancel\|logs` | Queue inspection and control |
| `undo <id\|--last>` | Reverse one job |
| `why <path>` | Explain a file's conversion |
| `explain <intent>` | Parse, plan, backend, estimate and diagnostics without a file |
| `list [--tiers]` | Configured pairs. `--tiers` adds one line per claim with its engine and version, `docs/fidelity.md` |
| `tools list\|install\|update\|verify` | Managed toolchain inspection, acquisition and verification |
| `doctor [--emit\|--write\|--report]` | Tool probing, availability, support bundle |
| `bench` | Calibrate the estimator |
| `init [--output <path>]` | Write the example configuration |
| `version`, `completions <shell>` | |

Global flags, valid on every command: `--config <path>`, `--state-dir <path>`, `--json`, `--quiet`, `--verbose`, `--no-spawn`, `--help`, `--version`.

`--` terminates flag parsing, for files whose names begin with a dash.

## 2. Conversion

```
retype convert <input> <intent> [-o <path>] [--force] [--dry-run]
```

`<intent>` is the intent string from `docs/grammar.md`, taken as a positional. No `--intent` flag exists, and no output path is required, because the intent already names its outputs.

| Invocation | Result |
|---|---|
| `retype c a.heic jpg` | `a.jpg` |
| `retype c a.heic jpg+quality80` | `a.jpg` at quality 80 |
| `retype c a.heic png,webp` | `a.png` and `a.webp` |
| `retype c a.heic webp+q80+nometa` | `a.webp`, quality 80, metadata stripped |
| `retype c a.heic jpg+q80 -o holiday.jpg` | `holiday.jpg` |
| `retype c Photos zip` | `Photos.zip` |
| `retype c a.heic png,webp -o x.png` | `E_USAGE`: one path, several outputs |
| `retype c a.heic` | `E_USAGE`: nothing to do |
| `retype c a.heic jpg --dry-run` | Prints what would happen, writes nothing |

Shell quoting is rarely needed: the intent character set excludes every shell metacharacter except `!`, and `!` is never required because `now` exists as a token. `png,webp` and `jpg+q80` pass through unquoted in the common shells.

`-o` overrides the derived name and is the only way to rename and convert in one step. It requires exactly one output group.

`--force` overwrites existing outputs. Without it, an existing target is an error and nothing is written.

`--dry-run` is `explain` with a real file: same output, including the estimate from the file's actual size and the resolved backend.

The command submits an `immediate` job to the daemon and waits for it, `docs/ipc.md` §6. Immediate means it does not queue behind a directory's renames: it is journalled, visible in `jobs list`, cancellable with `jobs cancel`, and shares the pool, so parallelism is bounded the same way for a person and a watcher.

Everything that can fail before a byte moves fails at submission, with the code and the exit status the failure would have had locally, so a bad intent or a missing tool is reported without a job that only fails later. The client polls `jobs.get` while the job runs, which keeps the daemon free to answer anything else, including the cancel that stops it.

A submitted conversion does not consume its source: `convert` was handed a path and leaves it where it is. Undo for one is therefore "remove the outputs and keep the source", which is not implemented; a rename in a watched directory is the undoable path today.

A failure writes nothing and leaves nothing behind. The output is staged in its target directory and renamed into place only when the tool exits zero, so an output path is absent or complete, never empty and never half written.

## 3. Explanation

```
retype explain <name> <intent>
```

Two positionals, the same two `convert` takes, so `convert --dry-run` and `explain` print the same thing for the same arguments. There is no flag that decomposes the intent into parts, and no flag that supplies a stem or a format: the name carries both, exactly as a rename does.

| Argument | Purpose |
|---|---|
| `<name>` | The path being replaced. It need not exist, because only its stem and suffix are read |
| `<intent>` | The intent string, `docs/grammar.md` |

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

The name is read three ways, in this order:

| Name | Read as |
|---|---|
| `holiday.heic` | A file, stem `holiday`, format `heic` |
| `heic` | A source format with no file, stem `input`, which is how a user asks about a pair without having a file in hand |
| `Photos` | A directory, which is what makes `explain Photos zip` work |

A name that has a suffix this build has no format for is a diagnosis, because retype does not convert what it does not know. A name that is a declared format wins over the directory reading: `heic` is a format, and a directory may be spelled any other way.

`source` echoes the argument as written. The sentence and the outputs use the format's canonical spelling, so `explain photo.jpeg png` says `convert from jpg to png` and writes `photo.png`.

The estimate needs an estimator calibrated against real files, which is `retype bench`'s job; until then `explain` prints no number rather than a guess.

A backend that the plan needs and the machine lacks is reported in the plan and exits 4, rather than failing without an explanation: the string was understood, the machine cannot carry it out.

`explain` never touches the daemon and never writes anything. It exists to answer "what does this string mean", which is why it takes no file. "What would happen to this file" is `convert --dry-run`.

## 4. Registry

```
retype list [--tiers]
retype watch [<dir>...]
retype unwatch <dir>...
```

`list` prints what the configuration declares: every format with its family, whether it is lossy and carries alpha, and the tier a pair targeting it claims; every operation by slot with its parameter class and the formats it applies to; every preset with its tokens; and the backend order with the installed version of each. It reads configuration and the filesystem only, so it never starts a daemon.

`--tiers` prints one line per pair instead: the tier the pair claims and the backend that would carry it, with that backend's installed version. A pair nothing can carry prints the diagnostic code where a backend would go, in brackets, because a blank column would read as "not declared" rather than "not possible here". A tier here is a declaration, not evidence: a claim is earned by a fixture run, `docs/fidelity.md` §4, and until a run has happened the version beside it is what is installed now rather than what the claim was earned with.

`--json` writes one object per line with a `kind` of `format`, `operation`, `preset`, `backend` or `pair`.

`watch` with no arguments lists registrations as `<path>  <recursive>  <conversions summary>`. With arguments it registers each directory; registration is idempotent, and the daemon re-reads the file so a running one picks the directory up within a second. `watch` starts nothing itself, so it can be scripted.

The summary column is the directory's own `.retype.toml` policy, where it has one, because "what will happen here" is the question a listing is asked.

`unwatch` removes registrations by canonical path. Removing something absent succeeds.

```
retype init [--output <path>]
```

`init` prints a starting configuration, or writes it where `--output` says and creates the directories on the way. Every key in it is one this build reads, so the file works as it stands and can be pruned key by key; a file of commented examples for features that do not exist would be documentation pretending to be configuration. It refuses to replace a file that is already there, because a configuration may be the only record of how a machine was set up, and the way to start over is to remove it deliberately.

`doctor --emit` is the other half: it prints a `[tools]` snippet pinning the tools this machine resolved, which is what makes a result reproducible across machines. Only tools that are present are pinned, and a tool whose path has a space is skipped with a note, because a command with a space cannot be named in configuration. `--write` puts the snippet in the user configuration file and refuses when that file already has content, since merging by hand is the only thing that cannot silently lose a line.

## 5. Toolchain

```
retype tools list [--installed|--available]
retype tools install <tool>[@variant> [--offline <dir>]
retype tools update [<tool>]
retype tools verify [<tool>]
```

`tools list` shows each declared tool, its source, its resolved command, its version and its build fingerprint. `--installed` limits the output to the managed set; `--available` shows what the signed manifest offers for this platform and architecture.

`tools install` and `tools update` are the only commands that download. They verify the manifest signature, the generation counter, the artifact hash and the artifact signature before unpacking into `<state>/tools`. `--offline` takes a directory or archive containing the same manifest and artifacts for a machine with no network.

`tools verify` rechecks what is installed against the cached manifest and reports every artifact whose hash, signature or fingerprint no longer matches. It does not check system tools, because there is no manifest for them.

A bare tool command resolves in the managed directory before `PATH`; an absolute `command` in configuration bypasses it, `docs/config.md` §3. A failed verification is an environment failure and exits 4; it never falls through to a different tool silently.

## 6. Jobs and history

```
retype jobs list [--state <state>] [--limit <n>] [--since <id>]
retype jobs get <id|prefix>
retype jobs cancel <id|prefix>
retype jobs resume <id|prefix> [--force]
retype jobs logs <id|prefix> [--follow]
retype undo <id|prefix|--last>
retype why <path>
```

`jobs list`, `jobs get`, `jobs cancel`, `jobs resume`, `jobs logs`, `undo` and `why` exist today. A job is recorded when it starts, so `jobs list --state running` finds what can still be cancelled.

`jobs get` answers with the whole record: the outputs, their bytes, their tier and their errors. It is the form a bug report or a script wants, so it prints JSON rather than a rendering of it.

`why` takes a file and answers what produced it, naming the store that answered: `journal-identity` when the file is the one a record produced, which survives the rename that usually follows a conversion, `journal-path` when only the name matched, and `unknown` when nothing did. Identity alone is not trusted, because file ids are recycled, so the size has to agree as well. A file nothing produced is not an error: the command says so, and the record expires at twice the undo retention.

`undo` reads the undo store rather than the journal, so it answers even when the journal is gone, and it removes the record it used: one record, one restore target.

`jobs resume` continues a `partial` job: an output that still carries the identity and size the job recorded is skipped, and the rest are run again from the source bytes, which come from the source path or from the undo record when a rename consumed the name. `--force` re-runs all of them, `jobs list --state partial` finds the candidates, and a job with neither source nor record is `E_JOB_RESUME_UNAVAILABLE`.

Ids accept a unique prefix; an ambiguous prefix is `E_IPC_AMBIGUOUS` with the candidates printed.

`jobs logs` prints what the tool printed. The full output is in `<state>/logs/<id>.log`, which is written as it arrives, and the journal keeps the last four kilobytes, because a converter can print megabytes while a failure needs the last ten lines. A job whose log file is gone answers from the record instead, and says which it used. `--follow` prints what arrives while the job runs and stops when the job reaches a terminal state, so it ends by itself rather than waiting to be interrupted.

`undo --last` reverses the most recent completed job, which is the gesture a notification button performs.

## 7. Exit codes

| Code | Meaning |
|---|---|
| 0 | Success |
| 1 | Unexpected internal error |
| 2 | Usage error, including anything the argument parser rejects |
| 3 | Diagnosis: intent invalid, pair unsupported, operation inapplicable |
| 4 | Backend missing or failed |
| 5 | Filesystem error: permission, space, vanished path |
| 6 | Daemon unavailable: spawn failed, or `--no-spawn` |
| 7 | State conflict: lock held, ambiguous prefix, wrong job state |

3 means the request cannot be satisfied. 4 means the machine cannot satisfy it. 6 means try again.

## 8. Output

- Results go to stdout, diagnostics and progress to stderr, so a successful conversion in a pipeline emits one line and nothing else
- Human output is column-aligned. Nothing colours it yet: `NO_COLOR`, `CLICOLOR` and `--no-color` arrive with the first coloured output, and are not accepted as flags until then, so a script cannot pass one that does nothing
- `--json` writes one object per result to stdout; list commands write one object per line
- Envelope responses carry `"v": "retype-cli-json-1"`. Individual list items do not
- `--quiet` suppresses everything below errors
- `--verbose` is a count, not a filter language: `-v` reports what the command decided, such as which configuration file it read and which daemon it reached; `-vv` adds what it asked and got. `RETYPE_LOG` sets the same number, as a digit or as `error`, `warn`, `info`, `debug`, `trace`, so a wrapper can turn it on without rewriting arguments

## 9. Examples

```
retype c photo.heic webp+quality80+nometa
retype c clip.mov mp4+cap1080+mute
retype c photo.heic jpg,png
retype c scan.pdf png+page2-5+dpi300
retype watch ~/Pictures ~/Documents
retype jobs list --state needs-attention
retype jobs get 01J8Z
retype undo --last
retype why photo.webp
retype explain clip.mov 'mp4+cap1080+trim10s-30s+mute+now'
retype explain photo.heic 'webp+q80+cap1080+rotate90'
retype doctor --emit
retype tools list --installed
retype tools install ffmpeg@gpl
```
