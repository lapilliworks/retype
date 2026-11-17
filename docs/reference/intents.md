# Retype intent grammar

Specification version: `retype-grammar-1`.

Inspiration: Consul's rename-to-convert grammar. This specification is otherwise independent of it.

Normative keywords: MUST, MUST NOT, SHOULD, MAY, as in RFC 2119.

## 1. Scope

This document defines the syntax and semantics of an *intent*: the string that expresses a conversion and its transformations, carried in a file name.

Out of scope, defined elsewhere:

- The operation set, its parameters and its behaviour
- Backend selection, tiers and fidelity claims
- Job lifecycle, journalling and undo

## 2. Terms

*Intent string*: the part of a file name that follows the source stem and a single `.`

*Source*: the file being renamed, together with its stem and format as observed before the rename

*Slot*: the ordering category of an operation, used only for canonical printing

*Key*: the unique identity of an operation. Aliases resolve to a key

## 3. Carriers and anchoring

An intent string is a value. Its carriers are a file name, a CLI argument, an editor field and a journal record.

For the file name carrier only, the parser MUST be given the source stem and MUST NOT derive the stem from the intent-bearing name.

```
anchor(new_name, stem) -> intent string | NoIntent | E_ANCHOR
```

`new_name` MUST begin with `stem` followed by `.`. The remainder is the intent string.

- A name that does not begin with the source stem carries no intent. This is a plain rename
- If such a name nevertheless contains `+`, `,` or a final `!` after its last `.`, the outcome is `E_ANCHOR`, never a silent plain rename
- No other carrier is anchored

Consequences of anchoring, all required:

- `my.file.txt` has stem `my.file`, not `my`
- `archive.tar.gz` has a format component `tar.gz`, not `gz`
- `a+b.jpg` has stem `a+b`; the `+` is not a token separator
- A stem change cannot carry an intent. Renaming and transforming are separate gestures
- The directory cannot change. A rename that moves the file is a move, and a move carries no intent

The source stem is not what follows the last dot. It is what remains when the source *format* is removed, and the format is the longest declared format suffix of the source name, matched case insensitively, with aliases canonicalised. `archive.tar.gz` therefore has the format `tar.gz` and the stem `archive` whenever both `gz` and `tar.gz` are declared. The registry is the list that decides this boundary, the way a public suffix list decides where a domain's registrable label ends; a name whose suffix no declared format matches has no format, and there is nothing to convert from.

## 4. Character set

The intent string MUST match:

```
[A-Za-z0-9.,+!=_-]*
```

Any other code point is `E_CHARSET`. There is no quoting and no escaping.

Positional restrictions:

- `!` MUST appear only as the final character, at most once
- `=` MUST appear only inside a token, at most once per token; a second occurrence is `E_VALUE_SYNTAX`
- `/`, `\` and space are forbidden, so an intent can never name a path

## 5. Form

```
intent   = group *( "," group )
group    = format *( "+" token )
format   = ext / "folder"
ext      = part *( "." part )
part     = 1*( %x61-7A / DIGIT )
token    = name [ "=" value ]
name     = %x61-7A *( %x61-7A / DIGIT )
value    = int / dim / ratio / size / time / range / enum / code / text
```

Names and formats are matched case-insensitively and canonicalised to lowercase. Values are case-sensitive except `enum` and `code` (see 7).

Limits:

| Limit | Value | Violation |
|---|---|---|
| Intent string | 255 bytes | `E_TOO_LONG` |
| Groups | 8 | `E_TOO_MANY` |
| Tokens per intent | 32 | `E_TOO_MANY` |

A new name longer than the platform limit MUST also fail with `E_TOO_LONG`.

## 6. Parsing

```
parse(source, new_name) -> Job | Diagnosis
```

1. Strip a final `!` and record the `now` meta token.
2. Obtain the intent string, by anchoring for the file name carrier or directly from the caller. An empty string is `E_INTENT_EMPTY`.
3. Strip a final `+` and record the `ask` meta token. A `+` in any other position MUST be followed by a non-empty token.
4. Split on `,`. An empty group is `E_GROUP_EMPTY`.
5. Split each group on `+`. Element 0 is the format; the remainder are tokens.
6. For each token: recognise the job-wide `keep` control, or split at `=` when present and require an exact registered name. Otherwise resolve the longest registered name prefix; the remainder is the value. No name prefix is `E_NAME_UNKNOWN`.
7. Validate the value against its class and bounds, then against the duplicate and exclusive rules in 8.
8. Check applicability against the group's format and the pair's backend support.
9. When both `ask` and `now` are present, keep `ask`, drop `now`, and record a warning with code `E_META_CONFLICT`. A prompt can be dismissed with one key; a refused rename has to be performed again, so the recoverable reading wins
10. Reject an intent whose only group has the source format and no tokens as `E_NOOP`. In a multi-group intent such a group means: keep the source unchanged.

The parser MUST NOT backtrack and MUST be total: every string in the character set produces either a job or a diagnosis.

The parser MUST accumulate all independent diagnostics, capped at 8, ordered by offset.

## 7. Value classes

A class is declared per operation parameter. Bounds come from the operation.

| Class | Form | Notes |
|---|---|---|
| `int` | `1*DIGIT` | Leading zeros accepted, not printed |
| `dim` | `int "x" int` | Pixels. `0` means auto on one side |
| `ratio` | `int "x" int` | Aspect only. Both sides non-zero |
| `size` | `decimal ( decimal-unit / binary-unit )` | `kb`, `mb`, `gb` are powers of a thousand; `kib`, `mib`, `gib` are powers of a thousand and twenty-four. A fraction is allowed here and nowhere else, because there is no byte unit to fall back on |
| `time` | see below | Seconds unless a unit is present |
| `range` | `scalar`, `scalar-scalar`, `scalar-`, `-scalar` | Scalar is declared per parameter as `int` or `time` |
| `enum` | one of a declared list | Matched case-insensitively |
| `code` | `2*3ALPHA *( "-" 1*8ALNUM )` | BCP 47 subset. Subtag case preserved |
| `text` | `1*64( ALPHA / DIGIT / "." / "_" / "-" )` | Free-form preset argument |

```
int    = 1*DIGIT
dim    = int "x" int
ratio  = int "x" int
size   = decimal ( decimal-unit / binary-unit )
decimal-unit = "kb" / "mb" / "gb"      ; 1000, 1000^2, 1000^3 bytes
binary-unit  = "kib" / "mib" / "gib"   ; 1024, 1024^2, 1024^3 bytes
decimal = int [ "." 1*DIGIT ]
time   = tagged
tagged = 1*DIGIT / 1*DIGIT "s" / 1*DIGIT "m" [ 1*DIGIT "s" ]
       / 1*DIGIT "h" [ 1*DIGIT "m" ] [ 1*DIGIT "s" ]
