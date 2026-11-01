# Configuration schema

Declarative configuration for tools, formats, operations, presets and policy.

## 1. Principle

Schema version: `retype-config-1`. Every configuration file begins with `version = 'retype-config-1'`; a file declaring a different major fails to load with an explanation.

One schema, two sources, one validator.

- `builtin.toml` ships inside the binary and defines every tool, format, operation and preset the product claims
- `retype.toml` in the user config directory overrides and extends it by name
- `.retype.toml` in a watched directory carries policy only

Both files are parsed by the same loader and validated by the same rules. Nothing is declared twice, and no operation, format, tool or preset is hardcoded in Rust at all.

Configuration is declarative throughout. There is no shell evaluation, no conditionals, no string interpolation beyond a closed placeholder set.

## 2. Precedence

Highest wins:

| Order | Source | May define |
|---|---|---|
| 1 | Command line flags | Anything a single invocation needs |
| 2 | Environment variables | Paths and switches only, see below |
| 3 | Directory file `.retype.toml` | Policy only, and only narrowing |
| 4 | User file `retype.toml` | Everything |
| 5 | `builtin.toml` | Everything the product ships |

Environment variables, all optional:

| Variable | Effect |
|---|---|
| `RETYPE_STATE_DIR` | State directory, overriding platform defaults |
| `RETYPE_CONFIG` | User configuration path, instead of the platform default. A daemon started by a client inherits the path the client used, so both read the same file |
| `RETYPE_NO_SPAWN` | Clients never start the daemon; the command-line spelling is `--no-spawn` |
| `RETYPE_LOG` | Verbosity, the same number `--verbose` counts, as a digit or a word: `error`, `warn`, `info`, `debug`, `trace` |
| `RETYPE_FIXTURES` | Property fixtures for `retype fidelity`, instead of `fixtures/fidelity`; `--fixtures` wins |
| `RETYPE_FIDELITY_MANIFEST` | Manifest `retype why` and `retype fidelity` read when no path is given; `retype fidelity`'s flags win |

`--config <path>` names the user file for one invocation and wins over `RETYPE_CONFIG`. A file named that way has to exist: falling back to the built-in table would answer a different question than the one asked, so a missing one is `E_CFG_MISSING` rather than silence.

Without either, the user file is `$XDG_CONFIG_HOME/retype/retype.toml` on Unix and `%APPDATA%\retype\retype.toml` on Windows; `XDG_CONFIG_HOME` defaults to `~/.config`. macOS is Unix here: the user file lives in `~/.config` and not in `~/Library/Application Support`, so configuration, state and data share one tree on the one platform where the two conventions disagree. There is no alternate path and no symlink between them.

Merge rules per key type:

| Type | Rule |
|---|---|
| Table | Merged key by key, recursively |
| Array | Replaced whole, never concatenated |
| Scalar | Replaced |
| Entry with `enabled = false` | Removed from the merged result |

So a user who redefines one operation's `argv` keeps that operation's slot, parameter class and format applicability, because those keys are not present in the override. To change them is to define a different operation under a name that is already taken, which is rejected as `E_CFG_VALIDATION`.

Shadowing:

| Case | Allowed | Effect |
|---|---|---|
| User overrides a built-in operation's `argv` | Yes | The operation behaves differently, the grammar does not change |
| User overrides a built-in operation's `slot`, `param` or `formats` | No | `E_CFG_VALIDATION` |
| User defines an operation with a built-in name | No | `E_CFG_NAME_CLASH` |
| User preset with a built-in preset name | Yes, with a warning, `E_CFG_SHADOWED` | The user pipeline replaces the shipped one. Its `param` is part of the merge, so a preset that needs a different parameter is a new preset rather than an override |
| User preset with an operation name | No | `E_CFG_NAME_CLASH` |
| Directory file defining anything but policy | No | `E_CFG_VALIDATION` |

