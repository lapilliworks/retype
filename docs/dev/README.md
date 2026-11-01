# Development documentation

These documents specify how `retype` is built and verified. Behaviour a user can observe belongs in the [user documentation](../README.md). ## Contracts

| Document | Contents |
|---|---|
| [`architecture/state.md`](architecture/state.md) | State directory, journal, undo store, identifiers, calibration |
| [`architecture/ipc.md`](architecture/ipc.md) | Daemon and client protocol |

## Measurements

| Document | Contents |
|---|---|
| [`measurement/events.md`](measurement/events.md) | What each platform reports for a rename (generated) |
| `measurement/probe/` | Raw probe transcripts, named `<platform>-<date>.txt` |

`events.md` is generated from the probe, not written by hand:

```
cargo run -p retype-watch --example probe_events
```

The probe lives in `crates/retype-watch/examples/probe_events.rs`. CI runs it on Linux, macOS and Windows and uploads each transcript as an artifact, so every claim in the document can be checked against the run that produced it.

## Build and test

Four checks, and the last two need a toolchain other than the default one.

```
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace

# the floor, which is not what `rustup` installs by default
rustup toolchain install 1.85.0 --profile minimal --component clippy
cargo +1.85.0 clippy --workspace --all-targets -- -D warnings
cargo +1.85.0 test --workspace

# the platforms we do not build on here
rustup target add x86_64-pc-windows-msvc aarch64-apple-darwin x86_64-unknown-freebsd
cargo check --workspace --all-targets --target x86_64-pc-windows-msvc
cargo check --workspace --all-targets --target aarch64-apple-darwin
cargo check --workspace --all-targets --target x86_64-unknown-freebsd
```

`cargo check` is enough for a foreign target: no linker is involved, and it is what CI does. Clippy at the floor matters as much as clippy on stable, because a lint that exists in 1.85.0 and not in a later release fails only there.

## Generated documents

`measurement/events.md` is produced by the probe, and the claims table in [`fidelity.md`](../advanced/fidelity.md) is produced by a fidelity run:

```
retype fidelity --write share/retype/fidelity.toml --render docs/fidelity.md
```

Edit the probe or the fixtures, not the generated output.
