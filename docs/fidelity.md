# Fidelity

What a conversion may be claimed to preserve, and how that claim is earned.

## 1. A claim is about a pair

A fidelity claim is evidence about one source format becoming one target format through one backend. It is never a property of a format, and never a count: "supports forty formats" is not a statement this product makes, because it says nothing about what any of those forty lose.

```
(source, target, backend, tier, engine, engine version, engine fingerprint)
```

The engine is part of the key because two backends implementing the same pair are not interchangeable. A PDF from `typst` and a PDF from `weasyprint` are both `pdf` and both `converted`, and no user would accept one as a substitute for the other without being told. The fingerprint is part of it because one version can be built with different codecs; `docs/tools.md` §5.1 records both with the job for the same reason.

## 2. Tiers

| Tier | Definition | Promise | Typical |
|---|---|---|---|
| `exact` | No sample was re-encoded. The container may change, the payload may not | The output holds the same samples, bit for bit | `mp4` to `mkv` remux, `tiff` to `png`, `docx` to `odt` where no feature is dropped |
| `converted` | Every sample or glyph passes through a decoding step and a re-embedding step | The content is recognisably the same; the encoding is new, and a lossy target loses what its dial says it loses | `heic` to `jpg`, `wav` to `mp3`, `md` to `pdf` |
| `delegated` | A tool we do not model performs the conversion | That a valid file of the target format was produced. Nothing about what is inside it | `psd` through ImageMagick, `pages` through LibreOffice, anything a user declares |

Two rules follow from the definitions.

- `exact` is not a quality claim, it is an absence of lossy steps. A `png` re-encode at `compress9` is `exact`; a `jpg` at `quality100` is `converted`, because the samples were reconstructed from coefficients
- `delegated` exists so that a pair can be offered without pretending to know what it does. It is the honest answer for a converter we did not write, and it is never silently promoted to `converted` on the grounds that the output looked fine

## 3. Reported states

Three tiers, and two states that are not tiers and never appear in configuration. `docs/config.md` V10 accepts only the three.

| Value | Valid in config | Meaning |
|---|---|---|
| `exact`, `converted`, `delegated` | yes | A claim, as defined above |
| `unverified` | no | A claim exists, but the installed engine reports a version or build fingerprint other than the one it was earned with |
| `unclaimed` | no | No claim exists for the pair, because the user declared the conversion themselves |

`unverified` and `unclaimed` are reported in `plan`, `why` and the journal. They are never written to a configuration file and never accepted from one.

## 4. Where claims come from

A claim is earned by running the conversion.

A fixture run converts a fixture per pair with the real tool, records the resolved engine, its reported version and its build fingerprint, and verifies the output is readable and of the target format. The result is a manifest, one record per claim, checked in and shipped with the release. `docs/tools.md` §5.1 records the same engine, version and fingerprint with every job, so a claim made two years ago can be compared against what is installed now.

`retype fidelity` is that run, `docs/cli.md` §10. A pair is checked by converting the sample `[bench.sample]` declares for its source family, so a claim records what it was earned on.

That sample proves the conversion succeeds. What turns it into "the tool preserved these properties" is the fixtures in `fixtures/fidelity`: one tiny checked-in file per property, converted through the same plan and probed on both sides. `fixtures/fidelity/fixtures.toml` declares which property each file carries, and a test probes every one of them, so a fixture whose bytes stop carrying its axis fails rather than quietly weakening a claim.

| Axis | Fixture | Preserved when |
|---|---|---|
| `alpha` | `alpha.png` | The output is not fully opaque |
| `animation` | `anim.gif`, `anim.webp` | Every frame is still there |
| `depth` | `depth.png` | Bits per channel do not shrink |
| `icc` | `icc.png` | The embedded profile is still embedded |
| `orientation` | `orient.jpg` | The EXIF rotation is baked into the pixels *or* kept as a tag, never both |
| `palette` | `palette.png` | The output invents no colour the source did not have |

A pair records both lists: the properties it kept and the ones it lost. A `converted` pair may lose one, and saying so is the point. An `exact` pair may not: a run that cannot show the property was kept has not earned the word, and exits 4, which is the same answer as a stale claim and for the same reason.

The properties are read from the bytes where the decoder does not report them. ImageMagick surfaces geometry, depth, opacity, frame count and colour count; it does not surface the EXIF orientation tag or, reliably, an embedded profile, so both are read directly: APP1 and APP2 segments in JPEG, `iCCP` and `eXIf` chunks in PNG, `ICCP` in WebP, tag 34675 in TIFF. An axis that cannot be read cannot be checked, and guessing would be worse than saying nothing.

Size and duration are separate: they come from `retype bench`, not from the manifest, because they depend on the machine and the claim does not, `docs/state.md` §4.

A pair the user declares in their own `[conversions.*]` table cannot have evidence, so it is `unclaimed` by construction and reported as `delegated` with a diagnostic saying so. That sentence is the whole reason `delegated` is a tier rather than an escape hatch.

## 5. How a claim is used

| Surface | Use |
|---|---|
| `retype list --tiers` | Prints the claims, per pair, with engine, version and fingerprint |
| Plan compilation | A backend is chosen only if its claim meets the tier the pair declares, `docs/config.md` §4 |
| `retype why` | Prints the claim, the engine, and whether it is current |
| `retype doctor` | Reports the manifest version, each tool source, and how many installed engines differ from the versions and fingerprints their claims were earned with |
| Editor badge | The tier, with `unverified` shown as such rather than hidden |

## 6. Staleness

Evidence is about a version and a build. When the installed engine reports a different version or fingerprint from the one recorded:

- The pair still converts. A distro's older ImageMagick must not turn an install into a product that refuses to work
- The tier is reported `unverified`, and `why` names both versions and both fingerprints
- A pair that declares `exact` fails with `E_UNSUPPORTED` and a hint naming the version, the fingerprint and the declaration to relax. A promise of no loss cannot rest on evidence about a different encoder, and silently downgrading a promise is worse than refusing it

Staleness never silently upgrades either. An engine newer than the recorded one is still unverified, because a new version changing encoder defaults is exactly how a fidelity claim goes stale.

## 7. What is not a claim

Three things wear the same vocabulary and are not fidelity.

- `confidence` in `docs/provenance.md` is `exact`, `path-only` or `none`, and describes how a write was matched to an observed rename. It says nothing about an output's quality
- The quality dials in `docs/quality.md` move within a tier. `jpg+quality100` and `jpg+quality10` are both `converted`; the dial does not reach `exact`
- The format list is capability, not fidelity. A format we can write is not a pair we have evidence for

## 8. Claims for this release

Generated between the markers by `retype fidelity --render`, one line per claim, sorted by source, target and backend. Empty in the source tree until the first release run. The fingerprint is shortened here and kept whole in the manifest.

<!-- claims:start -->
<!-- claims:end -->
