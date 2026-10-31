# Watch scope

What the daemon watches, what it ignores, and what it does at startup.

## 1. Registrations

| Property | Value |
|---|---|
| Unit | A directory, registered by absolute path |
| Canonicalisation | Symlinks resolved; the canonical path is the identity of the registration |
| Recursion | Always recursive. There is no depth limit and no non-recursive mode |
| Nesting | Allowed. A registration inside another is recorded, and its own `.retype.toml` applies to its subtree |
| Persistence | `watches.toml` in the state directory, re-read on daemon start |
| Missing path | A registration whose path is gone stays registered, is marked unavailable in `daemon status`, and is retried every 30 s |
| Count | Bounded by configuration, default 32. Noise beyond that is a misconfiguration, not a use case |

## 2. Directory configuration lookup

`.retype.toml` is resolved by **nearest ancestor**, starting at the file's own directory and walking up no further than the registration root. Results are cached per directory and re-read when the file's modification time moves, which is the configuration watcher's job done without a second watcher; `docs/config.md` §12 says which keys are read.

A file outside every registration root is never converted, so it has no directory configuration.

## 3. Startup

- **Nothing is converted retroactively.** Registering a directory does not convert files that are already there, even if their names carry intent syntax. That would be surprising, and on first registration it would be destructive
- **Identity priming does happen.** The daemon stats every file under the root and records device and inode, because without priming, renaming a file that existed before the daemon started has no identity to correlate and would be silently ignored. Priming reads no file contents and writes nothing
- Priming is incremental and cancellable: it walks in the background, reports progress in `daemon status`, and stops immediately if the registration is removed. A root of a million files delays correlation of unprimed files, it does not delay startup
- A file whose identity is unprimed at rename time is still handled: the `From` and `To` event pair covers it, and if that is unavailable the result is `NeedsAttention`, never silence

## 4. Ignores

Evaluated in order, first match wins.

| Rule | Default |
|---|---|
| Temp files of the tool itself | Always ignored: the `.retype-in.` and `.retype-out.` prefixes |
| Directories in the ignore list | `.git`, `.hg`, `.svn`, `node_modules`, `target`, `.venv`, `__pycache__`, `System Volume Information`, `$RECYCLE.BIN`, `.Trash`, `.Trashes` |
| Hidden directories | Ignored unless `watch_hidden = true` |
| User globs | `ignore = ['**/Library/Caches/**', '*.tmp']` in the user configuration, `docs/config.md` §5 |
| Directory patterns | `patterns = ['*.jpg']` in `.retype.toml`, which restricts what is considered at all |

`ignore` adds to the built-in list and cannot remove an entry from it. The only way past a built-in entry is to register a directory inside it explicitly, because naming a directory is intent.

Ignoring a directory prunes the whole subtree, including identity priming. Ignoring a file pattern only skips conversion; the file is still primed.

A directory whose name does not match user globs is not skipped, only the paths that match.

## 5. Synced folders

A cloud provider that can leave a file as a placeholder is a network filesystem with a proprietary protocol. The file looks local, its contents are elsewhere, and reading it triggers a download. That is the deferred network stage, so it is refused rather than half-supported.

Registration is refused with `E_USAGE_ARGS` when the path is inside a known placeholder-capable provider:

