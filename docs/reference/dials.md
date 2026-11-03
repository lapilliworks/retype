# Quality and size dials

Per-format meaning of `quality`, `effort` and `compress`. Syntax and applicability are in `docs/reference/intents.md` and `docs/reference/operations.md`.

## 1. Rule

There is no uniform quality dial. `quality` is expressed in the target format's own scale, with the target format's own range, and a format without a fidelity axis makes `quality` inapplicable, which is `E_INAPPLICABLE`, never a silent no-op.

Anchors:

- `quality` always increases monotonic fidelity and cost with its value
- Where the reference encoder counts backwards (CRF, LAME `-V`), the mapping inverts it, so `quality100` is always best quality
- `effort` is the speed dial where a format has one, likewise normalised so higher is slower and better
- `compress` is a lossless compression level, 0 to 9, and never changes pixels

Defaults policy: the reference encoder's documented default when one is documented; otherwise a recorded choice, marked as such.

## 2. Images

| Format | `quality` range | Default | `effort` range | Default | `compress` | `colors` |
|---|---|---|---|---|---|---|
| `jpg` | 1 to 100 | 75 | none | | none | none |
| `png` | inapplicable | | none | | 0 to 9 | 2 to 256 |
| `webp` | 1 to 100 | 75 | 0 to 6 | 4 | none | none |
| `avif` | 1 to 100 | 60 | 0 to 10 | 6 | none | none |
| `heic` | 1 to 100 | 50 | none | | none | none |
| `gif` | inapplicable | | none | | none | 2 to 256, default 256 |
| `tiff` | inapplicable | | none | | 0 to 9, default 6 | none |
| `bmp` | inapplicable | | none | | none | none |
| `ico` | inapplicable | | none | | none | none |

Capability is per backend and per tool build, not only per format. The table states what a format accepts; whether a particular build can produce it is resolved by the plan compiler against `docs/advanced/converters.md`, so a format row must never be read as a promise that every backend implements it.

Per-format rules:

- `jpg`: `quality` scales the quantization tables. At `quality >= 90` chroma subsampling is 4:4:4, below it 4:2:0. Sampling is set explicitly, never left to the encoder default. Progressive encoding and Huffman optimisation are on by default
- `png`: always lossless. `compress` is the deflate level. Filter selection is adaptive. No quality axis exists, and a request for one is an error
- `webp`: lossy by default. `lossless` switches to lossless mode, where `quality` instead trades speed for size and `effort` sets the method. Both modes go through libwebp, which is what ImageMagick and FFmpeg link
- `avif`: `quality` is the colour quantiser, 100 being lossless; alpha uses the same value. `effort` is the encoder speed
- `heic`: `quality` is the encoder quality; `lossless` overrides it, and combining the two is `E_EXCLUSIVE`
- `gif`: paletted, at most 256 entries. `colors` sets the palette size; dithering is Floyd-Steinberg
- `tiff`: `compress` selects the deflate level; other codecs are chosen by `codec` when that lands
- `bmp`, `ico`: no encoder dials; only geometry and colour operations apply

## 3. Audio and video

Reserved. Ranges and defaults are fixed with the audio and video verticals. Verified so far:

| Format | Dial | Range | Source |
|---|---|---|---|
| `flac` | `compress` | 0 to 8, default 5 | xiph flac(1) documentation |
| `mp3` | `quality` | maps to LAME `-V`, 0 to 9 | FFmpeg codecs documentation |
| `av1` | `quality` | maps to CRF 0 to 63 | FFmpeg codecs documentation |
| `hevc` | `quality` | maps to CRF 0 to 51 | FFmpeg codecs documentation, NVENC `cq` |
| `h264` | `quality` | maps to CRF 0 to 51 | FFmpeg codecs documentation, NVENC `cq` |
| `opus` | `bitrate` | 6 to 512 kbit/s | FFmpeg codecs documentation |

Not yet verified against `ffmpeg -h encoder=<encoder>`: libx264 and libx265 CRF bounds, MJPEG `-q:v` bounds, libvorbis `-q:a` bounds, AAC VBR behaviour.

## 4. Sources

- S1 ImageMagick, command-line options, `-quality`: <https://imagemagick.org/command-line-options/#quality>. Establishes JPEG 1 to 100, estimated default else 92, chroma channels not downsampled at 90 or above; PNG quality maps to deflate level and filter, default 75; TIFF quality maps to a deflate level; PNG quality does not affect appearance
- S2 `cjpeg(1)`, libjpeg-turbo-progs, Debian: <https://manpages.debian.org/unstable/libjpeg-turbo-progs/cjpeg.1.en.html>. Establishes JPEG quality 0 to 100, default 75, implemented as quantization table scaling
- S3 `cwebp(1)`, webp, Debian: <https://manpages.debian.org/unstable/webp/cwebp.1.en.html>. Establishes `-q` 0 to 100 default 75, `-lossless`, `-alpha_q` default 100, `-m` 0 to 6 default 4
- S4 `avifenc(1)`, libavif-bin, Debian: <https://manpages.debian.org/unstable/libavif-bin/avifenc.1.en.html>. Establishes `--qcolor` 0 to 100 with 100 lossless, `--qalpha`, `--speed` 0 to 10 default 6
- S5 `heif-enc(1)`, libheif-examples, Debian: <https://manpages.debian.org/unstable/libheif-examples/heif-enc.1.en.html>. Establishes `-q` 0 to 100, `-L` lossless, and that `-q` has no effect with `-L`
- S6 zlib manual: <https://www.zlib.net/manual.html>. Establishes levels 0 to 9 and that `Z_DEFAULT_COMPRESSION` is level 6
- S7 FLAC documentation, `flac(1)`: <https://xiph.org/flac/documentation_tools_flac.html>. Establishes `-0` to `-8` presets and default `-5`
- S8 FFmpeg codecs documentation: <https://ffmpeg.org/ffmpeg-codecs.html>. Establishes libaom-av1 `crf` 0 to 63, libmp3lame `-q` and `compression_level` 0 to 9, NVENC `cq` 0 to 51 for H.264 and HEVC and 0 to 63 for AV1, libvvenc `crf` 0 to 63, libopus `vbr` 0 to 2
- S9 GIF89a specification: <https://www.w3.org/Graphics/GIF/spec-gif89a.txt>. Establishes a 3-bit size field for the colour table, so at most 256 entries

## 5. Re-verification

Every row must be reproducible without trust in this document:

```
magick -list format
ffmpeg -h encoder=<encoder>
avifenc --help
heif-enc --help
cwebp -longhelp
```

A range that cannot be reproduced from a local probe or a cited document does not ship.