range  = scalar / scalar "-" scalar / scalar "-" / "-" scalar
scalar = int / time
code   = 2*3ALPHA *( "-" 1*8ALNUM )
text   = 1*64( ALPHA / DIGIT / "." / "_" / "-" )
enum   = <literal from the operation's declared list>
```

Accepted `time` forms:

| Form | Example | Equivalent |
|---|---|---|
| `int` | `90` | 90 s |
| `int "s"` | `90s` | 90 s |
| `int "m" [ int "s" ]` | `2m30s` | 150 s |
| `int "h" [ int "m" ] [ int "s" ]` | `1h30s` | 3630 s |
| `int "d" [ int "h" ] [ int "m" ] [ int "s" ]` | `1d12h` | 129600 s |
| `int "w" [ int "d" ] [ int "h" ] [ int "m" ] [ int "s" ]` | `2w3d` | 1468800 s |

Rules:

- In the tagged form the units MUST appear in the order w, d, h, m, s, each at most once. A unit MAY be omitted, so `1h30s` needs no `0m`: nobody says "one hour, zero minutes, thirty seconds"
- A colon form is not accepted: `:` is outside the intent character set, because a Windows file name cannot carry it
- A number without a unit MUST be the whole value, so `1h30` is `E_VALUE_SYNTAX`
- There is no month unit. A month is not a fixed number of seconds, and every other unit is exact, so `1M` is `E_VALUE_SYNTAX` and a quarter year is written `13w`
- A fraction is permitted only in `size`, and only down to a whole byte: `1.5mb` and `1.234kb` are exact, `1.2345kb` is half a byte and is `E_VALUE_SYNTAX`
- Decimal and binary units are different quantities, and neither is a synonym for the other: `1mb` is a thousand kilobytes and `1mib` is a thousand and twenty-four kibibytes, which is 4.9 percent more. A budget written as `1mib` means 1048576 bytes

## 8. Names, keys and exclusivity

Every operation declares:

| Field | Requirement |
|---|---|
| `key` | Unique among operations. Immutable across versions |
| `name` | Primary spelling, printed canonically |
| `aliases` | Zero or more alternative spellings, accepted, never printed |
| `parameter` | Zero or one, with a value class and bounds |
| `slot` | Ordering category, see 10 |
| `exclusive` | Zero or more group names |

Rules:

- R1 Names, aliases and preset names MUST be unique across the registry
- R2 Resolution is the longest registered name prefix, so a name that is a prefix of another name resolves to the longer one. Registry authors SHOULD NOT declare such pairs, and loading SHOULD warn when they exist. `q` beside `quality` is legitimate; `crop` beside `crop9` is a trap
- R3 A repeated key within one intent is `E_DUPLICATE`
- R4 Two tokens sharing an `exclusive` group within one intent are `E_EXCLUSIVE`
- R5 A value on an operation that declares no parameter is `E_VALUE_UNEXPECTED`; an absent value on an operation that declares one is `E_VALUE_MISSING`
- R6 `=` forces an exact name match and exists to disambiguate R2

Presets occupy the same name space and declare zero or one parameter, of any class. A preset with a parameter accepts a positional value: `+thumbnail200` and `+thumbnail=200` are equivalent.

## 9. Aliases

Aliases are a desugaring pass applied to a whole token before name resolution:

| Pattern | Desugars to |
|---|---|
| `^[0-9]+(kb\|mb\|gb)$` | `size=<token>`, subject to the operation's bounds |
| `^[0-9]+p$` | `cap=<int>`, the numeric part |
| `^4k$` | `cap=2160` |

Short-name aliases (`q` for `quality`, `w` for `width`, `h` for `height`, and so on) are declared per operation and MUST be unique across the registry.

Desugaring MUST NOT be applied inside a value.

## 10. Canonical form

`print(job) -> intent string`

Rules:

- C1 Formats and names are printed in lowercase, using primary names only
- C2 Tokens retain written order. Reordering is never applied, because the registry has no table of which operations commute: `quality` and `cap` do, `resize` and `rotate` do not, so any fixed sequence would be right for some strings and silently wrong for others. The string is the pipeline
- C3 Groups retain written order
- C4 Values are printed in the form produced by their class, without leading zeros. A duration prints in the tagged form with zero units omitted, so 3630 s prints as `1h30s` and 3600 s prints as `1h`
- C5 A token is printed without `=` when reparsing the compact form yields an identical token, and with `=` otherwise
- C6 `keep`, `ask` and `now` print on the first group as `+keep`, `+ask` and `+now`, never as the trailing `+` or `!` shorthands
- C7 Nothing is printed for the implicit conversion of a group

The `slot` an operation declares is a presentation category: it orders controls in the editor and rows in `retype list`, and it decides nothing about a conversion. Two operations that interact may not share one, so that a generated interface still presents them in a workable order.

| Rank | Slot | Member examples |
|---|---|---|
| 10 | temporal | `trim`, `at`, `speed`, `fps` |
| 20 | geometry | `rotate`, `fliph`, `flipv`, `crop`, `resize`, `width`, `height`, `cap` |
| 30 | colour | `gray` |
| 40 | audio | `mute`, `normalize`, `mono`, `bitrate`, `codec` |
| 50 | encoder | `quality`, `lossless`, `size`, `effort`, `compress`, `colors` |
| 60 | metadata | `nometa` |
| 90 | control | `ask`, `now` |

Because order is the user's, two spellings that differ only in the order of commuting operations are two canonical strings: `webp+q80+cap1080` and `webp+cap1080+q80` produce the same file and are printed as written. Normalising them would require knowing which pairs commute, and claiming a pair commutes when it does not is the worse error.

## 11. Output groups

A group denotes one output file.

- A group whose format equals the source format and which carries no conversion operations means: keep the source file unchanged. No conversion runs for it
- A group whose format equals the source format and which carries conversion operations means: re-encode the source
- Every other group produces a converted file
- `+keep` applies to the whole job regardless of which group carries it. It keeps the original visible independently of undo, preserving the exact original filename, format alias and case
- `keep` is reserved for this control; user operations and presets cannot redeclare it or hide it in a preset

Derived names:

```
<stem> "-" <token> *( "-" <token> ) "." <ext>
```

where `<token>` is the canonical `name` followed by its printed value, for example `quality60`. A `.` inside a value is written as `_` so the derived name has one extension. A group with no tokens derives `<stem>.<ext>`. A collision appends `-2`, `-3`, and so on, in group order.

Invariants:

- D1 A derived name MUST NOT contain `+`, `,` or a final `!`, so a produced file can never parse as an intent
- D2 Derived names MUST be unique within a job and MUST NOT overwrite the kept source
- D3 Group order is stable and determines collision numbering

## 12. Diagnostics

A diagnosis carries a code and a half-open byte span into the intent string.

| Code | Condition |
|---|---|
| `E_CHARSET` | Character outside the set in 4 |
| `E_ANCHOR` | Name not anchored on the source stem but carrying intent syntax |
| `E_INTENT_EMPTY` | No intent string |
| `E_TOO_LONG` | Length limit exceeded |
| `E_TOO_MANY` | Group or token count exceeded |
| `E_GROUP_EMPTY` | Empty group between commas |
| `E_TOKEN_EMPTY` | Empty token not in trailing position |
| `E_FORMAT_UNKNOWN` | Format not in the registry |
| `E_FORMAT_FOLDER` | `folder` used with a non-directory source |
| `E_NAME_UNKNOWN` | Token prefix matches no name, alias or preset |
| `E_VALUE_MISSING` | Value required and absent |
| `E_VALUE_UNEXPECTED` | Value present and not accepted |
| `E_VALUE_SYNTAX` | Value fails its class |
| `E_VALUE_RANGE` | Value outside its bounds |
| `E_DUPLICATE` | Repeated key |
| `E_EXCLUSIVE` | Conflict within an exclusive group |
| `E_NOOP` | Only group equals the source format and carries no tokens |
| `E_INAPPLICABLE` | Operation not applicable to the group's format |
| `E_UNSUPPORTED` | No backend for the pair at the required tier |

Diagnostics SHOULD carry a suggestion when a registered name is within edit distance 2 of an unknown token.

One code is a warning rather than a diagnosis: `E_META_CONFLICT`, when an intent asks (`+`) and also demands `now` (`!`). Asking wins, `now` is dropped, and the warning names what was ignored. Every other condition in the table fails the parse.

## 13. Application on diagnosis

The job MUST NOT run as written. Retype restores the original name when it can do so without overwriting another file, and reports a failure when it cannot. A user-space watcher observes the rename after the operating system performs it, so it cannot prevent a temporary mismatch or recover a destination overwritten before observation.

## 14. Conformance

An implementation conforms when all of the following hold as property tests over a generated corpus:

- T1 `parse(print(job)) == job` for every valid job in the corpus
- T2 `print(parse(s))` is idempotent for every valid intent string `s`
- T3 Every string in the character set either parses or yields at least one diagnosis; no panic and no partial job
- T4 No derived name parses as an intent (D1)
- T5 The registry satisfies R1, and a duplicate name fails to load
- T6 Diagnostics cover the offending substring, never the whole intent

The corpus is generated from the registry's own declarations with a fixed seed, so a format or operation added to the shipped table is covered without a new test, and a failing case reproduces from the code alone.

## 15. Examples

Valid, with canonical form:

| Input | Canonical | Result |
|---|---|---|
| `photo.jpg` | `photo.jpg` | Convert to JPEG |
| `photo.webp+quality80` | `photo.webp+quality80` | Quality 80 |
| `photo.webp+q80` | `photo.webp+quality80` | Alias expanded |
| `photo.png,webp` | `photo.png,webp` | Two files from one source |
| `photo.jpg,png` | `photo.jpg,png` | Keep the JPEG, add a PNG |
| `photo.png+keep` from `photo.jpg` | `photo.png+keep` | Keep the original JPEG, add a PNG |
| `photo.jpg,jpg+q60,jpg+q90` | `photo.jpg,jpg+quality60,jpg+quality90` | Kept plus `photo-quality60.jpg`, `photo-quality90.jpg` |
| `clip.mp4+1080p+mute` | `clip.mp4+cap1080+mute` | Downscale only, drop audio |
| `clip.mp4+mute!` | `clip.mp4+mute+now` | No confirmation |
| `clip.mp4+!` | `clip.mp4+ask` | Warned and asked: a dismissed prompt beats a refused rename |
| `clip.mp4+trim1h30s` | `clip.mp4+trim1h30s` | An hour and a half, without the zero minutes |
| `clip.mp4+trim60s-150s` | `clip.mp4+trim1m-2m30s` | Range read, tagged form printed |
| `photo.webp+q80+` | `photo.webp+quality80+ask` | Editor prefilled |
| `notes.tar.gz` -> `notes.zip` | unchanged | Repack an archive into another container |

Invalid:

| Input | Code |
|---|---|
| `photo.jpg++quality80` | `E_TOKEN_EMPTY` |
| `photo.jpg,` | `E_GROUP_EMPTY` |
| `photo.jpg+zzz` | `E_NAME_UNKNOWN` |
| `photo.webp+quality` | `E_VALUE_MISSING` |
| `photo.webp+mute80` | `E_VALUE_UNEXPECTED` |
| `photo.webp+quality1e2` | `E_VALUE_SYNTAX` |
| `photo.png+quality80` | `E_INAPPLICABLE` |
| `photo.jpg+quality80+quality90` | `E_DUPLICATE` |
| `clip.mp4+cap1080+size5mb` | `E_EXCLUSIVE` |
| `clip.mp4+trim1h30` | `E_VALUE_SYNTAX` |
| `photo.jpg` renamed from `photo.jpg` | `E_NOOP` |
| `photo.jpg+5 mb` | `E_CHARSET` |

## 16. Versioning

Version identifier: `retype-grammar-1`.

The canonical form MUST NOT change within a major version, so a form may be accepted later only if the canonical form of everything that already parsed stays the same. Aliases MAY be added; aliases MUST NOT be removed or reused within a major version. Operation keys MUST NOT be reused. The identifier appears in `retype --version`, in journal records and in any file that declares a grammar version.
