# Tools

Retype converts nothing by itself. Every conversion is carried out by an external program that already exists on the machine, and this page is about those programs: which ones retype expects, where it looks for them, how it calls them, and how to add, change or remove one.

## 1. The default tools

| Tool | Command | Floor | Carries |
|---|---|---|---|
| ImageMagick | `magick` | 7.1 | Images |
| FFmpeg | `ffmpeg` | 6.0 | Audio and video |
| pandoc | `pandoc` | 3.0 | Documents |
| 7-Zip | `7zz`, `7z` or `7za` | 23.0 | `.7z`; `zip` and `tar` are handled natively |

A tool is installed with the platform's package manager and found on `PATH`, or managed by the GUI. [Quickstart](../README.md) has the install commands. `retype doctor` reports what is present, its version and its build, and names any component a format needs but the build lacks.

For ImageMagick or FFmpeg JPEG output, independent output validation currently uses both `magick` and `ffmpeg` from `PATH`, regardless of the configured encoder path. If either is unavailable, the original is retained and the job and daemon log identify the missing validator. Validation checks readability, not fidelity.

The defaults are not the whole story. A tool can be installed and still not carry a format, because the feature belongs to a build-time component rather than to the program:

| Tool | What the build must have | Formats affected |
|---|---|---|
| ImageMagick | libheif, libwebp and the other delegates for the format | `heic`, `avif`, `webp` |
| FFmpeg | The encoder the shipped target names, such as `libx264` or `libmp3lame` | `mp4`, `mkv`, `mov`, `mp3` |
| pandoc | A PDF engine | `pdf` output only |
| 7-Zip | The unRAR code, or a separate `unrar` | `rar` input only |

Fedora's `ffmpeg-free` has no `libx264`, so it produces `webm` but not `mp4`. A distribution still on ImageMagick 6 installs `convert` rather than `magick`, which is below the floor. `unrar` is not distributed with retype, so reading a `.rar` archive needs the user to install it.

Two programs are optional because they change one result rather than every conversion. A document to `pdf` uses a PDF engine, and a `.rar` archive needs `unrar`. Neither is part of the default set.

## 2. Where a tool comes from

A tool can be managed or supplied by the system.

| Source | Where it lives | Who puts it there | What retype knows |
|---|---|---|---|
| Managed | `<state>/tools` | The GUI, from a signed distribution list | The exact artifact, its hash, its build and its version |
| System | `PATH`, or an absolute path in configuration | The user's package manager, or the user by hand | What the probe reports when `retype doctor` runs |

The command line does not install, update or verify tools. It uses a system tool, an absolute path the user pinned, or a managed set the GUI already installed. The GUI offers the managed set to users who do not have a package manager or do not want to use one, and it verifies each artifact against its manifest before use. An offline pack is the same artifacts and manifest on disk, for machines with no network or a controlled one.

A managed artifact has a known fingerprint. A system tool has the version and build its own probe reports, which can change with a package update. That difference matters when a format needs a fidelity guarantee: `retype why` reports a claim as `unverified` when the installed build is not the one the claim was earned with, as described in [Fidelity](fidelity.md).

Resolution order is fixed:

1. An absolute `command` in `[tools.<name>]` is used as written and pins the user's choice
2. A bare command is looked up in `<state>/tools/bin`, then on `PATH`
3. If the command is absent or unusable, its declared `aliases` are tried in order

Nothing is downloaded during a conversion. A missing tool is an environment failure that names the package, the variant and the command that would install it. A download happens only when the user asks for one, during GUI setup or later through the GUI.

## 3. Declaring and changing a tool

Tools are configuration, not compiled-in facts. Each one is declared under `[tools]`, so a user can add a tool, raise a version floor, change its command or replace its argument layout without a new build.

```toml
version = 'retype-config-1'

[tools.magick]
command = 'magick'
probe = ['-version']
version_min = '7.1'
purpose = 'image conversion'
components = ['heic', 'webp']
layout = ['{input}', '{ops}', '{output}']
```

