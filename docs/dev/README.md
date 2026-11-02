# Development documentation

These documents specify how `retype` is built and verified. Behaviour a user can observe belongs in the [user documentation](../README.md). ## Contracts

| Document | Contents |
|---|---|
| [State, journal and identifiers](architecture/state.md) | The state directory, journal records, the undo store and calibration |
| [Daemon and client protocol](architecture/ipc.md) | Requests, responses and the job lifecycle the client sees |

## Measurements

| Document | Contents |
|---|---|
| [Filesystem events](measurement/events.md) | What each platform reports for a rename (generated) |
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

Optional external-tool guards report missing prerequisites, but their early returns still count as passes in Rust's test harness. For encoder verification, install the prerequisites and set `RETYPE_REQUIRE_TEST_TOOLS=1` so the CLI codec guards fail instead. See [test preparation and platform validation](testing.md) for tool versions, PDF engines, filesystem requirements, native Windows commands and remaining coverage limits.

When a failure depends on the platform, `scripts/probe-supersede.sh` reports the stage it broke at on the host that fails: delivery, batching, correlation, a running job holding the source, or the cancellation. It runs the same gesture as `tests/supersede.rs` in a temporary tree with the daemon's own log, so a report can be attached to an issue without the test harness.

## Generated documents

`measurement/events.md` is produced by the probe, and the claims table in [Fidelity](../advanced/fidelity.md) is produced by a fidelity run:

```
retype fidelity --write share/retype/fidelity.toml --render docs/advanced/fidelity.md
```

Edit the probe or the fixtures, not the generated output.
