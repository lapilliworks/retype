# Fidelity fixtures

The files `retype fidelity` checks a pair against, `docs/fidelity.md` §4. Small on purpose: they are evidence, not samples, and every one of them is under a kilobyte.

| File | Format | Axis | What it is |
|---|---|---|---|
| `alpha.png` | png | alpha | A transparent left half and an opaque right half |
| `depth.png` | png | depth | Sixteen bits per channel |
| `icc.png` | png | icc | A synthetic RGB profile embedded in an `iCCP` chunk |
| `palette.png` | png | palette | Three colours in an indexed container |
| `orient.jpg` | jpg | orientation | 32×16 with EXIF orientation 6, so a baked rotation is visible |
| `anim.gif` | gif | animation | Three frames |
| `anim.webp` | webp | animation | Three frames |

`fixtures.toml` declares which axis each file carries. `Probe::read` is the check that the declaration is true; a file whose bytes stop carrying its axis fails the test rather than quietly weakening the claim.

`generate.py` regenerates the set. It is not part of the build and needs ImageMagick. Two of the files are written partly by hand, because ImageMagick will not write them: the ICC profile is synthesised by the script rather than copied from somebody else's licensed profile, and the EXIF orientation tag is inserted as a JPEG APP1 segment, because ImageMagick only writes that tag for images it already believes are rotated.