A removed entry leaves everything that named it behind: disabling a tool drops it from `backends.prefer`, from every `argv` fragment keyed by it, and from `[bench.sample]`, because a fragment is how a format reaches a tool rather than a capability of its own. A name that was never declared is still a V9 error, so a typo is not silently ignored.

The distinction exists because the grammar is a published contract. A configuration file may change how an operation is executed, and may add new names to the vocabulary, but may not silently change what a shipped name means. Otherwise two machines running `retype-grammar-1` would disagree about the same intent string.

Directory files narrow and never widen: `deny` can forbid an operation, `limits` can clamp a parameter, `patterns` can restrict which files are considered. There is no key that adds a capability.

## 3. Tools

```toml
version = 'retype-config-1'

[tools.magick]
command = 'magick'                     # absolute path, or a bare name resolved on PATH
probe = ['-version']                   # argv whose output reports the version
version_min = '7.1'
purpose = 'image conversion'           # shown by retype doctor
components = ['heic', 'webp']          # substrings the probe output must mention
layout = ['{input}', '{ops}', '{output}']
```

| Key | Required | Meaning |
|---|---|---|
| `command` | yes | Executable, absolute or bare name. No shell metacharacters, no spaces |
| `aliases` | no | Alternate executable names, tried in order when `command` is absent or unusable |
| `probe` | no | Argv run to report a version: by `retype doctor`, and once at plan time. A tool that declares neither a probe nor a floor makes no version claim, so a local script is usable as it stands, and nothing runs it to find that out |
| `version_min` | no | Minimum accepted version, compared against the probe output |
| `purpose` | no | One line, printed by `retype doctor` |
| `components` | no | Substrings the probe output has to mention, see 3.2 |
| `families` | no | The format families this tool can carry: `image`, `audio`, `video`, `document`, `archive`, `folder`. A tool that names any is only chosen for a conversion that starts and ends inside them, see 3.3. Empty means no claim, which is what a user-written tool gets |
| `layout` | yes | Argv template. Names `{input}`, `{ops}` and `{output}` once each, in any order |
| `engines` | no | Named alternatives resolved at plan time, see 3.1 |

The layout is the whole reason a tool is configuration: `7zz` takes its output before its input and `ffmpeg` needs `-i` in front of its input, and neither fact belongs in code.

A bare `command` resolves in the managed toolchain first, then in `PATH`; an absolute path is used as written and pins the user's choice. The managed directory is `<state>/tools/bin` and is populated only by the GUI from a signed distribution list, `docs/tools.md` §2; the CLI never writes to it. There is no per-tool source switch: pinning is expressed by an absolute path, and the fixed order keeps.

`aliases` are other executable names for the same tool, tried in declared order after `command` when that candidate is absent or unusable. They change neither the tool's name nor its `[tools.<name>]` key, so every `argv` fragment and `backends.prefer` entry still names the declared tool. An absolute `command` is a pin and suppresses the aliases. A candidate below its version floor, or missing a required component, is skipped rather than shadowing a usable alias, and `retype doctor` reports which candidate answered.

### 3.2 Components

A tool can be installed, new enough, and still lack the delegate that makes a format work: ImageMagick without libheif opens a HEIC file and fails at the codec. `components` names the substrings that have to appear in the probe output, so the answer to "why can't I convert this" is *this build has no libheif* rather than *magick failed*.

The declaration is per tool because that is what the probe can observe. Of the capability tuple in `docs/tools.md` §1, the format and the direction belong to the operation; the build variant, version floor and components belong to the tool and its manifest.

### 3.1 Engines

Some tools delegate the hard part to an external engine, and which engine is present changes the output. An engine is a named list of candidate commands, resolved first-present:

```toml
[tools.pandoc]
command = 'pandoc'
layout = ['{input}', '{ops}', '-o', '{output}']
engines = { pdf = ['typst', 'xelatex', 'weasyprint', 'wkhtmltopdf'] }

[formats.pdf.argv.pandoc]
args = ['--pdf-engine', '{engine.pdf}']
```

