# Watching a folder

A registered folder converts a file whenever its extension changes, from any program that renames it. This page is the working guide; [Watch scope](../reference/watch.md) is the exhaustive specification of what is watched, ignored and done at startup.

## Registering

```
retype watch                            # register the current folder, recursively
retype watch ~/Pictures                 # register another, recursively
retype watch ~/Pictures --non-recursive # this folder only
retype watching                         # list what is registered
retype unwatch ~/Pictures               # stop watching it
```

Registration is idempotent and immediate: a running daemon re-reads the file within a second, so registering twice is not an error and does not convert twice. Registrations are remembered across restarts. Every flag these commands take is in [Command line](../reference/cli.md).

A folder registered inside another is allowed, and its own policy applies to its subtree. Symlinks are resolved, and the resolved path is the identity: registering the same folder by another name is the same registration.

A registration whose folder is later deleted stays in the list, marked unavailable in `retype daemon status`, and is retried every thirty seconds.

## Nothing already there is converted

Registering a folder does not inspect the names of the files in it. A file that already carried intent syntax before registration keeps it.

What does happen is a scan: the daemon walks the folder once to learn which files it has, so that renaming an existing file can be recognised afterwards. The scan reads no file contents and writes nothing. It runs in the background, reports its progress in `daemon status`, and stops if the registration is removed. A folder of a million files delays what it can recognise, never its start.

## What is ignored

The first matching rule wins.

| Rule | Default |
|---|---|
| Retype's own temporary files | Always ignored |
| Version control, build output and cache folders | `.git`, `.hg`, `.svn`, `node_modules`, `target`, `.venv`, `__pycache__`, `System Volume Information`, `$RECYCLE.BIN`, `.Trash`, `.Trashes` |
| Folders whose name begins with a dot | Ignored unless `watch_hidden = true` |
| Your globs, in configuration | `ignore = ['**/Library/Caches/**', '*.tmp']` |
| A folder's own patterns | `patterns = ['*.jpg']` restricts what the folder considers at all |

Your globs add to the built-in list and cannot remove an entry from it. The only way past a built-in entry is to register a folder inside it by name, because naming a folder is intent.

A glob without a slash matches the file name; one with a slash matches the path relative to the registered folder. A rename is matched under both names it has, since they describe the same bytes.

Ignoring a folder prunes its whole subtree. Ignoring a file pattern only skips conversion; the file is still recognised, so it can be renamed back later.

## What a folder may decide for itself

A `.retype.toml` in a watched folder narrows what happens there. It can restrict patterns, refuse operations, clamp a value, turn off confirmation, or pause the folder. It cannot define a tool, a format, an operation or a preset: a folder can arrive from an untrusted source, and a command is not something it may introduce.

```
version = 'retype-config-1'
patterns = ['*.jpg', '*.png']
deny = ['nometa']
limits = { quality = 85 }
paused = true
```

`limits` clamps what was asked for, and the output name says the clamped value, so nothing is silently reduced. `deny` parks a job that asks for a refused operation rather than dropping it. The keys are in [Configuration schema](../reference/config.md).

## Folders retype will not watch

Cloud storage that can leave a file as a placeholder is refused at registration, naming the provider: the file looks local, its contents are elsewhere, and reading it triggers a download. The refusal says so, and `allow_synced = true` in the user configuration overrides it for someone who knows their provider keeps full local copies.

Ordinary sync clients are unaffected. A folder synced by Dropbox with files available offline, by Syncthing, by rsync, or a git worktree, is an ordinary local folder, and the rule is about placeholders rather than about the word "sync".

A dataless file encountered inside an ordinary registration is skipped, logged, and counted, and never converted implicitly.

A network mount is not refused, because a warning that is wrong is better than a refusal that blocks a legitimate setup. It is warned about once at registration: renames on network filesystems are reported unreliably, so a conversion there may be missed. [Platforms](../reference/platforms.md) records which filesystems are supported and to what degree.

## Large files

Automatic conversion is off above four gigabytes per source, because a rename is a fast gesture and a forty gigabyte video is not. An oversized file keeps its name, and the count of what was skipped appears in `daemon status`.

`retype convert` is not affected by the limit. Asking for a large conversion is how it is meant to happen; a rename doing it unasked is not.

## Pausing

```
retype watch --pause
retype watch ~/Pictures --pause
retype watch --resume
retype watch ~/Pictures --resume
```

A paused folder keeps its registration and converts nothing. `paused = true` in the folder's own configuration is stronger: it survives a resume, and resuming it is refused, naming the file to edit. A committed project setting should not be undone by a client that resumed once and left.

Pausing is also what happens by itself when a folder produces more renames than a queue should absorb, for example a build system writing through a watched tree. Above 256 queued jobs, new automatic jobs are dropped with a warning and a count, and the notification says conversion is paused rather than pretending it worked.

## Batching

Renames are collected before anything runs: four hundred milliseconds of arrivals, then two hundred milliseconds of quiet, capped at three seconds. A program that saves by renaming twice is therefore converted once, under the name it finally chose. A folder may raise its own window with `debounce_ms`.

A rename chain inside one window collapses to a single job, `a.heic` to `a.jpg` to `a.webp` being one conversion from the first name to the last. Converting the middle one would re-encode twice and produce a file nobody asked for.

One batch produces one notification, when its last job settles.

## What you are told

```
retype daemon status
```

That reports whether the daemon is running, what it is doing, and the counters behind the answers above: converted, skipped, parked, failed, and placeholders. A parked job is one waiting for an answer, and [Jobs, undo and history](jobs.md) is where it is answered or refused.

Notifications say what a batch produced rather than what it attempted: three converted, or one converted and one failed with the reason, or one parked. The button on the notification is Undo, and it reverses every job of that batch that has a record, which is the same record [Jobs, undo and history](jobs.md) describes.

Where notifications come from depends on the platform: notification actions on Linux with a session bus, plain notifications elsewhere, and the daemon's own output where there is no session at all. A machine with nowhere to show a notification still converts.

## Stopping

```
retype daemon stop           # stop after running work finishes
retype daemon stop --now     # stop now, cancelling it
retype daemon stop --force    # override a supervisor that refuses
```

The same from a signal: the first `SIGTERM` or `SIGINT` stops accepting work and lets running jobs finish, a second cancels them. That is the difference between a supervisor stopping a unit and a person who wants it gone.

Nothing is lost by stopping. Registrations, the queue and the undo records are on disk, and a daemon that exits removes its endpoint, so a missing endpoint means no daemon rather than an unknown one.
