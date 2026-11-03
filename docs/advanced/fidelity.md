# Fidelity

A conversion can preserve everything, preserve the content but re-encode it, or do whatever a tool does without retype knowing. This page explains what retype claims about each result, and when that claim stops applying.

## 1. A claim is about a pair

A fidelity claim is evidence about one source format becoming one target format through one tool and one engine:

```
(source, target, tool, tier, engine, engine version, engine fingerprint)
```

It is never a property of a format, and never a count. "Supports forty formats" says nothing about what any of those forty lose.

The engine is part of the key because two tools implementing the same pair are not interchangeable. A PDF from `typst` and a PDF from `weasyprint` are both `pdf` and both `converted`, and no user would accept one as a substitute for the other without being told.

## 2. Tiers

| Tier | Definition | Promise | Typical |
|---|---|---|---|
| `exact` | No sample was re-encoded. The container may change, the payload may not | The output holds the same samples, bit for bit | `mp4` to `mkv` remux, `tiff` to `png`, `docx` to `odt` where no feature is dropped |
| `converted` | Every sample or glyph passes through a decoding step and a re-embedding step | The content is recognisably the same; the encoding is new, and a lossy target loses what its dial says it loses | `heic` to `jpg`, `wav` to `mp3`, `md` to `pdf` |
| `delegated` | A tool retype does not model performs the conversion | That a valid file of the target format was produced. Nothing about what is inside it | `psd` through ImageMagick, `pages` through LibreOffice, anything a user declares |

Two rules follow from the definitions.

- `exact` is not a quality claim, it is an absence of lossy steps. A `png` re-encode at `compress9` is `exact`; a `jpg` at `quality100` is `converted`, because the samples were reconstructed from coefficients
- `delegated` exists so that a pair can be offered without pretending to know what it does. It is the honest answer for a converter retype did not write, and it is never silently promoted to `converted` on the grounds that the output looked fine

## 3. Reported states

Three tiers, and two states that are not tiers and never appear in configuration.

| Value | Valid in config | Meaning |
|---|---|---|
| `exact`, `converted`, `delegated` | yes | A claim, as defined above |
| `unverified` | no | A claim exists, but the installed tool reports a version or build fingerprint other than the one it was earned with |
| `unclaimed` | no | No claim exists for the pair, because the user declared the conversion themselves |

`unverified` and `unclaimed` are reported by `retype explain`, `retype why` and the job record. They are never written to a configuration file and never accepted from one.

## 4. Where claims come from

A claim is earned by running the conversion against the real tool and checking the result. `retype fidelity` performs that run, records the tool, the resolved engine, the version and the build fingerprint, and verifies that the output is readable and of the target format. The result is a manifest that ships with the release and is updated by a later run.

The same run checks a set of properties on a pair: alpha, animation, depth, embedded colour profile, EXIF orientation and palette. A pair records both the properties it kept and the ones it lost. A `converted` pair may lose one, and saying so is the point. An `exact` pair may not.

Size and duration are separate: they come from `retype bench`, not from the fidelity claim, because they depend on the machine and the claim does not.

`delegated` is the answer for a pair no manifest describes: a converter retype did not write, or a format offered because a tool accepts it. There is no fixture to run and nothing to claim, so the tier says what is true, which is that a valid file of the target format was produced.

## 5. Where a claim is shown

| Surface | Use |
|---|---|
| `retype list --tiers` | Prints the claims, per pair, with engine, version and fingerprint |
| `retype explain` | Prints the tier the plan would produce |
| `retype why` | Prints the claim, the engine, and whether it is current |
| `retype doctor` | Reports the manifest version, each tool source, and how many installed tools differ from the versions and fingerprints their claims were earned with |
| Editor badge | The tier, with `unverified` shown as such rather than hidden |

## 6. When a claim goes stale

Evidence is about a version and a build. When the installed tool reports a different version or fingerprint from the one recorded:

- The pair still converts. A distribution's older ImageMagick must not turn an install into a product that refuses to work
- The tier is reported `unverified`, and `why` names both versions and both fingerprints
- A pair that declares `exact` fails with `E_UNSUPPORTED` and a hint naming the version, the fingerprint and the declaration to relax. A promise of no loss cannot rest on evidence about a different encoder, and silently downgrading a promise is worse than refusing it

Staleness never silently upgrades either. A tool newer than the recorded one is still unverified, because a new version changing encoder defaults is exactly how a fidelity claim goes stale.

## 7. What is not a claim

Three things wear the same vocabulary and are not fidelity.

- `confidence` in [Provenance](provenance.md) is `exact`, `path-only` or `none`, and describes how a write was matched to an observed rename. It says nothing about an output's quality
- The dials in [Quality and size dials](../reference/dials.md) move within a tier. `jpg+quality100` and `jpg+quality10` are both `converted`; the dial does not reach `exact`
- The format list is capability, not fidelity. A format retype can write is not a pair it has evidence for

## 8. Claims for this release

Generated between the markers by `retype fidelity --render`, one line per claim, sorted by source, target and tool. The list is empty in the source tree until the first release run. The fingerprint is shortened here and kept whole in the manifest.

<!-- claims:start -->
<!-- claims:end -->
