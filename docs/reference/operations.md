# Operations

Registry reference. Syntax is defined in `docs/reference/intents.md`; this document defines the operations that occupy the name space.

An operation is identified by an immutable `key`. Aliases are accepted and never printed. A `slot` places it in the canonical order. An `exclusive` set makes members of the same set conflict.

A group with one member is not declared: repeating one operation is `E_DUPLICATE`, so a group whose only member is `rotate` would add nothing and `docs/reference/config.md` V8 refuses it as a probable mistake.

## 1. Name space

Reserved keys, all phases. Keys are permanent; a key is never reused for a different meaning.

| Key | Name | Slot | Exclusive | Parameter | Status |
|---|---|---|---|---|---|
| `quality` | `quality` | 50 | `encode-target` | `int`, range per format | Built |
| `lossless` | `lossless` | 50 | `encode-target` | none | Declared |
| `size` | `size` | 50 | `encode-target` | `size` | Declared |
| `effort` | `effort` | 50 | none | `int`, range per format | Declared |
| `compress` | `compress` | 50 | none | `int`, 0 to 9 | Declared |
| `colors` | `colors` | 50 | none | `int`, 2 to 256 | Declared |
| `cap` | `cap` | 28 | `scale` | `int`, 16 to 16384 | Built |
| `resize` | `resize` | 25 | `scale` | `dim`, 1 to 16384 per side | Built |
| `width` | `width` | 26 | `scale` | `int`, 16 to 16384 | Built |
| `height` | `height` | 27 | `scale` | `int`, 16 to 16384 | Built |
| `crop` | `crop` | 24 | none | `ratio`, 1 to 1000 per side | Declared |
| `rotate` | `rotate` | 21 | none | `enum`: 90, 180, 270 | Built |
| `fliph` | `fliph` | 22 | none | none | Built |
| `flipv` | `flipv` | 23 | none | none | Built |
| `gray` | `gray` | 30 | none | none | Declared |
| `nometa` | `nometa` | 60 | none | none | Declared |
| `noprofile` | `noprofile` | 60 | none | none | Declared |
| `trim` | `trim` | 10 | `time-slice` | `range` of `time` | Reserved |
| `at` | `at` | 10 | `time-slice` | `time` | Built |
| `speed` | `speed` | 10 | `tempo` | `int` 1 to 400, percent | Reserved |
| `fps` | `fps` | 10 | `tempo` | `int` 1 to 240 | Built |
| `mute` | `mute` | 40 | `audio-track` | none | Built |
| `normalize` | `normalize` | 40 | none | none | Reserved |
| `mono` | `mono` | 40 | none | none | Reserved |
| `bitrate` | `bitrate` | 40 | `audio-rate` | `int`, range per codec | Reserved |
| `codec` | `codec` | 40 | none | `enum`, per container | Reserved |
| `page` | `page` | 10 | `time-slice` | `range` of `int` | Reserved |
| `dpi` | `dpi` | 10 | none | `int`, 36 to 1200 | Reserved |
| `ask` | `ask` | 90 | `confirm` | none | Built |
| `now` | `now` | 90 | `confirm` | none | Built |

Status is `Built` when this build has a tool fragment for the operation, `Declared` when the grammar accepts it but no tool fragment exists yet, and `Reserved` when the name and its parameter are fixed but the operation is not implemented. `Declared` and `Reserved` operations fail with `E_UNSUPPORTED` rather than doing nothing.

Slots are integers used for presentation: they order a generated editor's controls and the rows of `retype list`, and they never reorder an intent string. Two operations that interact do not share a slot, so a generated interface does not present them as interchangeable. The geometry numbers run `rotate` 21, `fliph` 22, `flipv` 23, `crop` 24, `resize` 25, `width` 26, `height` 27, `cap` 28, which is a workable order to show them in and nothing more.

Aliases:

| Alias | Key |
|---|---|
| `q` | `quality` |
| `w` | `width` |
| `h` | `height` |
| `d` | `dpi` |
| `grey`, `grayscale` | `gray` |

## 2. Geometry semantics

Geometry runs in the order written, and the order changes the result: `rotate90+cap1080` rotates and then caps, while `cap1080+rotate90` caps and then rotates, and the two are not the same image. Nothing reorders a string, `docs/reference/intents.md` C2.

| Operation | Rule |
|---|---|
| `rotate` | Clockwise by the given multiple of 90 degrees. Pixel-exact, no resampling |
| `fliph`, `flipv` | Mirror about the vertical, respectively horizontal, axis |
| `crop` | Centre crop to the given aspect ratio, keeping the largest region that fits. Never upscales |
| `resize` | Fit inside `WxH` preserving aspect. A `0` side is computed from the other. Never upscales |
| `width` | Scale so the width equals `N`, preserving aspect. Never upscales |
| `height` | Scale so the height equals `N`, preserving aspect. Never upscales |
| `cap` | Scale down so the longest edge is at most `N`. Equivalent to `resize` with both sides set to `N` |