| Platform | Detected roots |
|---|---|
| macOS | `~/Library/CloudStorage/**`, dataless files (`SF_DATALESS`) under a registration |
| Windows | `%USERPROFILE%\OneDrive*`, `\Cloud\` mirrors, `FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS` on a registered file |
| Linux | `gvfs` and FUSE mounts that report `EAGAIN` on read |

The refusal names the provider and says that support is planned as a separate stage. `allow_synced = true` in the user configuration overrides it for people who have checked that their provider keeps full local copies. That key is user configuration only: a directory file may not set it, because a directory that arrives from an untrusted source must not be able to opt the machine into a provider whose placeholders it cannot see.

Ordinary sync clients are unaffected, because there is nothing to detect: Dropbox with files available offline, Syncthing, rsync targets and git worktrees are ordinary local directories, and refusing them would be both wrong and infuriating. The rule is about placeholders, not about the word "sync".

Dataless and recall-on-access files encountered inside an ordinary registration are skipped, logged once at info level, and counted in `daemon status`. They are never converted implicitly.

## 6. Size limits

| Limit | Default | Behaviour over the limit |
|---|---|---|
| Source size for automatic conversion | 4 GiB, `watch.max_source_size` | Skipped, logged, counted. `retype convert` is unaffected |
| Batch size per debounce window | 64 files, fixed | Queue admission is delayed until the next window |

The size limit exists because a rename is a fast gesture and a 40 GiB video is not. Automatic conversion of something that large needs to be asked for, which is what `retype convert` is for.

## 7. Event handling

| Concern | Rule |
|---|---|
| Debounce | Per path, default 500 ms, overridable by `.retype.toml` `debounce_ms` |
| Scope | In-place renames only, per |
| Batching | Renames arriving within one window are collected into one job batch and one notification |
| Ordering | Queue order follows the earliest event in the batch, not arrival order of the last |
| Self-inflicted events | Recognised by temp prefix and by identity, and never treated as user intent |
| Batch order | Within one debounced batch, observe paths that exist before forgetting paths that are gone. Reversed, identity correlation loses every rename, which is invisible on Linux and fatal elsewhere. Measured in `docs/events.md` §1.5 |
| Existence check | A rename fact is dispatched only if the target still exists and is a regular file. A symlink is not followed and a directory is not a candidate, because a directory whose name carries an extension is still a directory and following a link would write outside the tree. The identity strategy produces no fact for a rename followed by a delete, which is the correct answer |
| Data events | A write to a renamed file is not a second trigger. Only rename observations are fed to the correlator |
| Event shape | A platform may report a rename as one event with two paths, as two events with one path each, or both. The feeder handles all three; splitting is not an error |
| Duplicates | Linux reports the same rename as `From`, `To` and `Both`. Facts are deduped by source and target within a batch |
| Unmatched source | A `From` with no `To` by the end of the batch is a file that left the tree, not a rename |
| Chains, same batch | A rename chain inside one batch, `a.heic` to `a.jpg` to `a.webp`, collapses to one job from the first source to the last target, keeping the last intent. Converting the intermediate would re-encode twice and produce a file nobody asked for |
| Visible name during conversion | The file is renamed back to its source extension before converting, so a slow job shows the old name until it commits. Documented rather than surprising; `docs/state.md` §5.4 |
| Chains, later batch | The chain is resolved against the job that already exists: queued or running work is cancelled and retargeted to the original source, finished work is left alone. Sequence in `docs/state.md` §5.3 |

## 8. Churn protection

A directory can produce more rename events than any queue should absorb, for example a build system writing through a watched tree. Rules:

- Above 256 queued jobs, new automatic jobs are dropped with a warning and a counter, and the notification says conversion is paused rather than pretending it succeeded. The cap is fixed; a machine with a different number of cores does not get a different answer
- `pause` and `resume` exist as `retype watch --pause <dir>` and `--resume <dir>`, and as a switch in the directory file (`paused = true`) so a project can ship the setting

A pause is state; `paused = true` in a directory file is policy. Policy wins, because a committed project setting must not be silently undone by a client that resumed once and departed. Resuming a policy-paused directory is refused with `E_WATCH_PAUSED_BY_POLICY`, naming the file to edit, which is the only case where the two can disagree.

## 9. Network filesystems

Out of scope, per the Deferred table. The daemon detects an obvious network mount at registration, warns once, and does not refuse the registration, because a warning that turns out to be wrong is better than a refusal that blocks a legitimate setup.

Event delivery on network mounts is unreliable in both directions, and identity is often synthetic. A user who registers a network path is choosing to rely on the `From` and `To` pair.

## 10. Saying what happened

The daemon notifies once per batch, when its last job settles, and the notification says what the batch produced rather than what it tried to, `docs/config.md` §17.

Capability is a property of the platform, in three tiers:

| Tier | How | Where |
|---|---|---|
| Actions | `notify-send --wait --action` | Linux, with a session bus and a libnotify build that has actions |
| Plain | `notify-send` | Linux, with a session bus |
| Log | a line on the daemon's own stderr | everywhere else, including Windows and macOS until the signed notifier bundle exists |

The tier is decided once at startup, because the session is either there or it is not. A daemon with no notifier still converts: the tier is about being told, never about the work.

Undo is the action, and it runs from the daemon rather than through a URI handler, because `notify-send --wait` answers on its own standard output and there is nothing else to listen to. The Windows toast and the macOS User Notifications centre need an AppUserModelID and a signed bundle; that bundle is the first signed artifact of a later release, not a library call.
