# Converting a file

The conversion is the rename. Everything below is that gesture, typed or performed in a file manager.

## From a terminal

```
retype <source> <rest of the new name>
```

```
retype c holiday.heic jpg            # holiday.jpg
retype c holiday.heic jpg+q90        # holiday-quality90.jpg
retype c holiday.heic png,webp       # holiday.png and holiday.webp
```

`c` is short for `convert`. The first argument is the file being converted. The second is what follows the dot in the new name: a format, then zero or more transformations, which [Quality, size and geometry](dials.md) walks through and [Retype intent grammar](../reference/intents.md) defines exactly.

No output path is required, because the rest of the name already determines the output name.

Quoting is rarely needed. The tokens exclude every shell metacharacter except `!`, and `!` never has to be typed: `now` means the same thing.

## Several outputs

A comma starts another output. Each group is one file:

```
retype c holiday.heic png,webp+cap1080
```

That writes `holiday.png` unchanged in size, and `holiday-cap1080.webp`.

A source-format group with no conversion operations keeps the original. The job-wide `+keep` control does the same regardless of which output group carries it:

```
holiday.heic  ->  holiday.heic,jpg       # keep holiday.heic, add holiday.jpg
holiday.heic  ->  holiday.jpg+keep        # the same visible result
```

The original name, including extension alias and case, is preserved. `+keep` works independently of undo. The current `retype convert` path always retains its source, even without `+keep`; watched conversions can consume it only after their safety checks pass.

## Names the source does not need

`-o` names the output. It requires exactly one output group, because one path cannot name several files:

```
retype c holiday.heic jpg+q80 -o card.jpg
```

That is the only way to rename and convert in one step. A name that changes the stem carries no conversion, so a plain rename and a conversion are separate gestures.

## Overwriting

An existing target is an error. Nothing is written, and nothing else in the job runs. The failure names the file that is in the way, and [Errors](../reference/errors.md) gives the status it exits with:

```
retype c holiday.heic jpg --force
```

`--force` replaces existing files. Use it deliberately: the previous file is not kept, and it is the previous file that [undo](jobs.md) would otherwise put back.

## Looking before doing

```
retype c holiday.heic webp+q80 --dry-run
retype explain holiday.heic 'webp+q80'
```

Both print the output name, the plan in the order it runs, the program chosen, the command line it would run, and an estimate. `--dry-run` uses the real source size and so prints a better estimate; `explain` needs no file at all, which is how a pair is tried before there is anything to try it on. The tier printed beside the plan is the claim described in [Fidelity](../advanced/fidelity.md).

## Asking and telling

A trailing `+` parks the job until someone confirms it. A trailing `!` means do it without asking:

```
holiday.heic  ->  holiday.webp+           # ask
holiday.heic  ->  holiday.webp!           # do not ask
```

Asking is about cost rather than permission: a rename that would take a while, or that would consume a source it can no longer restore, stops and waits instead of surprising someone. What it waits for, and for how long, is described in [Deferral](../advanced/deferral.md).

Both together is not an error: asking wins and the `!` is ignored with a warning, because a dismissed prompt costs a keystroke and a refused rename has to be performed again.

## What is written, and what is left alone

- The output is written under a temporary name in its target directory and renamed into place only when the converter exits successfully. An output path is therefore absent or complete, never half a file
- A partial failure keeps completed outputs and preserves the source; it does not automatically roll back successful outputs
- `convert` currently retains its source. A watched conversion removes the original only when its backup, output validation, ownership and durability checks all permit it. `+keep` requests that the original remain visible
- A conversion that takes a while shows the source name until it finishes: the file is put back to its original extension before the work starts, and renamed to the target when it commits

## When the name is wrong

A name that cannot be understood is a diagnosis rather than a guess: the job does not run, the error names the offending part and why, and `retype explain` prints the whole reading. The codes and the exit statuses are in [Errors](../reference/errors.md); the tokens themselves are in [Retype intent grammar](../reference/intents.md).

```
retype explain holiday.heic 'webp+quality'
E_VALUE_MISSING: quality takes a value, in 'quality'
```

## Next

- The dials themselves: [Quality, size and geometry](dials.md)
- The same gesture without a terminal: [Watching a folder](watch.md)
- Reversing it afterwards: [Jobs, undo and history](jobs.md)