Rules:

- `{engine.<name>}` may appear only in an `argv` fragment, and only if `engines.<name>` is declared
- Resolution picks the first candidate that exists on `PATH` and answers its probe, if a probe is declared for it
- When no candidate exists, the plan fails with `E_TOOL_MISSING`, naming every candidate so the message tells the user what to install
- The resolved candidate is recorded with the job and reported by `retype why`, because the same intent with a different engine is a different result
- An `argv` fragment may also name an engine literally, for example `args = ['--pdf-engine=xelatex']`. That pins the engine, bypasses resolution, and fails as `E_TOOL_MISSING` if the named engine is absent

### 3.3 Families

`families` is the one thing about a tool that a probe cannot check, and without it the matrix offers ImageMagick for a video container: an encoder that cannot open the file, chosen because it was first in `prefer` and carried the operations.

A tool that states families is eligible only when both the source and the target family are among them. A tool that states none makes no claim and is eligible for anything, so a user who writes their own tool is not forced to describe it.

The consequence is deliberate: `mp4` to `png` is `E_UNSUPPORTED`, because no single tool in the shipped table carries both. Claiming otherwise would be a promise to fail at run time.

## 4. Formats

```toml
[formats.jpg]
family = 'image'
lossy = true
alpha = false
tier = 'converted'
aliases = ['jpeg']

[formats.jpg.argv.magick]
args = ['-quality', '92']

[formats.mp4.argv.ffmpeg]
args = ['-c:v', 'libx264', '-pix_fmt', 'yuv420p', '-movflags', '+faststart']
```

| Key | Meaning |
|---|---|
| `family` | `image`, `audio`, `video`, `document`, `archive`, `folder` |
| `direction` | `decode`, `encode` or `both`, default `both`. A format that is read and never written, such as `dng`, or written and never read, such as `pdf` |
| `native` | `zip`, `tar`, `tar.gz` or `7z`. The format is repacked rather than converted. Only the `archive` family may name one, and a native target needs a native source, because a container is converted from another container. `7z` is repacked through the declared tool, `docs/tools.md` §7 |
| `lossy` | Whether the format is lossy |
| `alpha` | Whether the format carries alpha |
| `tier` | Default fidelity tier for pairs targeting this format |
| `aliases` | Alternate extension spellings, resolved by the grammar |
| `argv.<tool>` | Fragment contributed by the format to that tool's `{ops}` |

A pair whose source cannot be read or whose target cannot be written is `E_UNSUPPORTED` before a tool is chosen, because the honest answer is "not supported" rather than a tool that fails once the work has started. `retype list` prints the direction beside each format.

A format name containing a dot is written either way:

```toml
[formats."tar.gz"]      # the spelling the shipped file uses
[formats.tar.gz]        # the same format, restored by the loader
```

An unquoted `[formats.tar.gz]` is TOML for a table `gz` inside `tar`. A format entry has no nested tables except `argv`, and `family` is required of one, so the loader puts such a table back into the name `tar.gz` before anything else looks at it, and `tar.xz` needs no code when it arrives. A nested table that has no `family` is refused with the quoted spelling in the message, because it is a key of nothing and used to disappear without a word.

Both layers are flattened before they are merged, so `enabled = false` removes a compound format in either spelling.

## 5. Watch policy

What the daemon considers automatically. These keys live in the user file only: they describe this machine, its filesystems and its sync clients, so a directory file may not set them, for the same reason it may not define a tool.

```toml
[watch]
ignore = ['**/Library/Caches/**', '*.tmp']
watch_hidden = false
allow_synced = false
max_source_size = '4GiB'
```

| Key | Type | Default | Meaning |
|---|---|---|---|
| `ignore` | array of globs | `[]` | Additional paths to ignore. Adds to the built-in list, never replaces it |
| `watch_hidden` | bool | `false` | Consider dot-files and dot-directories |
| `allow_synced` | bool | `false` | Convert inside placeholder-capable cloud providers instead of refusing with the provider named |
| `max_source_size` | size | `4GiB` | Largest source for automatic conversion. `retype convert` ignores it |