| Key | Required | Meaning |
|---|---|---|
| `command` | yes | Executable, absolute or a bare name. No shell metacharacters or spaces |
| `aliases` | no | Other executable names for the same tool, tried in order |
| `probe` | no | Argv run to report a version, by `retype doctor` and once at plan time |
| `version_min` | no | Minimum accepted version, compared against the probe output |
| `purpose` | no | One line, printed by `retype doctor` |
| `components` | no | Substrings the probe output must mention, such as `heic` or `libx264` |
| `families` | no | Format families this tool can carry: `image`, `audio`, `video`, `document`, `archive`, `folder` |
| `layout` | yes | Argv template naming `{input}`, `{ops}` and `{output}` once each, in any order |
| `engines` | no | Named alternatives resolved at plan time, such as a PDF engine |

A tool that declares no `probe` and no `version_min` makes no version claim, so a local script is usable as it stands and is not run merely to find that out. A tool that declares `families` is chosen only when both the source and the target family are among them; a tool that declares none is eligible for anything, which is what a user-written tool gets.

`aliases` are other names for the same executable, tried in declared order when `command` is absent or unusable. They change neither the tool's name nor its `[tools.<name>]` key, so every `argv` fragment and `[backends]` entry still names the declared tool. An absolute `command` suppresses the aliases. A candidate below its version floor, or missing a required component, is skipped rather than shadowing a usable alias, and `retype doctor` reports which candidate answered.

To remove a tool, set `enabled = false` on its entry or remove it. Everything that named it goes with it: its place in `backends.prefer`, every `argv` fragment keyed by it, and any benchmark sample that used it. A name that was never declared remains an error, so a typo is not silently ignored.

A watched directory cannot define a tool. Its `.retype.toml` carries policy only, because a directory can arrive from an untrusted source and a command is not something it may introduce.

### 3.1 Preference and selection

`[backends]` decides which tool is tried first:

```toml
[backends]
prefer = ['magick', 'ffmpeg', 'pandoc', '7zz']
```

`prefer` names tools and nothing else. A file that omits the table uses every declared tool in name order. The first tool in the list that implements every operation in the plan and is available wins. Availability is decided at plan time: the tool is present, above its version floor, and carrying its components. The chosen tool, its resolved engine and its fingerprint are recorded with the job, so a later change to the machine does not silently change what a recorded job used.

Archive formats are not one backend. `zip`, `tar` and `tar.gz` are handled natively, and `7zz` carries `.7z` only.

## 4. How a tool is called

The `layout` is the argv template. It names `{input}`, `{ops}` and `{output}` once each, in any order, because tools disagree about where each belongs: `7zz` takes its output before its input, and `ffmpeg` needs `-i` in front of its input.

`{ops}` expands in two parts, in this order:

1. The format's own fragment for that tool, such as the encoder and container flags for `mp4`
2. The operation fragments, in the canonical slot order described in [Retype intent grammar](../reference/intents.md) §10

```toml
[formats.mp4.argv.ffmpeg]
args = ['-c:v', 'libx264', '-pix_fmt', 'yuv420p', '-movflags', '+faststart']

[ops.quality.argv.magick]
args = ['-quality', '{value}']
```

A placeholder that occupies a whole argument is replaced by a list; a placeholder inside an argument is replaced by a string. `{input}`, `{ops}` and `{output}` belong to the layout, and `{value}` and `{engine.<name>}` belong to a fragment. A fragment naming a layout hole, or a layout naming a fragment hole, is a configuration error rather than a literal brace in the argv.

Every operation reaches a process this way. There is no in-process backend and no plan that carries code instead of an argv. `retype explain` prints the command line that would run, which is the quickest way to see how a tool is called.

## 5. Engines

Some tools delegate part of the work to another program, and which one is present changes the result. A PDF engine is the common case. An engine is a named list of candidate commands, resolved first present:

```toml
[tools.pandoc]
command = 'pandoc'
layout = ['{input}', '{ops}', '-o', '{output}']
engines = { pdf = ['typst', 'xelatex', 'lualatex', 'pdflatex', 'weasyprint', 'wkhtmltopdf'] }

[formats.pdf.argv.pandoc]
args = ['--pdf-engine', '{engine.pdf}']
```

The order sets the default: `typst` first, then a TeX engine, then `weasyprint`, then `wkhtmltopdf`. A preset that needs a particular engine pins it by naming it literally in the argv, which bypasses resolution and fails with `E_TOOL_MISSING` when the named engine is absent.

When no candidate is present, the plan fails with `E_TOOL_MISSING` and names every candidate, so the message says what to install. The resolved engine is recorded with the job and reported by `retype why`, because two PDFs from the same intent are not interchangeable across engines.

