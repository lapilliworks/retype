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

- `pandoc` writes `docx`, `html`, `odt`, `epub`, `rst`, `tex`, `typst`, `markdown` and many more. Measured here on 2026-10-30 with pandoc 3.11: `md` to `html`, `docx`, `odt`, `epub` and `pdf` all succeed, and `html` uses `--standalone` so the output is a document rather than a fragment
- `pdf` is written and never read: pandoc has no PDF reader, so `pdf` as a source is refused at plan time rather than at the tool
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

Archives are not one format and are not one tool. Each format is a container, and converting one into another is a conversion like any other: the entries keep their names, their bytes and their permissions, and only the container changes.

| Format | Backend | Notes |
|---|---|---|
| `zip` | Native | Repacked, with explicit zip64 and permission rules below |
| `tar` | Native | Repacked |
| `tar.gz` | Native | Repacked: a tar stream through gzip |
| `7z` | `7zz`, `7z` or `7za` | Not declared yet: the only archive format that needs an external tool, and a repack through it needs a staging directory |
| `gz`, `bz2`, `xz`, `zst`, `br`, `lz4` | Native | Not declared, and not planned: those are compression, not conversion |
| `rar` | `unrar` | Read-only, user-supplied, never distributed |

What is not done, and is not a gap: an archive is never unpacked into a directory and a loose file is never compressed on its own. Both are operations a file manager already offers with a right click, and both are destructive in a way a conversion is not: the source disappears and a tree appears. The gesture this product exists for is a name that says what the file contains, which is why `notes.zip` to `notes.tar.gz` is a conversion and `notes.zip` to `notes/` is not.

Native handling is not only fewer dependencies. Entry names, symlinks, size limits, permissions and cancellation are decisions that belong in our code, not in another tool's argv. The rules this build applies when repacking, measured 2026-10-31:

- An entry name must be relative and free of `..`. A name that would escape the archive is refused rather than copied into another container
- A symbolic link refuses the archive. Carrying one is how a repack becomes a traversal for whoever unpacks it next
- Directories, files and their permission bits are carried; devices, FIFOs and anything else are refused by name
- The declared size of every entry is checked against a budget before a byte is copied: 100,000 entries and 8 GiB of declared expansion. Both are fixed and documented as fixed
- Names are written with `/` separators, so a zip made on Windows becomes a tarball a Unix can read
- Entries keep the order the source lists them, so repacking one archive twice produces the same bytes
- The output is staged and renamed into place, so a cancelled or failed repack leaves no partial archive

`7zz` remains the declared backend for `.7z` and may be installed from the managed set; `7z` and `7za` are executable aliases, not separate backends.