All four are read. `allow_synced` decides a registration: without it, `retype watch` refuses a directory inside a provider that can hold placeholders, naming the provider, `docs/watch.md` §5.

Three things are deliberately not configurable, so that nobody tunes them and then wonders why behaviour differs between machines:

- The built-in ignore list, `docs/watch.md` §4. It is lifted only by registering a directory inside one of its entries, because naming a directory is intent
- The batch size of 64 files per debounce window and the queue cap of 256, which are churn protection rather than policy, `docs/watch.md` §8
- The debounce default of 400 ms plus 200 ms of quiet, capped at 3 s, which a directory may override but a user may not, because pacing belongs to the directory being watched

## 6. Operations

```toml
[ops.quality]
slot = 'encoder'
exclusive = ['encode-target']
param = { class = 'int', min = 1, max = 100 }
formats = ['jpg', 'webp', 'avif', 'heic']

[ops.quality.argv.magick]
args = ['-quality', '{value}']

[ops.resize]
slot = 'geometry'
exclusive = ['scale']
param = { class = 'dim', min = 1, max = 16384 }
formats = ['*']

[ops.resize.argv.magick]
args = ['-resize', '{value}']
```

| Key | Required | Meaning |
|---|---|---|
| `slot` | yes | Integer presentation rank, which orders editor controls and `retype list` rows. It never reorders an intent: the string is the pipeline, `docs/grammar.md` C2 |
| `exclusive` | no | Group names; two members of one group conflict |
| `param` | when it takes a value | `class`, plus `min` and `max` for the numeric classes, `values` for `enum`, and `scalar` for `range`, which is `int` unless it says `time` |
| `formats` | yes | Formats the operation applies to, or `['*']` |
| `argv.<tool>` | once the vertical has landed | Fragment contributed to that tool's `{ops}` |
| `say` | no | How the operation reads in a sentence, with `{value}` where its parameter goes. Absent, the canonical token is used |

Formats, operations and presets enter one name space. Aliases are declared per operation.

## 7. Presets

A preset is a named token sequence, optionally parameterised. It compiles to operations, so it composes with them and participates in the same conflict rules.

```toml
[presets.thumbnail]
param = { class = 'int', min = 16, max = 4096 }
tokens = ['cap={value}', 'quality85']

[presets.headshot]
tokens = ['crop1x1', 'cap1024', 'quality80', 'nometa']
```

`tokens` is parsed by the grammar parser with the same registry. Presets may reference other presets; cycles are load errors; `ask` and `now` are forbidden inside a preset, because a preset is a conversion rather than a decision.

A parameter and its `{value}` must agree: a preset that declares one without using it, or uses one without declaring it, is a configuration error. A preset expands where it stands, so its tokens run in its position and are subject to the same duplicate and exclusivity rules as if they had been typed there. The canonical form prints what the preset expanded to, because that is what runs.

## 8. Assembly

For an external backend, argv is the tool `layout` with substitutions:

- `{input}` and `{output}` are paths
- `{ops}` expands to the concatenation of the format fragments in declaration order, then the operation fragments in canonical slot order

No splicing happens inside an argument; a placeholder occupying a whole argument is replaced by a list, a placeholder inside an argument is replaced by a string. Both forms are permitted and distinct.

Placeholders belong to one position and not the other. `{input}`, `{ops}` and `{output}` are layout holes; `{value}` and `{engine.<name>}` are fragment holes. A fragment naming a layout hole, or a layout naming a fragment hole, is a validation error rather than a literal brace in the argv.

Every operation reaches a process this way. There is no in-process backend, and no plan carries a typed implementation instead of an argv.

Backend selection:

```toml
[backends]
prefer = ['magick', 'ffmpeg', 'pandoc', '7zz']
```

