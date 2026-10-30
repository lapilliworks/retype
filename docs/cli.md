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
| `doctor [--emit\|--write\|--report]` | Tool probing, availability, support bundle |
| `bench` | Calibrate the estimator |
| `init [--output <path>]` | Write the example configuration |
| `version`, `completions <shell>` | |

Global flags, valid on every command: `--config <path>`, `--state-dir <path>`, `--json`, `--quiet`, `--verbose`, `--no-color`, `--no-spawn`, `--help`, `--version`.

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

The command runs through the daemon as an immediate job when one is reachable, and in-process under the lock otherwise. Either way it is journalled and undoable.

## 3. Explanation

```
retype explain <intent> [--from <ext>] [--name <stem>]
```

Prints the canonical intent, the derived output names, the resolved plan in pipeline order, the chosen backend, the argv it would run, the tier and every diagnostic.

| Flag | Purpose |
|---|---|
| `--from <ext>` | Source format, needed for applicability, for the tier and for recognising a kept source. An unknown or absent one is a diagnosis |
| `--name <stem>` | Source stem, for the derived names. Defaults to `input` |

The estimate needs an estimator calibrated against real files, which is `retype bench`'s job; until then `explain` prints no number rather than a guess, and takes no `--size`.

A backend that the plan needs and the machine lacks is reported in the plan and exits 4, rather than failing without an explanation: the string was understood, the machine cannot carry it out.

`explain` never touches the daemon and never writes anything. It exists to answer "what does this string mean", which is why it takes no file. "What would happen to this file" is `convert --dry-run`.

## 4. Registry

```
retype watch [<dir>...]
retype unwatch <dir>...
```

`watch` with no arguments lists registrations as `<path>  <recursive>  <conversions summary>`. With arguments it registers each directory and starts the daemon if needed. Registration is idempotent.

`unwatch` removes registrations by canonical path. Removing something absent succeeds.

## 5. Jobs and history

```
retype jobs list [--state <state>] [--limit <n>] [--since <id>]
retype jobs get <id|prefix>
retype jobs cancel <id|prefix>
retype jobs resume <id|prefix> [--force]
retype jobs logs <id|prefix> [--follow]
retype undo <id|prefix|--last>
retype why <path>
```

`jobs resume` continues a `partial` job: outputs that still match their recorded identity are skipped, the rest are run again, and `--force` re-runs all of them. Resume is available only while the source bytes are still reachable, which means the source path or the job's undo record. `jobs list --state partial` finds the candidates.

Ids accept a unique prefix; an ambiguous prefix is `E_IPC_AMBIGUOUS` with the candidates printed. `jobs logs --follow` streams from the daemon until interrupted.

`undo --last` reverses the most recent completed job, which is the gesture a notification button performs.

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
| 7 | State conflict: lock held, ambiguous prefix, wrong job state |

3 means the request cannot be satisfied. 4 means the machine cannot satisfy it. 6 means try again.

## 7. Output

- Results go to stdout, diagnostics and progress to stderr, so a successful conversion in a pipeline emits one line and nothing else
- Human output is column-aligned and colourised when stdout is a terminal
- `--json` writes one object per result to stdout; list commands write one object per line
- Envelope responses carry `"v": "retype-cli-json-1"`. Individual list items do not
- `--quiet` suppresses everything below errors. `--verbose` sets the log filter, repeatable
- Colour respects `NO_COLOR`, `CLICOLOR` and TTY detection; `--no-color` always wins

## 8. Examples

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
retype explain 'mp4+cap1080+trim10s-30s+mute+now' --from mov
retype doctor --emit
```
