# Quality, size and geometry

The tokens after the format are the transformation. They run in the order they are written.

## Quality is the target format's own

`quality`, or `q`, is a number from 1 to 100 where higher is better looking and larger. There is no shared scale: each format has its own range and its own meaning, and the number is passed to that format's encoder in that format's terms. The exact ranges and defaults are in [Quality and size dials](../reference/dials.md).

| Format | Range | Default | What the number does |
|---|---|---|---|
| `jpg` | 1 to 100 | 75 | Quantisation table scaling. At 90 and above colour is kept at full resolution |
| `webp` | 1 to 100 | 75 | Encoder quality. `lossless` switches the mode and the number then trades speed for size |
| `avif` | 1 to 100 | 60 | Colour quantiser, 100 is lossless |
| `heic` | 1 to 100 | 50 | Encoder quality |
| `png`, `gif`, `tiff`, `bmp`, `ico` | none | | Lossless formats have no quality axis |

Asking for a dial a format does not have is an error, not a no-op:

```
retype c scan.png jpg+q80       # fine
retype c scan.jpg png+q80       # E_INAPPLICABLE: png has no quality
```

The alternative would be to accept the token and quietly ignore it, which teaches a habit that fails later on a format where it matters.

No setting here reaches a lossless re-encode: a `jpg` at `quality100` is still a re-encode of a re-encode, while a `png` at `compress9` loses nothing at all. That difference is the tier, and [Fidelity](../advanced/fidelity.md) is where it is defined.

## Shrinking

Sizes are pixels, and nothing here ever enlarges an image. A resize that would make a picture bigger produces the source size instead.

| Token | Effect |
|---|---|
| `cap1080` | Longest edge at most 1080. The one to reach for |
| `1080p` | The same as `cap1080` |
| `4k` | The same as `cap2160` |
| `resize1280x720` | Fit inside the box, keeping the proportions. A side of `0` is computed from the other, so `resize1280x0` means 1280 wide |
| `width1280` | Scale so the width is 1280 |
| `height720` | Scale so the height is 720 |
| `crop1x1` | Centre crop to an aspect ratio, keeping the largest region that fits |

`resize`, `width`, `height` and `cap` are four ways of saying "these proportions, no larger than this", and they are mutually exclusive within one output: asking for a width and a cap in the same name is `E_EXCLUSIVE`. Which operations a format accepts at all is the table in [Operations](../reference/operations.md).

## Turning and mirroring

| Token | Effect |
|---|---|
| `rotate90`, `rotate180`, `rotate270` | Clockwise. Pixel-exact, nothing is resampled |
| `fliph` | Mirror left to right |
| `flipv` | Mirror top to bottom |

The rotation a camera recorded in the file is applied first and then removed, once, before any other geometry, so an output never carries a rotation tag that contradicts its pixels.

## The order is the pipeline

```
photo.heic  ->  photo-rotate90-cap1080.webp
photo.heic  ->  photo-cap1080-rotate90.webp
```

Those are different images: the first turns a portrait picture sideways and then caps the long edge, the second caps first and turns afterwards. Nothing is reordered on the way in, because there is no table of which operations commute, and a fixed order would be right for some names and silently wrong for the rest.

## Byte budgets and lossless

| Token | Effect |
|---|---|
| `size5mb` | Re-encode to fit under a budget, searching quality and then effort. `5mb` is decimal, `5mib` is binary, and [Retype intent grammar](../reference/intents.md) has the units and their spellings |
| `lossless` | Request lossless encoding, where the target has a lossless mode |
| `effort4` | The encoder's speed dial, where it has one. Higher is slower and better |
| `compress9` | The lossless compression level, 0 to 9, for containers that have one |
| `colors256` | Palette size for paletted formats |

`quality`, `size` and `lossless` are three answers to the same question and only one of them may appear per output. `effort` is orthogonal and may.

A budget that cannot be reached is refused rather than ignored: the search ends, and the job fails naming the smallest size it could produce.

## Recipes

A preset is a name for a token sequence, declared in configuration. It composes with everything else:

```
retype c photo.heic jpg+thumbnail800     # whatever the preset expands to
```

Presets are described in [Configuration schema](../reference/config.md).

## What is not available yet

This is alpha, and the name space is larger than the set of conversions that run. Asking for an operation that is declared but has no implementation fails with `E_UNSUPPORTED` rather than doing nothing:

- `crop`, `gray`, `nometa`, `noprofile`, `lossless`, `size`, `effort`, `compress` and `colors` are declared and not built
- Video and audio: `cap`, `at`, `mute` work; `trim`, `speed`, `fps`, `normalize`, `mono`, `bitrate`, `codec` are reserved
- Documents convert through Pandoc with no operations at all, and archives are repacked from one container to another rather than transformed

[Operations](../reference/operations.md) is the full table, including which operations apply to which format.
