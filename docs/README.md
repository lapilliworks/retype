# Quickstart

Retype converts a file when its extension changes. Rename `photo.heic` to `photo.jpg` and the bytes follow the name.

Status is pre-alpha: some documented behaviour is still being built. Work on copies of important files and keep an independent backup.

## What you need

- A terminal
- `retype` itself. The pre-release installers fetch a release binary and update it in place: `curl -fsSL https://retype.name/install.sh | bash` on Linux and macOS, or `irm https://retype.name/install.ps1 | iex` in PowerShell on Windows. Without that, build from source with a Rust toolchain: `cargo install --git https://github.com/LapilliWorks/retype retype-cli`
- A converter for each format that needs one. ZIP and TAR repacking is native; [The default tools](#the-default-tools) lists external tools, and `retype doctor` reports which it finds

## The default tools

Retype ships the command line, the daemon and, in the installed tier, the notifier. It ships no converter: a format is read and written by a program that is already on the machine, with its own release and its own version floor.

Four tools carry the shipped formats. None of them is bundled, and none is part of macOS or Windows; a Linux desktop may already carry one, sometimes below the floor.

| Tool | Command | Floor | Carries |
|---|---|---|---|
| ImageMagick | `magick` | 7.1 | Images |
| FFmpeg | `ffmpeg` | 6.0 | Audio and video |
| pandoc | `pandoc` | 3.0 | Documents |
| 7-Zip | `7zz`, `7z` or `7za` | 23.0 | `.7z`; `zip` and `tar` are native |

Install them with the platform's package manager:

| Platform | Install |
|---|---|
| Debian, Ubuntu | `sudo apt install imagemagick ffmpeg pandoc 7zip` |
| Fedora | `sudo dnf install ImageMagick ffmpeg-free pandoc 7zip` |
| Arch | `sudo pacman -S imagemagick ffmpeg pandoc 7zip` |
| macOS | `brew install imagemagick ffmpeg pandoc sevenzip` |
| Windows | `winget install ImageMagick.ImageMagick Gyan.FFmpeg JohnMacFarlane.Pandoc 7zip.7zip` |

A package manager can hand back a build that is installed and still not enough. Fedora's `ffmpeg-free` has no `libx264`, so it can write `webm` and not `mp4`. A distribution still on ImageMagick 6 installs `convert` instead of `magick`, and is below the floor. ImageMagick needs its libheif and WebP delegates for `heic`, `avif` and `webp`. `retype doctor` reports the version and build it found, names what is missing, and [Tools](advanced/converters.md) says what each one has to be.

Two more programs are optional, because they change one result rather than every conversion. A document to `pdf` needs a PDF engine, resolved in this order: `typst`, then `xelatex`, `lualatex` or `pdflatex`, then `weasyprint`, then `wkhtmltopdf`. A `.rar` archive needs `unrar` to be read. Neither is part of the default set.

## Convert one file

```
retype c photo.heic jpg
```

`photo.jpg` appears beside the source. `c` is short for `convert`, and the second argument is the rest of the new name, so the conversion is the same thing as the rename:

```
photo.heic  ->  photo.jpg
```

Quality rides on the same name:

```
retype c photo.heic webp+q80
```

That produces `photo-quality80.webp`. The tokens after the format are the same tokens that would be typed after a dot in a file name, which is why the two gestures are one grammar. What the tokens can be is in [Quality, size and geometry](usage/dials.md) and, down to the value forms and their limits, in [Retype intent grammar](reference/intents.md).

## Ask what would happen

```
retype explain photo.heic 'webp+q80+cap1080'
```

That prints the name it would produce, the program it would run, and what the result is expected to weigh. With the file at hand, `--dry-run` on the conversion itself says the same thing, using the real source size:

```
retype c photo.heic webp+q80+cap1080 --dry-run
```

Neither writes anything. The plan it prints is the one that would run, with the program that would run it on this machine; [Fidelity](advanced/fidelity.md) explains what that program is claimed to preserve.

## Do it by renaming

Start the daemon, register a folder and check that the daemon is reachable:

```
retype daemon start
retype watch ~/Pictures
retype --no-spawn daemon status
```

`retype watch` currently registers and returns; it does not start the daemon. If the daemon is stopped, a rename can change the extension without converting the bytes. Status reports registrations and whether paths exist, but does not yet prove that each watcher is active. Allow the registration and initial scan to complete, check the daemon log for watch errors, and try a disposable copy before relying on it.

Nothing already in the folder is converted. While watching is active, renaming `photo.heic` to `photo.jpg` requests a conversion, and `photo.webp+q80` adds a quality setting. Registrations survive a restart; [Deferral](advanced/deferral.md) describes jobs that wait.

## Take it back

```
retype undo --last
```

Undo records have a one-week retention by default when a record is successfully created. Undo can be disabled or unavailable, and Windows currently refuses destructive output cleanup. Check the job and undo record rather than assuming every conversion is reversible; keep an independent backup. [Jobs, undo and history](usage/jobs.md) covers the queue and retention settings.

## The manual

Usage is the daily work: [Converting a file](usage/convert.md), [Quality, size and geometry](usage/dials.md), [Jobs, undo and history](usage/jobs.md), and [Watching a folder](usage/watch.md).

In depth answers the longer questions: [Tools](advanced/converters.md) for which converter carries which format, [Deferral](advanced/deferral.md) for why a rename sometimes waits, [Fidelity](advanced/fidelity.md) for what a tier claims, and [Provenance](advanced/provenance.md) for how retype knows what it made.

Reference is the exhaustive part: [Command line](reference/cli.md) for every flag and exit status, [Retype intent grammar](reference/intents.md) for the tokens and their limits, [Operations](reference/operations.md) and [Quality and size dials](reference/dials.md) for what each format can be asked for, [Configuration schema](reference/config.md) for every key, [Errors](reference/errors.md) for the codes, [Platforms](reference/platforms.md) for what is supported where, and [Watch scope](reference/watch.md) for every rule behind a watched folder.

Development is how retype is built and verified: [Development documentation](dev/README.md), the [Daemon and client protocol](dev/architecture/ipc.md), [State, journal and identifiers](dev/architecture/state.md), the measured [Filesystem events](dev/measurement/events.md), and [Test preparation and platform validation](dev/testing.md).
