# Release packaging

The source build in `README.md` is a workaround. A release is the installed tier:
the CLI and daemon, the notifier, URI registration and a user service, with no
external tool binaries. Until signing lands, the release workflow also publishes
a pre-release archive and a `curl | bash` installer for the CLI and daemon only.

## Asset layout

Every archive is named `retype-<tag>-<target>.tar.gz` and contains `bin/retype`
(`retype.exe` on Windows) and, when the release run has earned them, these shared
assets:

```
share/retype/fidelity.toml
share/retype/fixtures/fidelity/
share/retype/packaging/
```

`retype why` and `retype fidelity` look in `<prefix>/share/retype`; a packaged
build therefore reads the shipped claims rather than a source tree. The manifest
is produced by:

```
retype fidelity --write share/retype/fidelity.toml --render docs/advanced/fidelity.md
```

The release run must happen on the build machine whose tool builds are recorded
in the manifest. A manifest earned elsewhere is reported `unverified`.

## Release pipeline

The release workflow builds every supported target from `docs/reference/platforms.md`:

| Platform | Target | Build host |
|---|---|---|
| Linux x86_64 | `x86_64-unknown-linux-gnu` | Debian 11 container, glibc 2.31 |
| Linux aarch64 | `aarch64-unknown-linux-gnu` | Debian 11 container on an arm64 runner, glibc 2.31 |
| macOS x86_64 | `x86_64-apple-darwin` | `macos-13` |
| macOS aarch64 | `aarch64-apple-darwin` | `macos-14` |
| Windows x86_64 | `x86_64-pc-windows-msvc` | `windows-latest` |
| Windows aarch64 | `aarch64-pc-windows-msvc` | `windows-latest`, cross compiled |

A tag push publishes the archives, per-archive `.sha256` files, `SHA256SUMS`,
`install.sh`, `install.ps1`, and GitHub build attestations. A manual dispatch
builds the same archives without creating a release. The tag must match the
workspace version in `Cargo.toml`.

## Script install

`install.sh` is the `curl | bash` installer and updater for Linux and macOS.
`install.ps1` is the PowerShell installer and updater for Windows. Both resolve
the latest release tag through the GitHub API, download the matching archive and
`SHA256SUMS`, verify the digest, and install the binary and shared assets.
`install.sh` uses `~/.local` by default; `install.ps1` uses
`%LOCALAPPDATA%\Programs\retype` and adds its `bin` directory to the user
`PATH`. Running either again updates in place.

```sh
curl -fsSL https://justretype.com/install.sh | bash
curl -fsSL https://justretype.com/install.sh | bash -s -- --version v0.1.0
curl -fsSL https://justretype.com/install.sh | bash -s -- --prefix "$HOME/.local"
```

```powershell
irm https://justretype.com/install.ps1 | iex
$env:RETYPE_VERSION = 'v0.1.0'; irm https://justretype.com/install.ps1 | iex
$env:RETYPE_PREFIX = 'C:\Tools\retype'; irm https://justretype.com/install.ps1 | iex
```

The scripts are copied into the product site at build time and are also release
assets. They do not install a user service, register the URI scheme or ship the
notifier.

## Per platform

| Platform | Files | What installs them |
|---|---|---|
| Linux | `share/retype/packaging/linux/retype.service`, `share/retype/packaging/linux/retype-uri.desktop` | The package's systemd/user and xdg-mime hooks |
| macOS | `share/retype/packaging/macos/Info.plist`, `share/retype/packaging/macos/com.lapilliworks.retype.plist` | The app bundle and launchd |
| Windows | `share/retype/packaging/windows/retype.manifest`, `share/retype/packaging/windows/retype.rc` | The executable's manifest resource and the Start-menu shortcut |

The launchd plist and the systemd unit name the install prefix as `@PREFIX@`; the package substitutes it before installing.

Signing is not optional for a published binary in the installed tier. Linux ships
a detached minisign or Sigstore signature beside the checksum. macOS ships a
Developer ID signed, hardened, notarised archive. Windows ships an
Authenticode-signed executable with a secure timestamp. The notifier bundle is
the first artifact that needs the platform identity, and it is signed before the
CLI.

The pre-release scripts are outside that guarantee. Their archives carry a
SHA-256 checksum and a GitHub build attestation. The installers reject a
corrupted download; the attestation can be checked out of band with
`gh attestation verify <archive> --repo LapilliWorks/retype`. Neither is a code
signature. They are a pre-alpha convenience, not the installed tier. Signing
steps are inserted into the same workflow before that tier is published.

## Rename or move

The daemon is local and per user. There is no system service and no privileged
helper. The launchd agent and systemd unit run in the user session, not as root.
