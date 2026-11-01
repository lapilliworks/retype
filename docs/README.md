# Quickstart

Retype converts a file when its extension changes. Rename `photo.heic` to `photo.jpg` and the bytes follow the name.

Status is pre-alpha: part of what is described here is specified and not built yet.

## What you need

- A terminal
- `retype` itself. There is no signed installer yet, so it is built from source, which needs a Rust toolchain: `cargo install --git https://github.com/justretype/retype retype-cli`
- A converter for each format you use. Retype converts nothing by itself; it drives programs that are already on the machine, ImageMagick for images, FFmpeg for audio and video, Pandoc for documents. `retype doctor` reports which it finds, and [Tools](advanced/converters.md) says what each of them has to be

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

Register a folder once, then rename inside it as usual, from any file manager:

```
retype watch ~/Pictures
```

Nothing already in the folder is touched. From then on, renaming `photo.heic` to `photo.jpg` in that folder converts it, and renaming it to `photo.webp+q80` converts it with a quality setting. Registrations survive a restart, and a rename that would take a long time stops and waits rather than running unattended, which is what [Deferral](advanced/deferral.md) is about.

## Take it back

```
retype undo --last
```

Every conversion keeps what it needs in order to be reversed for a week by default, including the source bytes when the rename is what consumed the source name. [Jobs, undo and history](usage/jobs.md) covers the queue, the settings behind that week, and what happens when a record has aged out.

## The manual

<!-- toc:start -->

### Usage

| Page | Contents |
|---|---|
| [Converting a file](usage/convert.md) | Several outputs at once, naming, overwriting, and what happens on failure |
| [Quality, size and geometry](usage/dials.md) | The dials, the geometry operations, and the order they run in |
| [Jobs, undo and history](usage/jobs.md) | The queue, logs, undo, and the jobs that wait for an answer |
| [Watching a folder](usage/watch.md) | What is ignored, what is refused, pausing, and what you are told |

### In depth

| Page | Contents |
|---|---|
| [Tools](advanced/converters.md) | Which program does which format, and what each one has to be |
| [Deferral](advanced/deferral.md) | Why a rename sometimes parks a job instead of running it |
| [Fidelity](advanced/fidelity.md) | The three tiers, and the evidence behind each claim |
| [Provenance](advanced/provenance.md) | The record that answers `retype why` |

### Reference

| Page | Contents |
|---|---|
| [Command line](reference/cli.md) | Every command, its arguments, and its exit status |
| [Retype intent grammar](reference/intents.md) | The tokens, the values they take, and the limits |
| [Operations](reference/operations.md) | Every operation, where it applies, and the presets |
| [Quality and size dials](reference/dials.md) | Quality, effort and compression ranges per format |
| [Configuration schema](reference/config.md) | Every configuration key, and the policy a folder may set |
| [Errors](reference/errors.md) | Codes, categories and message rules |
| [Platforms](reference/platforms.md) | Supported systems, filesystems and delivery |

### Development

| Page | Contents |
|---|---|
| [Development documentation](dev/README.md) | How retype is built and verified, and the commands for both |
| [Daemon and client protocol](dev/architecture/ipc.md) | The protocol between the two processes |
| [State, journal and identifiers](dev/architecture/state.md) | State directory, journal, undo store, identifiers, calibration |
| [Filesystem events](dev/measurement/events.md) | What each platform reports for a rename (generated) |

<!-- toc:end -->