`typst` is a single binary and can be part of the managed set. `weasyprint` carries a Python and GTK stack and is never managed; it belongs to the system.

## 6. What each tool is good for

The following constraints come from the tools themselves. They are the reason a format can be listed and still fail on a particular machine.

### 6.1 Images

- `ico` requires both dimensions to be at most 256. A plan that would produce a larger icon is refused with a hint to add `cap256`, rather than left to the encoder to fail
- `dng` is read-only, so it cannot be a target
- `svg` output uses ImageMagick's internal XML renderer when librsvg is absent. Text and CSS support differ from a librsvg build
- `tiff` is uncompressed unless a compression level is passed, which is why `tiff` has a `compress` dial and no quality dial

### 6.2 Documents

- pandoc writes `docx`, `html`, `odt`, `epub`, `rst`, `tex`, `typst`, `markdown` and many more, and writes `pdf` through an engine. It does not read `pdf`, so `pdf` cannot be a source
- `html` output uses `--standalone`, so the result is a document rather than a fragment
- With WeasyPrint as the engine, output is web-grade rather than TeX-grade: math needs an engine that supports it, cross-references and citation styles differ, and page breaking follows CSS. `pdf` is therefore `converted` at best, never `exact`
- WeasyPrint can print CSS warnings on stderr while succeeding. Success is decided by exit status, and the warnings are kept in the job log rather than treated as errors

### 6.3 Audio and video

The shipped targets name their encoder rather than leaving the choice to FFmpeg's default, so a missing encoder is a tool failure at the first conversion rather than a silent substitute. A build configured without `libx264` cannot run `mp4`, `mkv` or `mov`; one without `libmp3lame` loses `mp3`.

A user whose build differs can override the argv fragment for that format in configuration. The fragment is the whole difference between one build and another.

`trim` is not expressible yet: its value is a range, and a fragment has one `{value}` hole.

### 6.4 Archives

`zip`, `tar` and `tar.gz` are written by retype itself. `.7z` is written through `7zz`, `7z` or `7za`. A `.rar` archive is read through `unrar` and is never written.

Repacking an archive is a conversion: the entries keep their names, bytes and permissions, and only the container changes. An archive is never unpacked into a directory and a loose file is never compressed on its own; both are operations a file manager already offers, and both are destructive in a way a conversion is not.

The rules below apply when retype writes an archive, whether natively or through a tool:

- An entry name must be relative and free of `..`. Backslashes are read as separators before that check, so a name that would escape under either convention is refused rather than copied into another container
- A symbolic link refuses the archive, because carrying one turns a repack into a traversal for whoever unpacks it next
- Directories, files and their permission bits are carried; devices, FIFOs and anything else are refused by name
- Every entry's declared size is checked against a budget before a byte is copied: 100,000 entries and 8 GiB of declared expansion
- A PAX size override is authoritative over the raw header, and the bytes actually read must match it
- Names are written with `/` separators, so a zip made on Windows becomes a tarball a Unix can read
- Entries keep the order the source lists them, so repacking one archive twice produces the same bytes
- The staged output is reopened before publication and compared with the source inventory: every entry's name, kind, size and SHA-256 digest must agree
- The output is renamed into place only after that check, so a cancelled or failed repack leaves no partial archive

A repack through a tool keeps the same rules. The source is listed before it is unpacked, so names, entry kinds and the byte budget are judged on the listing. The entries are materialised into a staged directory beside the output, which is removed when the job ends. Through a tool the entry order is the tree's own, sorted by name, because the tool decides what order it writes.

## 7. Checking a machine

```
retype doctor
retype doctor --emit
retype list --tiers
retype explain photo.heic 'webp+quality80'
```

`doctor` reports every declared tool, the command that answered, its version, its components, which pairs are unavailable, and the source of each tool. `--emit` prints a `[tools]` snippet pinning what this machine resolved, which is how a setup is made reproducible across machines.

Each row can also be checked directly, without trusting this page:

```
magick -version
magick -list format
magick -list delegate
ffmpeg -hide_banner -version
ffmpeg -hide_banner -encoders
pandoc --list-input-formats
pandoc --list-output-formats
command -v typst xelatex lualatex pdflatex weasyprint wkhtmltopdf
7zz i                 # or `7z i`, `7za i`
```

`retype doctor` runs the probes a tool declares. A tool that declares no probe is never run merely to find out about it, because starting a converter is work in itself.
