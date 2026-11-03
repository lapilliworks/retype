# Provenance

The rename consumes its own syntax. After conversion the file is `holiday.jpg`, and nothing on it says whether it was quality 85 or 40, whether metadata was stripped, or which tool wrote it. Provenance is the record that answers those questions. This page explains what is recorded, where it is kept, how to read it, and what it cannot promise.

## 1. What is recorded

Provenance is the record of what was done to a file: the canonical intent string, the tool that ran it, the tool's version and build fingerprint, the tier of the conversion, the resolved engine where there is one, and when it happened.

It is not:

- A content hash, so it does not detect later edits
- A signature, so it does not authenticate anything
- An integrity check, so it does not verify the bytes
- A portability guarantee. Every mechanism below loses the record under some common operation

## 2. Why it matters

Four questions make the record worth keeping:

- **Explain.** "Why is this 900 KB?" and "was the GPS data removed?" are answerable only from a record
- **Avoid compounding loss.** Re-encoding a `quality60` JPEG at `quality85` does not restore anything; it loses again. Knowing the history stops the second loss
- **Reproduce.** A delivered file can be regenerated at different settings if the intent and the source are known
- **Audit.** Delivery workflows have settings that clients or colleagues ask about later, and a professional needs an answer that is not "I think it was 80"

## 3. Where the record is kept

| Mechanism | Survives rename | Survives move, same volume | Survives copy, other volume | Survives zip, upload, mail | Survives machine change | Touches the file |
|---|---|---|---|---|---|---|
| Extended attribute | yes | yes | no | no | no | yes |
| NTFS alternate data stream | yes | yes | NTFS only | no | NTFS only | yes |
| Identity index in state | yes | yes | no | no | no | no |
| Sidecar file | only if renamed too | only if moved too | yes | yes | yes | yes, visibly |

No mechanism survives everything, so provenance is best effort everywhere, and no feature is gated on a record being present.

The identity index is the mechanism of record on every platform. At completion the job stores the identity of each output: device and inode on Unix, volume serial and file id on Windows, plus the size. It costs nothing, never touches the file, survives the rename that usually follows a conversion, and cannot be stripped or corrupted by another program. Its limits are accepted: file ids are recycled, so a record is trusted only when the current file also matches the recorded size; a copy to another volume produces a new identity; and the record is local to the machine.

An extended attribute carries the intent on the file itself where the platform has one: `user.retype.intent` on Linux and `com.retype.intent` on macOS. It survives a rename and a move within the same filesystem, but it is lost by a copy to another filesystem, by an archive, and by an upload. On Windows the equivalent is an alternate data stream, which is not written until that question is settled.

## 4. How to see the record

```
retype why <path>
retype why --json <path>
```

`why` answers from the first store that has an answer and names which one: `journal-identity`, `journal-path`, `xattr`, or `unknown`.

| Answer | Meaning |
|---|---|
| `journal-identity` | This is the file a record produced. Survives the rename that usually follows |
| `journal-path` | Only the name matched a record |
| `xattr` | The intent came from the file's extended attribute |
| `unknown` | Nothing matched. Not an error: a file may have been made elsewhere |

The fields are `path`, `intent`, `tool`, `engine`, `tier`, `created`, `outputs`, `store` and `confidence`. `confidence` is `exact` when the identity matches, `path-only` when only the path matched, and `none` for an unknown answer.

Identity matches first and the name only afterwards, because a converted file is usually renamed straight after and the recorded name is then wrong. An identity match also requires the recorded size to agree, since file ids are recycled. An attribute answers with the intent alone, because that is what it holds and what a file cannot say for itself. It is consulted only when the record has nothing to say about the path.

`engine` is the resolved engine where a tool declares named engines, because the same intent through a different PDF engine is a different file. It is recorded with the job rather than derived at query time: the machine may have gained or lost an engine since.

## 5. Decision rules

1. The identity index is always written. No configuration disables it
2. An extended attribute is written when the platform supports it and the user has not turned it off. Windows streams are not written until that question is settled
3. Nothing is read as a precondition. A missing record never blocks a conversion, an undo, or any other feature
4. `retype why` answers from the first store that has an answer, and reports which one
5. A file converted before provenance existed answers `unknown, predates provenance`. There is no inference from file contents, extension or size
6. Nothing is written outside the target file and the state directory
7. The `Zone.Identifier` stream, which carries the Mark-of-the-Web, is never written

## 6. Retention

Journal records are pruned at twice the undo retention with a floor of 30 days. Provenance has the same horizon, which is deliberate: a record of a conversion from months ago is more likely to mislead than to help, given file id recycling and file reuse.

Extended attributes and streams outlive the journal because they are on the file, but they are not consulted when the journal says a different job produced that path.

## 7. Visibility

How easily a user can find the record differs by platform.

| Platform | To hand | Practical path |
|---|---|---|
| macOS | Extended attribute | `xattr -l`, `mdls`, and the Where-from entry in Finder's Get Info for downloaded files |
| macOS, Linux | Shell history | The `mv` or the `retype` invocation that produced the file is often still in history, which is the most-used provenance tool in practice |
| Linux | Extended attribute | `getfattr -d` |
| Windows | Nothing | File properties show no stream, and there is no shell equivalent of `xattr`. Windows users therefore see the record through `retype why` rather than through the file manager |

On Unix-like systems the record is reachable with tools users already have. On Windows, writing it is not enough, which is why shell integration matters there.
