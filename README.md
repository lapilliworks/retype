# Retype

Rename the extension, the file converts.

Status: pre-alpha. The manual marks which behaviour has landed.

## What it will be

A file watcher and a command line tool that convert a file when you rename its
extension. `photo.heic` becomes `photo.jpg`, and the bytes follow the name.
Transformations ride on the same gesture: `photo.heic` becomes `photo.webp+q80`.

- Cross platform: current Debian stable on Linux, macOS, Windows
- Local only: no upload, no account, no telemetry
- Scriptable, with the same intent string accepted by the filename, the command
  line and the editor

## Documentation

[`docs/`](docs/README.md) is the user manual: the intent grammar, operations, configuration, command line, and what each conversion preserves. [`docs/dev/`](docs/dev/README.md) holds the internal contracts, filesystem measurements, and the commands for building and testing.

## Installation tiers

Two tiers exist, and the difference is deliberate. Pre-release installers are a
convenience on top of the source tier, not a third tier.

- **Installed**: platform installer, signed. Command line, daemon, notifier,
  URI scheme, user service. External tools are acquired separately. Supported
- **Source**: `cargo install --git https://github.com/LapilliWorks/retype retype-cli`. The
  binary includes the daemon, but no service, URI registration or notifier
  bundle is installed. A workaround, not the supported path

Linux, macOS and Windows can install the alpha release without a Rust toolchain.

Linux and macOS:

```sh
curl -fsSL https://retype.name/install.sh | bash
```

Windows, in PowerShell:

```powershell
irm https://retype.name/install.ps1 | iex
```

The scripts verify the release checksum, install the CLI, daemon and shared release assets under `~/.local` on Unix or `%LOCALAPPDATA%\Programs\retype` on Windows, and update that installation when run again. The Windows installer adds its `bin` directory to the user `PATH`. The default release selection includes prereleases. On Linux, the installer requires Python 3 and `xdg-mime`, installs the notification/URI desktop entry and a disabled user-service unit, and registers `retype://`. Linux desktop support is Wayland-only. Run the same installer with `--uninstall` to remove that installation while keeping configuration and recovery data. macOS and Windows pre-release integration is unchanged. Publisher signing remains required for the installed tier.

Updates validate the staged binary before publication, retain the previous installation in a `share/retype-backup.*` or `share/retype-backup-*` directory under the prefix, and restore it on installation failure. The scripts print the backup path; old backups are not automatically pruned.

Tool acquisition follows the interface. The CLI does not install or manage tools: a
CLI user uses the platform's package manager or installs them by hand. The GUI manages
the signed toolchain for users who should not need to know what a package manager is.

## Tests

See the [native test harness](tests/README.md) for preparation, `./tests/run.sh` on Linux/macOS, the Windows runner, and report locations. The strict runner requires real tools; ordinary `cargo test --workspace` is a contributor baseline, not release evidence.

## Licence

ISC. See `LICENSE`. The graphical application is a separate, proprietary
product and lives in its own repository.