`prefer` names tools and nothing else. A file that omits the table gets every declared tool in name order, which is deterministic but rarely what a user wants, so the shipped file states an order. Archive formats are not one backend: `zip` and `tar` are native, and `7zz` carries `.7z` only, `docs/tools.md` §7.

The first backend in `prefer` that implements every operation in the plan and is available wins. Determinism is required: a plan never depends on discovery order, so a missing earlier backend moves the plan down the list rather than shuffling it.

Availability is a plan-time question with a fixed answer at that moment: the tool is in the managed directory or on `PATH`, above its version floor, and carrying its components. The chosen backend, its resolved engine and its build fingerprint are recorded with the job, because a later machine that has since gained ImageMagick must not silently produce a different file.

## 9. Undo retention, quota and overflow

Host-level, never per-directory: a `.retype.toml` committed to a repository must not decide every contributor's disk budget.

```toml
[undo]
enabled = true
retention = '1w'
quota = '5gb'
on_overflow = 'ask'
```

| Key | Default | Meaning |
|---|---|---|
| `enabled` | `true` | Keep records at all. Taking effect for future jobs, because a record has to exist at conversion time |
| `retention` | `1w` | Age after which a record is dropped |
| `quota` | `5gb` | Upper bound for the store, counted in source bytes |
| `on_overflow` | `ask` | `ask`, `evict` or `skip`, `docs/dev/architecture/state.md` §5.5 |

`retention` is read as the grammar's duration class, so it is spelled the way an intent spells one: `w`, `d`, `h`, `m` and `s`, in that order, and a unit may be skipped. Months are not a unit, because a month is not a fixed number of seconds. `quota` is read as the size class, so `gb` is decimal and `gib` binary, `docs/grammar.md` §7.

Retention decides two things at once, deliberately rather than incidentally: how long an undo record survives, and therefore how long the job it describes can be resumed, `docs/dev/architecture/state.md` §5.2. Records are dropped at startup and never during a conversion.

The quota is a second, independent bound: retention bounds records by age and the quota bounds them by size. `ask` is the intended answer and behaves as `evict` until notifications exist to ask with.

The journal outlives them. It is compacted at startup when it earns it, dropping records past twice the retention with a floor of thirty days, `docs/dev/architecture/state.md` §4.

## 10. Deferral

Host-level, because the thresholds describe this machine and this person's tolerance for waiting rather than a directory's contents, `docs/deferral.md`.

```toml
[deferral]
mode = 'ask'
above = '10s'
above_without_undo = '2s'
pause = '1h'
repair = 'ask'
```

| Key | Default | Meaning |
|---|---|---|
| `mode` | `ask` | `ask` defers by `docs/deferral.md` §3; `now` skips the two cost triggers and still parks a job that cannot run as asked |
| `above` | `10s` | Estimated duration above which a watched rename is parked |
| `above_without_undo` | `2s` | The same, for a job that consumes its source and keeps no record of it |
| `pause` | `1h` | How long a parked job waits before it runs anyway |
| `repair` | `ask` | `ask` parks a job that cannot run as asked; `convert` runs it without the tokens that could not be carried, `docs/deferral.md` §4 |

Durations are read as the grammar's time class, so they are spelled the way an intent spells one. A threshold is compared against the whole job, which is the sum of its outputs' estimates. `pause = '0s'` is refused: a parked job that never expires is a cancelled job with extra paperwork.

A directory file may set `confirm = false`, which suppresses the ordinary cost trigger in that directory and nothing else, `docs/deferral.md` §5.

## 11. Provenance

Host-level, like retention.

```toml
[provenance]
xattr = true
```

| Key | Default | Meaning |
|---|---|---|
| `xattr` | `true` | Record the intent on the file as an extended attribute, where the platform has them |

