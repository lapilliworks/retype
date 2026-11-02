# Release packaging

The source build in `README.md` is a workaround. A release is the installed tier:
the CLI and daemon, the notifier, URI registration and a user service, with no
external tool binaries.

## Asset layout

Every archive contains `bin/retype` (`retype.exe` on Windows) and, when the
release run has earned them, these shared assets:

```
share/retype/fidelity.toml
share/retype/fixtures/fidelity/
```

`retype why` and `retype fidelity` look in `<prefix>/share/retype`; a packaged
build therefore reads the shipped claims rather than a source tree. The manifest
is produced by:

```
retype fidelity --write share/retype/fidelity.toml --render docs/fidelity.md
```

The release run must happen on the build machine whose tool builds are recorded
in the manifest. A manifest earned elsewhere is reported `unverified`.

## Per platform

| Platform | Files | What installs them |
|---|---|---|
| Linux | `linux/retype.service`, `linux/retype-uri.desktop` | The package's systemd/user and xdg-mime hooks |
| macOS | `macos/Info.plist`, `macos/com.lapilliworks.retype.plist` | The app bundle and launchd |
| Windows | `windows/retype.manifest`, `windows/retype.rc` | The executable's manifest resource and the Start-menu shortcut |

The launchd plist and the systemd unit name the install prefix as `@PREFIX@`; the package substitutes it before installing.

Signing is not optional for a published binary. Linux ships a detached minisign
or Sigstore signature beside the checksum. macOS ships a Developer ID signed,
hardened, notarised archive. Windows ships an Authenticode-signed executable with
a secure timestamp. The notifier bundle is the first artifact that needs the
platform identity, and it is signed before the CLI.

## Rename or move

The daemon is local and per user. There is no system service and no privileged
helper. The launchd agent and systemd unit run in the user session, not as root.
