# Retype

Rename the extension, the file converts.

Status: pre-alpha. The engine is being rebuilt from specification; see and `docs/`.

## What it will be

A file watcher and a command line tool that convert a file when you rename its
extension. `photo.heic` becomes `photo.jpg`, and the bytes follow the name.
Transformations ride on the same gesture: `photo.heic` becomes `photo.webp+q80`.

- Cross platform: Linux, macOS, Windows
- Local only: no upload, no account, no telemetry
- Scriptable, with the same intent string accepted by the filename, the command
  line and the editor

## Documentation

[`docs/`](docs/README.md) is the user manual: the intent grammar, operations, configuration, command line, and what each conversion preserves. [`docs/dev/`](docs/dev/README.md) holds the internal contracts, filesystem measurements, and the commands for building and testing.


## Installation tiers

Two tiers exist, and the difference is deliberate.

- **Installed**: platform installer, signed. Command line, daemon, notifier,
  URI scheme, user service. External tools are acquired separately. Supported
- **Source**: `cargo install --git https://github.com/justretype/retype`. The
  binary includes the daemon, but no service, URI registration or notifier
  bundle is installed. A workaround, not the supported path

Tool acquisition follows the interface. The CLI does not install or manage tools: a
CLI user uses the platform's package manager or installs them by hand. The GUI manages
the signed toolchain for users who should not need to know what a package manager is.

## Licence

ISC. See `LICENSE`. The graphical application is a separate, proprietary
product and lives in its own repository.
