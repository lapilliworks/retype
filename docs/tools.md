# Tools

External tools are dependencies with their own support matrix. A format claim in `docs/operations.md` or `docs/quality.md` is conditional on some tool in this matrix providing it, on this machine, in this build.

## 4. Image capabilities

Measured by encoding `tests/fixtures/mountains.png` (800x400) and inspecting the result. A listed format is not the same as a working encoder, which is why these are measured.

| Target | ImageMagick | Result |
|---|---|---|
| `webp` | yes | 11,242 bytes; monotonic on `-quality`: 4,976 at 20, 13,852 at 80, 35,376 at 95 |
| `avif` | yes | 9,265 bytes, ISO Media AVIF |
| `heic` | yes | 16,625 bytes, HEIF HEVC Main |
| `jpg` | yes | 51,709 bytes, JFIF |
| `tiff` | yes | 960,264 bytes, uncompressed by default |
| `gif` | yes | 92,942 bytes, 89a |
| `ico` | conditional | Fails at 800x400 with `width or height exceeds limit`; succeeds after `-resize 200x100` |

Constraints this establishes:

- `ico` targets require both dimensions at most 256. The operation plan must either resize or refuse, and the refusal belongs to the plan, not to the encoder's stderr
- `dng` is a decode-only capability, so a `to-dng` claim would be false
- `svg` encoding goes through the internal XML renderer here, not librsvg. Text and CSS support will differ from a librsvg build, so `tier = 'converted'` at best
- `tiff` is uncompressed unless a compression level is passed, which is why `tiff` has a `compress` dial and no quality dial

## 5. Document capabilities

- `pandoc` writes `docx`, `html`, `odt`, `epub`, `rst`, `tex`, `typst`, `markdown` and many more, verified for `docx` here
- **PDF works via WeasyPrint 70.0**, measured: markdown to PDF through `--pdf-engine=weasyprint`, 5,177 bytes for a heading, emphasis and a table
- WeasyPrint is an HTML and CSS renderer. pandoc renders to HTML and then to PDF, so output is web-grade rather than TeX-grade: math needs an engine that supports it, cross-references and citation styles differ, and page breaking follows CSS. Therefore `pdf` is `converted` at best, never `exact`, and the engine is recorded with the job because it changes the result
- A TeX engine or `typst`, if later installed, produces better typesetting and is preferred by the resolution order below
- This is the first capability whose dependency is itself an alternative, which is why capabilities are tuples and why engines are a list

### 5.1 PDF engines

Resolution order, first present wins:

| Order | Engine | Why here |
|---|---|---|
| 1 | `typst` | Modern typesetting, fast, single binary, no TeX installation. pandoc's typst writer is younger than its LaTeX writer, so this position is a default to be confirmed by fixtures rather than a settled fact |
| 2 | `xelatex`, `lualatex`, `pdflatex` | Mature writer and package ecosystem. Remains the better choice for citation-heavy academic work, and stays available as an explicit choice |
| 3 | `weasyprint` | Portable, no TeX, adequate for reports and web-derived documents |
| 4 | `wkhtmltopdf` | Legacy, kept last |

The order sets the default; it does not remove choice. A preset that needs LaTeX semantics pins its engine by naming it literally in `argv` and bypassing resolution, per `docs/config.md` §3.1.

The resolved engine is stored with the job and shown by `retype why`, because two PDFs from the same intent are not interchangeable across engines. It is also recorded in the fixture manifest, so a tier claim names the engine it was earned with.

`typst` is a single binary and a candidate for the managed toolchain. `weasyprint` is not: it carries a Python and GTK stack that belongs to the system, not to us. The resolution order is unchanged by how an engine arrived.

### 5.2 Warnings

WeasyPrint emits CSS warnings on stderr while succeeding, for example about properties in pandoc's default stylesheet that WeasyPrint ignores.

- Success is decided by exit status alone. Stderr is captured for the log, never parsed for failure
- The tail of stderr is stored with the job, so a warning that matters is visible in `retype jobs logs` without being treated as an error

## 7. Archives

Archives are not one format and are not one tool. Each format uses the backend that fits it.

| Format | Backend | Notes |
|---|---|---|
| `zip` | Native | Write and read, with explicit zip64, symlink and permission rules |
| `tar` | Native | Write and read, with the compression layers below |
| `gz`, `bz2`, `xz`, `zst`, `br`, `lz4` | Native | Codec crates or the system C library where the crate is a thin binding |
| `7z` | `7zz` | The only archive format that needs an external tool |
| `rar` | `unrar` or a `7zz` build with the unRAR code | Read-only, user-supplied, never distributed |

Native handling is not only fewer dependencies. Path traversal, symlink escape, decompression bombs, permission restoration, overwrite policy and cancellation are decisions that belong in our code, not in another tool's argv. It also removes the one tool whose layout put its output before its input.

`7zz` remains declared and may be installed from the managed set. It is not used for `zip` or `tar`, where the native backend is more precise.

