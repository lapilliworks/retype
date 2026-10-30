# Provenance

What `retype` records about a completed conversion, where it keeps it, and what it refuses to promise.

## 1. Definition

Provenance is the record of *what was done to a file*: the canonical intent string, the backend that ran it, the backend's version and build fingerprint, the tier of the conversion, and when.

It is not:

- A content hash, so it does not detect later edits
- A signature, so it does not authenticate anything
- An integrity check, so it does not verify the bytes
- A portability guarantee. Every mechanism below loses the record under some common operation

## 2. Why it matters

The rename gesture consumes its own syntax. After conversion the file is `holiday.jpg` and nothing on it says whether it was quality 85 or 40, whether metadata was stripped, whether it passed through AVIF first, or which tool wrote it. Four consequences:

- **Explain.** "Why is this 900 KB?" and "was the GPS data removed?" are answerable only from a record
- **Avoid compounding loss.** Re-encoding a `quality60` JPEG at `quality85` does not restore anything; it loses again. Knowing the history stops the second loss
- **Reproduce.** A delivered file can be regenerated at different settings if the intent and the source are known
- **Audit.** Delivery workflows have settings that clients or colleagues ask about later, and a professional needs an answer that is not "I think it was 80"

## 3. Mechanisms

| Mechanism | Survives rename | Survives move, same volume | Survives copy, other volume | Survives zip, upload, mail | Survives machine change | Touches the file | Install cost |
|---|---|---|---|---|---|---|---|
| Extended attribute (`user.*`, `com.*`) | yes | yes | no | no | no | yes | none |
| BSD `extattr` (`user` namespace) | yes | yes | no | no | no | yes | none |
| NTFS alternate data stream | yes | yes | NTFS only | no | NTFS only | yes | none |
| Identity index in state | yes | yes | no | no | no | no | none |
| Sidecar file | only if renamed too | only if moved too | yes | yes | yes | yes, visibly | clutter |
| Shell property handler | yes | on NTFS | on NTFS | no | on NTFS | yes | signed DLL |

No column is all-yes. Provenance is therefore best effort everywhere, and the product never depends on it: no feature may be gated on a record being present.

## 3.1 Visibility

The mechanisms differ in how easily a user can find the record, and the platforms differ more.

| Platform | To hand | Practical path |
|---|---|---|
| macOS | Extended attribute | `xattr -l`, `mdls`, and the Where-from entry in Finder's Get Info for downloaded files |
| macOS, Linux | Shell history | The `mv` or the `retype` invocation that produced the file is often still in history, which is the most-used provenance tool in practice |
| Linux | Extended attribute | `getfattr -d` |
| BSD | `extattr` | `getextattr` |
| Windows | Nothing | File properties show no stream, `dir` needs `/r`, and there is no shell equivalent of `xattr`. Windows users therefore never see the record without our own interface |

The asymmetry explains the priorities: on Unix-like systems the record is reachable with tools users already have, so writing it is enough. On Windows, writing it is not enough, which is why shell integration is a requirement there rather than a nicety.

## 4. Identity index

The mechanism of record on every platform. At completion, the job stores the identity of each output:

- Unix: `st_dev` and `st_ino`, plus size and birth time when available (`statx` on Linux)
- Windows: volume serial from `GetVolumeInformation` and the file id from `GetFileInformationByHandleEx(FileIdInfo)`, which is 64-bit on NTFS and 128-bit on ReFS

Used because:

- It costs nothing, runs once, and never touches the file
- It survives rename and move within a volume, which is the case that matters: a converted file gets renamed by the user immediately afterwards
- It cannot be flagged by security software, cannot be stripped by an upload, and cannot corrupt a file

Its limits, accepted:

- File ids are recycled. A record is only trusted when the current file also matches the recorded size and creation time
- Copying to another volume produces a new identity, so the record is lost. This is the same failure as an upload
- It is machine-local, because the state directory is

## 5. Windows

Three candidate mechanisms, one accepted.

### 5.1 Alternate data streams

An NTFS file is a record containing one unnamed data stream, which is what every program sees as "the file", plus any number of named streams, addressed as `path:name:$DATA`. The API is `CreateFile` on the colon-qualified path, so `holiday.jpg:retype.intent` is a separate stream on the same file record. Streams are enumerated with `FindFirstStreamW`, and `dir /r` lists them.

Properties that matter here:

- A stream is part of the file record, so it follows renames and same-volume moves for free
- The stream is not part of the file's logical size, so size accounting, checksums made by other tools, and most copy utilities ignore it. It does consume allocation
- `CopyFile` and Explorer copy preserve streams within NTFS; most other paths do not. Archives, browsers, mail and non-NTFS destinations drop them
- Stream names are limited and may not contain `:`, `\` or `/`
- Streams named `Zone.Identifier` carry the Mark-of-the-Web. A program must never write that name. Doing so to an already-marked file loosens a browser safety property
- Endpoint security commonly treats a newly created stream on a document as suspicious, because hiding payloads in streams is a known malware technique. Creation can be blocked or silently stripped by policy

Value for provenance: it is the only mechanism that carries the record *with the file* while the file stays on NTFS, including onto a USB stick formatted NTFS, and it survives our state directory being wiped. That is exactly the case the identity index cannot cover.

Hazards: silent loss is indistinguishable from never having written it; the record is invisible in Explorer; security software may object; and it is a write to a file the user asked to convert, which is a side effect beyond converting it.

### 5.2 Identity index

Covered in 4. On Windows it requires no stream, no policy, and no visibility, and it is the mechanism that works on exFAT cards, which is where camera media lives.

## 6. Decision rules

1. The identity index is always written. No configuration disables it
2. An extended attribute or stream is written when the platform supports it and the user has not turned it off. On Linux this is `user.retype.intent`, on macOS `com.retype.intent`. On Windows it is disabled until the ADS question is settled
3. Nothing is read as a precondition. A missing record never blocks a conversion, an undo, or any other feature
4. `retype why` answers from the first store that has an answer, and reports which one: `journal-identity`, `xattr`, `stream`, or `unknown`
5. A file converted before provenance existed answers `unknown, predates provenance`. No inference from file contents, extension, or size
6. Nothing is written outside the target file and the state directory
7. Never write `Zone.Identifier` or any name outside the `retype.*` prefix

## 7. Retention

Journal records are pruned at twice the undo retention with a floor of 30 days. Provenance therefore has the same horizon, which is deliberate: a record of a conversion from months ago is more likely to mislead than to help, given file id recycling and file reuse. Extended attributes and streams outlive the journal because they are on the file, but they are not consulted when the journal says a different job produced that path.

## 8. Interface

```
retype why <path>            # human readable, names the store
retype why --json <path>     # stable fields for scripts
```

Fields: `path`, `intent`, `backend`, `tier`, `created`, `outputs`, `store`, `confidence`, where `confidence` is `exact` when identity matches, `path-only` when only the path matched, and `none` for an unknown answer.