The attribute is `user.retype.intent` on Linux and `com.retype.intent` on macOS. It is written after an output is produced and never read as a precondition: a filesystem that refuses attributes, or a policy that strips them, changes what `retype why` can answer later and nothing else. Windows streams are not written until that question is settled, `docs/provenance.md` §5.

Turning it off affects future conversions. An attribute already on a file is still read, because it is evidence about that file rather than a setting of this one.

## 12. Per-directory policy

`.retype.toml` in a watched directory may change behaviour but MUST NOT define tools, formats, operations or presets. A directory can arrive from an untrusted source, so it cannot introduce a command.

```toml
version = 'retype-config-1'
patterns = ['*.jpg', '*.png']
debounce_ms = 800
deny = ['nometa']
limits = { quality = 85 }
confirm = false
paused = false
```

| Key | Meaning |
|---|---|
| `patterns` | Globs a file must match to be considered. A pattern without `/` matches the file name; one with it matches the path relative to this directory. Empty means everything |
| `deny` | Operations refused here, by name or alias. A job that asks for one is `E_INAPPLICABLE` and parks, `docs/deferral.md` |
| `limits` | Ceilings for a parameter, by operation name. A number is clamped and the derived name says the clamped value; a class with no ceiling is left alone |
| `confirm` | `false` suppresses the cost confirmation in this directory, and not the one that exists because a source would be destroyed with no record, `docs/deferral.md` §5 |
| `paused` | Whether the directory is watched at all. `retype watch` refuses it with `E_WATCH_PAUSED_BY_POLICY` naming the file, rather than accepting a registration that the next read would undo |
| `debounce_ms` | How long a rename here waits for the file to stop changing before anything decides, `docs/watch.md` §7. Below the daemon's own window it changes nothing |

Every key is read. A `[conversions]` table was specified here once and has been dropped rather than built, for two reasons.

A per-pair `preset` would add operations to a rename nobody typed, and the rename is the request: `a.heic.png` asks for a png, and a directory deciding it also means `headshot` is an expansion of the request rather than a bound on it. Everything in this section narrows. `limits` clamps a value that was asked for and the derived name says the clamped number; a preset would invent a value that was not.

A per-pair `enabled = false` is a different and much smaller idea: refusing a *target format* here, which `deny` cannot express because it refuses operations. If that is wanted it should be specified as a format refusal beside `deny`, in one line, rather than as half of a rule engine. The gap is recorded in.

A file is read when it changes rather than on a second watcher: the answer is cached per directory and re-read when its modification time moves, which is the same answer `docs/watch.md` §2 asks for with less machinery.

An undeclared key here is ignored rather than refused, unlike the host file: every key that could widen something is refused by name, and a key nothing reads cannot change a decision. The host file has no such excuse, so it refuses one.

## 13. Validation

Loading fails, naming the file and key, on any of:

A key the entry does not declare is refused as `E_CFG_UNKNOWN_KEY` before any rule below is reached, so `qaulity = 3` under `[formats.jpg]` is an error rather than a silent no-op.

