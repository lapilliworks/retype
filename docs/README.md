# Retype documentation

Retype converts a file when you rename its extension. `photo.heic` becomes `photo.jpg`, and transformations ride on the same gesture: `photo.heic` becomes `photo.webp+q80`.

Status is pre-alpha, so some documented behaviour is specified but not built yet. The repository's records what has landed, and its README explains what the product is and how it is installed.

## Writing an intent

| Document | Contents |
|---|---|
| [`grammar.md`](grammar.md) | The intent grammar: what a file name may carry |
| [`operations.md`](operations.md) | Every operation, its parameter and where it applies |
| [`quality.md`](quality.md) | What `quality`, `effort` and `compress` mean per format |

## Running it

| Document | Contents |
|---|---|
| [`cli.md`](cli.md) | Commands, flags, output and exit codes |
| [`config.md`](config.md) | User and directory configuration, precedence and validation |
| [`watch.md`](watch.md) | What is watched, what is ignored, and what happens at startup |
| [`deferral.md`](deferral.md) | When a job waits or asks for attention instead of running |

## Results and failures

| Document | Contents |
|---|---|
| [`fidelity.md`](fidelity.md) | Tiers, how a claim is earned, and the claims for this release |
| [`provenance.md`](provenance.md) | What `retype why` can tell you, and how far the record travels |
| [`errors.md`](errors.md) | Error codes, categories and diagnostics |

## Tools and platforms

| Document | Contents |
|---|---|
| [`tools.md`](tools.md) | External tools and the formats each build supports |
| [`platforms.md`](platforms.md) | Supported targets, filesystems and delivery tiers |

## Development

[`dev/`](dev/README.md) holds the internal contracts, the filesystem measurements, and the commands for building and testing.
