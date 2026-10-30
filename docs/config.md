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
| `RETYPE_CONFIG` | User configuration path, instead of the platform default |
| `RETYPE_NO_AUTOSTART` | Clients never start the daemon |
| `RETYPE_LOG` | Log filter, same syntax as the `--verbose` counters |
| `NO_COLOR`, `CLICOLOR` | Colour handling, per their respective conventions |

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

A removed entry leaves the preference list with it: disabling a tool drops it from `backends.prefer` rather than making the user restate the whole order. A name that was never declared is still a V9 error, so a typo is not silently ignored.

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
| `probe` | no | Argv run by `retype doctor` to report a version. A tool that declares neither a probe nor a floor makes no version claim, so a local script is usable as it stands |
| `version_min` | no | Minimum accepted version, compared against the probe output |
| `purpose` | no | One line, printed by `retype doctor` |
| `components` | no | Substrings the probe output has to mention, see 3.2 |
| `layout` | yes | Argv template. Names `{input}`, `{ops}` and `{output}` once each, in any order |
| `engines` | no | Named alternatives resolved at plan time, see 3.1 |

The layout is the whole reason a tool is configuration: `7zz` takes its output before its input and `ffmpeg` needs `-i` in front of its input, and neither fact belongs in code.

A bare `command` resolves in the managed toolchain first, then in `PATH`; an absolute path is used as written and pins the user's choice. The managed directory is `<state>/tools/bin` and is populated only from a signed distribution list, `docs/tools.md` §2. There is no per-tool source switch: pinning is expressed by an absolute path, and the fixed order keeps.

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
| `lossy` | Whether the format is lossy |
| `alpha` | Whether the format carries alpha |
| `tier` | Default fidelity tier for pairs targeting this format |
| `aliases` | Alternate extension spellings, resolved by the grammar |
| `argv.<tool>` | Fragment contributed by the format to that tool's `{ops}` |

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

Three things are deliberately not configurable, so that nobody tunes them and then wonders why behaviour differs between machines:

- The built-in ignore list, `docs/watch.md` §4. It is lifted only by registering a directory inside one of its entries, because naming a directory is intent
- The batch size of 64 files per debounce window and the queue cap of 256, which are churn protection rather than policy, `docs/watch.md` §8
- The debounce default of 500 ms, which a directory may override but a user may not, because pacing belongs to the directory being watched

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

Availability is a plan-time question with a fixed answer at that moment: the tool is in the managed directory or on `PATH`, above its version floor, and carrying its components. A managed tool is also checked against its manifest. The chosen backend, its resolved engine and its build fingerprint are recorded with the job, because a later machine that has since gained ImageMagick must not silently produce a different file.

## 9. Undo retention

Host-level, never per-directory: a `.retype.toml` committed to a repository must not decide every contributor's disk budget.

```toml
[undo]
enabled = true
retention = '168h'
```

| Key | Default | Meaning |
|---|---|---|
| `enabled` | `true` | Keep records at all. Taking effect for future jobs, because a record has to exist at conversion time |
| `retention` | `168h` | Age after which a record is dropped |

`retention` is read as the grammar's duration class, so it is spelled the way an intent spells one: `h`, `m` and `s`, in that order, and a unit may be skipped. The class has no day unit, so a week is `168h`.

Retention decides two things at once, deliberately rather than incidentally: how long an undo record survives, and therefore how long the job it describes can be resumed, `docs/state.md` §5.2. Records are dropped at startup and never during a conversion.

The journal outlives them. It is compacted at startup when it earns it, dropping records past twice the retention with a floor of thirty days, `docs/state.md` §4.

## 10. Per-directory policy

`.retype.toml` in a watched directory may change behaviour but MUST NOT define tools, formats, operations or presets. A directory can arrive from an untrusted source, so it cannot introduce a command.

```toml
version = 'retype-config-1'
patterns = ['*.jpg', '*.png']
debounce_ms = 800
deny = ['nometa']
limits = { quality = 85 }
confirm = false
paused = false

[conversions.to-jpg]
enabled = false

[conversions.heic-to-jpg]
preset = 'headshot'
```

`deny` rejects an operation in that directory with `E_INAPPLICABLE`. `limits` clamps a parameter. `confirm = false` suppresses deferral for cheap jobs.

`paused = true` makes the directory unwatched by policy, and policy beats state: a later `retype watch --resume <dir>` is refused with `E_WATCH_PAUSED_BY_POLICY` naming the file, rather than being accepted and then quietly undone the next time the file is read.

## 11. Validation

Loading fails, naming the file and key, on any of:

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
| V13 | A per-directory file defining `tools`, `formats`, `ops`, `presets`, or any key of the `[watch]` table |

V12 named an in-process backend and was retired with it. The number is not reused, so that a configuration error reported against an older build stays identifiable.

Warnings, not errors: a name that is a prefix of another name; a tool that fails its probe; a managed tool whose fingerprint differs from its manifest; a pair whose tier is `delegated` but has no `argv` fragment.

## 12. Probing

Probing is an authoring aid, never a runtime decision.

```
retype doctor             # tools, versions, unavailable pairs, prefix warnings
retype doctor --emit      # a [tools] snippet reflecting what is installed
```

`--emit` writes nothing unless asked to with `--write`. The emitted snippet is ordinary configuration and is validated like any other.

## 13. Versioning

Version identifier: `retype-config-1`, declared at file top level. The key is mandatory in user and directory files and implicit in `builtin.toml`. A file with an unknown major fails to load; a file with a lower major is upgraded through a declared migration when one exists. Keys are never reused for different meanings, and a renamed key stays accepted as an alias for at least one major version.