| Rule | Condition |
|---|---|
| V1 | A placeholder outside `{input}`, `{ops}`, `{output}`, `{value}`, `{engine.<name>}`, or in the wrong position |
| V2 | A `layout` that does not name `{input}`, `{ops}` and `{output}` exactly once each |
| V3 | `{value}` in an entity that declares no `param`, or a declared `param` never used |
| V14 | A `say` with a hole other than `{value}`, or whose `{value}` disagrees with whether the operation takes a parameter |
| V15 | A command or alias named more than once on the same tool |
| V4 | `{engine.<name>}` naming an engine the tool does not declare |
| V5 | A command or alias containing a shell metacharacter or a space |
| V6 | A name used twice, or a preset name equal to an operation name or alias |
| V7 | A preset whose tokens do not parse, or a reference cycle |
| V8 | An exclusive group with a single member, or a group declared but never used |
| V9 | A format or tool referenced by an operation, fragment or `backends.prefer` but not declared |
| V10 | A tier outside `exact`, `converted`, `delegated`, `docs/fidelity.md` |
| V11 | `version_min` that does not parse as a version |
| V13 | A per-directory file defining `tools`, `formats`, `ops`, `presets`, `backends`, `undo`, or any key of the `[watch]` table |
| V16 | `components` declared by a tool that declares no `probe` |
| V17 | `version_min` declared by a tool that declares no `probe`, since the floor could then never be met |
| V18 | A `families` entry naming a family that does not exist |
| V19 | A `[bench.seed]` naming an undeclared format, or a format of another family |
| V20 | A `[bench.sample]` naming a family with no `[bench.seed]`, or a tool that is not declared |
| V21 | A sample argv naming a placeholder other than `{output}` |
| V22 | A `native` codec on a format whose family is not `archive` |
| V23 | A `[deferral]` key in a directory file, other than `confirm`, which is the same key as the top-level spelling |
| V24 | A `pause` of zero, a deferral value that is not a duration, or a `mode` or `repair` outside the two it accepts |

V12 named an in-process backend and was retired with it. The number is not reused, so that a configuration error reported against an older build stays identifiable.

Warnings, not errors: a name that is a prefix of another name; a tool that fails its probe; a pair whose tier is `delegated` but has no `argv` fragment. A managed artifact whose fingerprint no longer matches its manifest is reported by the GUI's verification, not by a CLI warning.

## 14. Probing

Probing is an authoring aid, never a runtime decision.

```
retype doctor             # tools, versions, unavailable pairs, prefix warnings
retype doctor --emit      # a [tools] snippet reflecting what is installed
```

`--emit` writes nothing unless asked to with `--write`. The emitted snippet is ordinary configuration and is validated like any other.

## 15. Benchmark samples

What `retype bench` converts, `docs/dev/architecture/state.md` §7. Host-level.

```toml
[bench.seed]
image = 'jpg'
video = 'mp4'
audio = 'mp3'

[bench.sample.image.magick]
args = ['-size', '2400x1600', '-seed', '0', 'plasma:fractal', '{output}']
```

| Key | Meaning |
|---|---|
| `[bench.seed]` | The format a family's sample is generated in, one entry per family that is measured |
| `[bench.sample.<family>.<tool>]` | An argv that writes the sample to `{output}` |

The argv is a tool's own way of drawing something, so it is declared rather than written in Rust. `{output}` is the only hole: the sample generator has no input. The first declared tool that is installed generates the sample, in `[backends]` order, so the preference the product states is the one the benchmark measures.

A seed is a statement about which format a family is sampled in, not a promise that a tool is installed, so a seed whose generators have all been removed still loads: `retype bench` reports that family as unmeasurable. A family may also be measured without being listed here, by naming pairs with `--pair`.

## 16. Versioning

Version identifier: `retype-config-1`, declared at file top level. The key is mandatory in user and directory files and implicit in `builtin.toml`. A file with an unknown major fails to load; a file with a lower major is upgraded through a declared migration when one exists. Keys are never reused for different meanings, and a renamed key stays accepted as an alias for at least one major version.

## 17. Notifications

Host-level, because whether a machine has somewhere to show a notification is a property of the session rather than of a directory.

```toml
[notify]
enabled = true
actions = true
```

| Key | Default | Meaning |
|---|---|---|
| `enabled` | `true` | Whether the daemon says what a batch did. Off is a supported answer: it then says nothing except in its own log |
| `actions` | `true` | Whether the notification carries an Undo button, on a platform that has buttons |

One notification per rename batch, after its last job settles, so a directory of photographs is one interruption rather than twelve. It says what happened, not what was attempted: `3 converted`, or `1 converted, 1 failed (a.heic: magick is not installed)`, or `1 parked; retype jobs --interactive decides what happens`.

The button is Undo, and it reverses every job of the batch that has a record. Tweak belongs to the editor and to the `retype://` handler, both of which are a later release's later half.