Resampling filter is Lanczos3 for downscale and triangle for upscale. Upscale is never implicit; a `resize` whose result would enlarge the image produces the source size unchanged.

## 3. Colour and metadata

| Operation | Rule |
|---|---|
| `gray` | Convert to single channel. luma from Rec. 709 coefficients in linear light |
| `nometa` | Remove EXIF, XMP, IPTC, GPS, thumbnails and comments. Preserve the ICC profile |
| `noprofile` | Remove the ICC profile. Pixel values are not converted, so colours may shift |

Orientation is always applied from EXIF and then removed, once, before any geometry. A job never produces a file whose EXIF orientation contradicts its pixels.

## 4. Encoder semantics

| Operation | Rule |
|---|---|
| `quality` | Fidelity dial in the target format's own scale. Range and meaning per format in `docs/reference/dials.md` |
| `effort` | Speed dial in the target encoder's own scale. Higher is slower |
| `lossless` | Request lossless encoding. Conflicting with `quality` and `size` |
| `size` | Re-encode to fit under a byte budget. The search tries `quality`, then `effort`; an unreachable budget is `E_VALUE_RANGE` after the search |
| `compress` | Lossless compression level, 0 to 9, for formats whose container has one (PNG deflate) |
| `colors` | Palette size for paletted formats. 256 means the format maximum |

`quality`, `size` and `lossless` share the `encode-target` group: exactly one target per output. `effort` is orthogonal.

## 5. Applicability, image formats

`y` applicable, `n` inapplicable, so `E_INAPPLICABLE`.

| Operation | jpg | png | webp | avif | heic | gif | tiff | bmp | ico |
|---|---|---|---|---|---|---|---|---|---|
| `quality` | y | n | y | y | y | n | n | n | n |
| `effort` | n | n | y | y | n | n | n | n | n |
| `lossless` | n | n | y | y | y | n | n | n | n |
| `size` | y | y | y | y | y | n | n | n | n |
| `compress` | n | y | n | n | n | n | y | n | n |
| `colors` | n | y | n | n | n | y | n | n | n |
| `cap` | y | y | y | y | y | y | y | y | y |
| `resize` | y | y | y | y | y | y | y | y | y |
| `width` | y | y | y | y | y | y | y | y | y |
| `height` | y | y | y | y | y | y | y | y | y |
| `crop` | y | y | y | y | y | y | y | y | y |
| `rotate` | y | y | y | y | y | y | y | y | y |
| `fliph` | y | y | y | y | y | y | y | y | y |
| `flipv` | y | y | y | y | y | y | y | y | y |
| `gray` | y | y | y | y | y | y | y | y | y |
| `nometa` | y | y | y | y | y | y | y | n | y |
| `noprofile` | y | y | y | y | y | y | y | n | y |

`size` on `png` means the deflate level is searched, not quality; on lossless targets it is satisfied or refused by `compress` bounds.

`png` with `colors` produces a paletted PNG, that is PNG8.

`ico` requires both dimensions at most 256. A plan that would produce a larger icon is rejected with `E_INAPPLICABLE` and a hint to add `cap256`, rather than left to the encoder to fail.

Per-format constraints of this kind come from [Tools](../advanced/converters.md) §6, because they are properties of the encoder rather than of the operation.

## 6. Not specified yet

`at`, `fps`, `mute` and `cap` work for video and audio. Documents have their formats and no operations: `md`, `html`, `docx`, `odt`, `epub` and `pdf` convert through pandoc.

Archives take no operations at all: `zip`, `tar` and `tar.gz` store the bytes they are given, and an operation that changed them would make the format an encoder. A plan that asks for one is `E_UNSUPPORTED` naming the archive. A directory is not an archive source, so the gesture stays container to container.

`crop`, `gray`, `nometa`, `noprofile`, `lossless`, `size`, `effort`, `compress` and `colors` are declared and do not yet have a tool fragment, so using one is `E_UNSUPPORTED` rather than a silent no-op.

`trim`, `speed`, `normalize`, `mono`, `bitrate`, `codec`, `page` and `dpi` stay reserved with their slots and parameters, and a job that uses one is `E_UNSUPPORTED` rather than `E_NAME_UNKNOWN`. Two of them have a reason rather than a pending vertical:

- `page` and `trim` take a `range`, and a fragment has one `{value}` hole. The grammar reads a range, but expressing two numbers to a tool needs an argument that can carry both
- `dpi` means something different to every document engine, and nothing at all to `docx`: pandoc's variable reaches a LaTeX preamble and a typst document alike only by accident. A dial that silently does nothing in half its targets is worse than a dial that refuses

## 7. Presets

A preset is a named token sequence and occupies the same name space. See `docs/reference/config.md`.

```toml
[presets.thumbnail]
param = { class = 'int', min = 16, max = 4096 }
tokens = ['cap={value}', 'quality85']

[presets.headshot]
tokens = ['crop1x1', 'cap1024', 'quality80', 'nometa']
```

Presets MUST expand to a conflict-free token set at load; a preset that cannot satisfy its own exclusivity rules fails to load. A preset MUST NOT contain `ask` or `now`.
