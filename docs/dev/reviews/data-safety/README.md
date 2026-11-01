# Data safety review

## Verdict

**Not safe to sign off against the requirement that originals are deleted only after all requested conversions have unmistakably finished.** The current implementation can delete a source after an empty, invalid, stale or incomplete output; truncate original bytes before conversion; remove the only original backup during cancellation or resume; and silently omit files from a valid archive conversion.

Reviewed production revision: `e422520d4e190470acda64f8a749d9f5b98a8268edc1dfc8c64242abcd818cf6`. The review started at `fb4ee0e` and was refreshed after the subsequent own-rename observation fix. That fix does not change the findings below. Line references refer to the reviewed production revision, not the audit fixtures.

No production behavior was changed. The accompanying adversarial tests are archived outside the normal test suite because they intentionally fail against the reviewed implementation.

## Scope and evidence

Traced source preparation, external-tool execution, native and tool-assisted archive execution, output staging and commit, source disposal, undo creation and restoration, quota eviction, cancellation, supersession, resume, journal durability and startup reconciliation. Checked both immediate CLI jobs and rename/deferred jobs. Read the relevant specifications and existing tests.

Executed on Linux with Rust 1.97.1:

- Existing workspace library tests: **484 passed**
- Existing safety-related CLI integration tests: **39 passed** across `archives`, `convert`, `handles`, `jobs_cancel`, `jobs_resume`, `reconcile`, `retention`, `signals`, `supersede`, `undo` and `watch_convert`
- New adversarial checks: **26 checks, 24 failed safety assertions and 2 passing controls**

`data-safety-audit.log` and `data-safety-additional.log` contain the failing-check evidence. Several assertions test the stronger safety requirement rather than the existing specification; in particular, deletion after partial completion is an intentional existing policy, not an accidental deviation from it.

All filesystem experiments used synthetic files. Some shell fixtures perform a filesystem change at a chosen point to deterministically simulate a concurrent editor or another process. Those checks do not require real encoders to behave maliciously. The ordinary failure control confirms that a nonzero-exiting tool discards its partial output without changing its source. The native archive control confirms ordinary TAR-to-ZIP finalization and content preservation.

Not executed: power-loss or storage-fault injection, Windows/macOS runtime tests, every real encoder/codec, remote filesystem behavior or provider-specific cloud synchronization. Absence of file/directory syncing is established by code inspection, not by a simulated power loss. This review is not a proof of safety against arbitrary privileged writers or faulty storage.

## Current deletion gate

Rename/deferred jobs follow this sequence in `crates/retype-cli/src/daemon/watcher.rs`:

1. Attempt to keep an undo record
2. Prepare the input path
3. Execute and immediately commit each output independently
4. Derive the final job state
5. Consume the input for either `Done` **or `Partial`**, at lines 1114-1117
6. Publish the terminal journal record, at line 1133
7. Remove the undo record for states other than `Done` and `Partial`, at line 1139

The ordinary tool runner's completion predicate is: the direct child exited zero, and the predictable temporary pathname exists. It then records its length and renames it onto the destination (`crates/retype-core/src/execute.rs:128-138`). No decoder validation, nonempty-file requirement, fresh-file identity, final cancellation check, output syncing or directory syncing is part of that predicate.

Immediate CLI jobs normally leave their input path alone. Their lexical source/output path check can be bypassed by a directory alias, however, so they do not provide an unconditional original-preservation guarantee either.

## Findings

### F1. Critical: A stale temporary input can truncate the original before conversion

Locations: `crates/retype-core/src/convert.rs:140-145`, `:155-157`; `crates/retype-core/src/undo.rs:245-250`.

When the restored source name is occupied, input preparation uses the fixed name `.retype-in.<stem>.<format>`. If a crash left that name as a hard link to the current renamed source, the new `hard_link` fails with an existing destination. The code treats every hard-link failure as lack of hard-link support and runs `fs::copy` onto that existing name. Source and destination are then the same inode: on the tested Linux filesystem, `fs::copy` truncates it to zero bytes.

This destroys the original before a converter is invoked. An undo record hard-linked to the same inode is also truncated. A stale symlink or unrelated existing destination can likewise redirect or receive the fallback write.

Evidence: `a_stale_temporary_input_is_not_truncated_through_a_hard_link` observed an empty original where the original byte sequence was expected.

Required change: Exclusively allocate a unique temporary input; never copy over an existing path; distinguish unsupported linking from destination conflicts and other failures. Copy fallback must use an exclusively owned destination and must not follow a preexisting link.

### F2. Critical: Staged outputs have neither freshness nor exclusive ownership

Locations: `crates/retype-core/src/convert.rs:47-80`; `crates/retype-core/src/execute.rs:130-138`; `crates/retype-core/src/archive.rs:315-316`.

`StagedOutput::new` computes `.retype-tmp.<target-name>` but neither creates nor reserves it. An earlier interrupted conversion can leave a partial file there. A new tool that exits zero without writing anything is accepted because the stale file exists, and that file is committed as the new result. Rename jobs then delete their originals.

Two concurrent jobs targeting the same destination share this pathname. One job can overwrite, commit or remove the other's staged output. Dropping a failed second stage was reproduced removing the first stage's completed file.

A preexisting staging symlink to the input is even more destructive: an ordinary output writer follows it and truncates the original, including a hard-linked undo copy. Staging is therefore not isolated from original data.

Evidence: `does_not_accept_a_stale_staged_file_from_an_earlier_run`, `output_staging_does_not_share_a_path_between_jobs`, `a_failed_competing_stage_cannot_delete_another_jobs_output`, `staged_output_symlinks_cannot_truncate_the_original_or_its_backup`.

Required change: Unique, exclusively created staging objects with verified ownership and safe handling for encoders that replace their output file. Reject links and nonregular outputs. Cleanup must only remove an object owned by that job. Serialize or otherwise arbitrate destination ownership across jobs.

### F3. Critical: A valid multi-member gzip TAR conversion silently loses entries

Locations: `crates/retype-core/src/archive.rs:151`, `:172-173`, `:226-258`, `:79-80`.

The native TAR.GZ reader uses `flate2::read::GzDecoder`, which reads one gzip member. A valid gzip stream can contain several members. A two-entry TAR split at an entry boundary and encoded as two gzip members is accepted, but only the first entry reaches the output ZIP. The runner returns success, so a rename/deferred job is eligible to consume the complete source archive despite its incomplete result.

TAR iteration also stops without necessarily draining the gzip decoder through the trailer. A source with a deliberately corrupted gzip checksum was accepted.

Evidence: `carries_every_member_of_a_valid_multi_member_gzip_tar` first proves that `MultiGzDecoder` recovers the complete two-entry source TAR, then observes only one output ZIP entry. `rejects_a_tar_gzip_source_with_an_invalid_gzip_checksum` also fails.

Required change: Read every gzip member and validate the compressed stream to completion, including its trailer. Repacking must verify that all source entries and declared bytes were carried before source disposal is permitted.

### F4. Critical: Atomic rename is mistaken for durable completion

Locations: `crates/retype-core/src/convert.rs:70-73`; `crates/retype-core/src/archive.rs:79-80`; `crates/retype-core/src/undo.rs:245-255`, `:491-502`; `crates/retype-cli/src/daemon/watcher.rs:1114-1133`; `crates/retype-core/src/journal.rs:165-187`.

Converted files, copied undo bytes and undo manifests are not synced before originals are removed. Output and undo directory entries are not synced either. The source is consumed before the terminal journal append. A journal `sync_data` does not make unrelated output files, backup files or directory mutations durable; `Partial` is not one of the journal's durable states at all.

A power loss can therefore leave source deletion persisted while replacement data or its recoverable backup is not. Atomic rename prevents observers seeing an ordinary half-written pathname; it does not establish crash durability. `Store::save` returning success is not evidence of a durable backup.

Evidence: Code inspection; syncing calls in the reviewed code are confined to journal persistence and unrelated state updates, not the conversion/backup commit protocol. No power-loss reproduction was attempted.

Required change: An explicit durable transaction with checked file and directory syncing, durable backup creation, durable output commits, and a durable recovery record before irreversible source disposal. Unsupported durability semantics must fail closed or fall back to retaining the original.

### F5. High: Empty and invalid outputs count as completed conversions

Location: `crates/retype-core/src/execute.rs:128-138`.

A zero-exiting tool that writes an empty file or arbitrary non-WebP bytes gets a successful report. Rename jobs classify it as `Done` and remove their source. The checks do not even require a regular file. Byte length is recorded but never validated.

Evidence: `rejects_an_empty_output_before_deleting_the_source` and `rejects_invalid_format_bytes_before_deleting_the_source` both observed `Done` with the original removed.

Required change: Backend-specific validation appropriate to the requested output, at least a fresh regular file, valid format/container, complete decoding or equivalent structural checks, and required dimensions, streams or entry coverage. Size alone is not proof. Lossy conversions cannot be validated by byte equality with their input.

### F6. High: Partial jobs delete their source, and explicit keep-source groups are ignored

Locations: `crates/retype-cli/src/daemon/watcher.rs:1114-1117`, `:925-940`; `crates/retype-core/src/plan.rs:158-160`; `crates/retype-core/src/output.rs:38-46`.

The deletion branch accepts `Partial`, meaning at least one requested output did not complete. With undo disabled, skipped, failed or later evicted, the missing outputs cannot be completed from the original. Existing documentation and `jobs_resume` tests deliberately support source consumption after partial success: the user's stronger requirement requires changing that policy.

Separately, an intent such as `png,webp` from PNG explicitly keeps its source. Planning correctly excludes the keep group from conversion, but the rename runner consumes the source anyway. The undo manifest also includes every derived group, including the kept source path, rather than only produced outputs.

Evidence: `keeps_the_original_when_one_requested_output_fails` and `honors_an_explicit_keep_source_group`.

Required change: Preserve the original on partial, failed, cancelled and interrupted jobs; preserve explicit keep-source groups. Only all required conversions successfully validated and durably committed may authorize disposal. Keep-source groups must not be recorded as produced files to remove during undo.

### F7. High: Failure to create an undo record does not prevent permanent source removal

Locations: `crates/retype-cli/src/daemon/watcher.rs:911`, `:915-960`, `:1106-1117`; `crates/retype-core/src/execute.rs:194-200`.

Disposal defaults to `Remove`. Unexpected `make_room` or `save` failures are logged, but disposal remains permanent removal. Trash is selected for the specific `TooLarge` policy outcome, not for unexpected storage failure. A save failure with no recoverable backup was reproduced followed by source deletion.

The linked-input path does not honor `Disposal::Trash` either: committing onto the renamed target replaces its source inode, and `Rename::consume` returns success without trashing those bytes; dropping the temporary link can remove their last remaining name. This requires the restored source name to be occupied and an output to replace the renamed target.

Evidence: `keeps_the_source_when_undo_save_fails`; inspection of the linked-input disposal branch.

Required change: Distinguish deliberate no-undo policy from unexpected preservation failure. Unexpected backup failure must retain the source or use a successfully verified recoverable fallback, never silently downgrade to permanent removal. Linked inputs need the same preservation/disposal contract as restored inputs.

### F8. High: Child-process exit and cancellation do not delimit the final write

Locations: `crates/retype-core/src/execute.rs:350-389`, `:128-138`, `:247-250`; `crates/retype-core/src/archive.rs:71-80`.

The runner waits for the direct child and its stderr pipe, not all processes writing the output. A helper with redirected stderr can keep writing after its parent exits. A reproduced helper appended its remaining bytes to the old temporary pathname after commit; the visible output remained partial while the original was deleted.

Conversely, when the parent has already exited but a helper still holds stderr, `reader.join` is uninterruptible by the cancellation flag. A cancellation received during that wait is not checked again before output commit. Native repacking also has no final cancellation check between finishing its last entry and committing the archive. On Windows, `kill` only kills the direct child, unlike the documented whole-process-group contract.

Evidence: `waits_for_a_detached_writer_before_committing_and_deleting_the_source`; the cancellation fixture in F9. Windows behavior and the native final-commit window are inspection findings only.

Required change: Define and enforce supported tool process-lifetime contracts, control helpers on supported platforms, and validate final output completion. Cancellation must participate in a synchronized commit decision, not merely be polled while the direct child is alive.

### F9. High: Cancellation can delete the only remaining original backup

Locations: `crates/retype-core/src/convert.rs:124-128`; `crates/retype-core/src/execute.rs:208-211`; `crates/retype-cli/src/daemon/watcher.rs:1087-1139`, `:647-664`; `crates/retype-cli/src/daemon/immediate.rs:336-340`.

The job removes its undo record whenever its final state is not `Done` or `Partial`, even if an earlier output committed. With a linked input, an output can have replaced the renamed target already; the old source name belongs to another file. `keep` cannot restore it, `PreparedInput::drop` unlinks its temporary input, and the cancelled-state cleanup removes the undo copy. The original bytes then have no remaining name.

The audit reproduced this with a zero-exiting parent, a helper retaining stderr and cancellation during the stderr join. The job ended `Cancelled`, the unrelated occupied source name remained, the output had committed, and `Store::source` found no original backup.

Supersession independently removes an earlier job's undo record immediately after setting its cancellation flag, without waiting for it to stop. This is the same unsafe assumption that no commit can have happened or still happen. Immediate cancellation also forgets its undo record despite earlier committed outputs, though its ordinary input normally remains.

Required change: Base cleanup on durable committed-output and source-preservation facts, not terminal state alone. Never unlink the last original-bearing input or backup merely because a job was cancelled or superseded. Retain recovery records whenever any output committed or input relabelling remains unresolved.

### F10. High: Path-based disposal and commit can destroy unrelated files

Locations: `crates/retype-core/src/execute.rs:220-227`; `crates/retype-cli/src/daemon/watcher.rs:1026-1028`; `crates/retype-core/src/convert.rs:70-71`, `:168-171`; `crates/retype-core/src/fsx.rs:17-43`.

The final removal acts on a pathname without checking that it still holds the input that was converted. Moving the input aside and creating a new file at that name before the runner returns resulted in deletion of the new file.

Output collision checks happen before a potentially long conversion. Commit then uses an overwriting rename with no no-clobber condition. A file created at the destination during conversion was overwritten without `--force`. Restore similarly checks `exists` then performs an overwriting rename. The intervening race can clobber a destination, and `exists` does not recognize a dangling symlink as occupied; that case was reproduced without a race.

Evidence: `source_disposal_does_not_delete_a_file_that_replaced_the_input`, `an_output_that_appears_while_converting_is_not_overwritten_without_force`, `a_dangling_symlink_is_an_occupied_restore_destination`.

Required change: Explicit source/destination ownership and identity, atomic no-clobber operations where overwriting was not authorized, and link-aware checks. A second pathname identity check immediately before unlink is still a check/unlink race; ownership needs an atomic claim or another preservation-first strategy.

### F11. High: CLI source/output aliases bypass the original-preservation check

Location: `crates/retype-cli/src/daemon/immediate.rs:121-137`; the corresponding dry-run check is in `crates/retype-cli/src/explain.rs:107-124`.

`std::path::absolute` makes paths absolute but does not resolve directory aliases. Input `<dir>/photo.png` and output `<dir>/alias/photo.png`, where `alias` points back to `<dir>`, compare unequal while naming the same directory entry. With `--force`, an immediate job writes the conversion over the original. Its undo subject has `consume_source = false`, so it intentionally kept no original bytes.

Evidence: `output_paths_through_a_directory_symlink_cannot_overwrite_the_input` executed the immediate job after validation accepted it and observed the original replaced with converted bytes.

Required change: Resolve and compare effective directory-entry identity, including directory symlinks/junctions and platform case handling, and defend against aliases changing during execution. `--force` must not override protection of the job's own input.

### F12. High: Resume can convert a different source and then delete the original backup

Locations: `crates/retype-cli/src/daemon/resume.rs:179-186`, `:169-174`.

`source_bytes` accepts any existing file at the old source pathname, without checking the recorded source identity, size, mtime or content. After a partial rename consumed the source, creating an unrelated file at that name caused resume to produce its missing output from that unrelated file rather than from the undo bytes.

A resume reaching `Done` then unconditionally deletes the undo record. Even an otherwise successful resume deletes the remaining original backup and makes normal undo unavailable; the comment incorrectly claims that the source still exists. The current `jobs_resume` integration test explicitly expects this deletion, so preserving ordinary undo after resume also requires a policy correction.

Evidence: `resume_reads_the_recorded_original_not_a_new_file_at_the_source_name`, `finishing_a_partial_resume_keeps_the_original_backup_for_undo`.

Required change: Verify source identity/content or read the job's immutable retained input. Keep the original backup through normal undo retention after resume. Reaching `Done` is not authorization to destroy the original backup.

### F13. High: Undo deletes unproduced or replaced files

Locations: `crates/retype-cli/src/daemon/watcher.rs:933-940`; `crates/retype-core/src/undo.rs:357-367`, `:479-486`.

The manifest names all intended outputs before execution; it is not updated with which outputs actually committed or their identities. Undo unlinks every listed pathname. A file that blocked a partial output and was never produced by the job was deleted by undo. An unrelated file later placed at an old output pathname was deleted as well.

Undo removes outputs before the restored source has been successfully carried back and synced. An error during cross-filesystem copying retains the store's original in ordinary execution, but a power loss has no durable restoration protocol to ensure the remaining copy survives before outputs disappear. Restore destinations also have the F10 no-clobber race.

Evidence: `undo_does_not_remove_a_blocking_file_the_partial_job_never_produced`, `undo_does_not_delete_a_new_file_at_an_old_output_path`; restoration ordering is established by inspection.

Required change: Persist only committed output identities and verify ownership before deleting anything. Stage and durably restore the original before removing converted outputs or the retained original record.

### F14. High: Undo records are not immutable and can be evicted while still needed

Locations: `crates/retype-core/src/undo.rs:245-250`, `:407-461`; `crates/retype-cli/src/daemon/queue.rs:32-37`, `:252-275`.

A hard-linked undo record shares the live source inode, so an in-place edit changes the retained original too. This compounds the staging and temporary-input truncation failures: nominally having an undo record does not imply having unchanged original bytes.

Quota eviction has no concept of active jobs or pinned records. Another job's `make_room` can delete a running job's undo record. The check, eviction and subsequent save are not a shared transaction, so concurrent runners also race quota accounting and record creation. The active-job eviction was reproduced by adding a `Running` job and requesting room for the next source.

Evidence: `undo_backup_is_a_snapshot_not_mutable_through_the_live_source`, `quota_eviction_does_not_remove_an_active_jobs_backup`. Concurrent quota oversubscription is an inspection finding, not a separate stress-test result.

Required change: Immutable input snapshots or an explicitly enforced no-mutation contract; pin records used by running/resuming/recovering jobs; serialize quota reservations, eviction and record creation. Expiry or quota pressure must never remove the only original-bearing recovery object for an unfinished job.

### F15. Medium: Startup reconciliation does not implement the documented interrupted-job recovery

Locations: `crates/retype-cli/src/daemon/reconcile.rs:36`, `:93-101`; `crates/retype-cli/src/daemon/server.rs:489-499`; `crates/retype-cli/src/daemon/watcher.rs:1030-1082`, `:1133`.

Per-output completion is held in local memory until the final job snapshot. If the daemon dies after one output commit, the journal still contains a running job without its completed-output identity. Reconciliation marks it `Failed`, while the resume endpoint accepts only `Partial`. The comment that `jobs resume` handles these interrupted outputs is not implemented. There is no durable per-output commit ledger to resolve this ambiguity.

Evidence: Code inspection. Existing reconciliation tests verify interruption classification, not reconstruction of completed outputs or successful interrupted-job resume.

Required change: Persist recoverable output commit facts and source-preservation state, and reconcile them into a resumable transaction without guessing from pathname existence. Journal persistence must be a checked prerequisite for irreversible cleanup, not a log-only failure discovered afterwards.

## Required safety contract

Source disposal must require all of the following facts, represented explicitly rather than inferred from a broad job-state enum:

1. Every requested non-keep output was produced by this job, validated and completely finalized
2. The input bytes used for every output are the same verified original snapshot
3. Every committed output and required backup is durably persisted, including relevant directory entries
4. Recovery metadata describing those commits and the retained original is durably persisted
5. The job has neither a cancellation/supersession outcome nor an unresolved partial/interrupted outcome
6. No group requests retention of the source
7. The disposal operation owns the original object and cannot delete an unrelated replacement at its former name
8. Backup failure, validation failure, ambiguous ownership or unavailable durability retains the original and reports the failure

The linked-input path must satisfy the same contract; overwriting the renamed target is already source disposal even when no explicit `remove_file` call follows. Backup expiry and undo also need their own checked, durable disposal contracts.

No single added `exists`, size check, sleep, final cancellation poll or `fsync` call repairs this as a whole. The commit boundary, source snapshot, ownership and recovery model need to be designed together. Retaining originals on failure/partial completion does not require rolling back successful outputs.

## Recommended order

1. Stop irreversible source cleanup on partial/cancelled/interrupted jobs, explicit keep-source groups and unexpected backup failures
2. Fix exclusive input/output staging and the stale-hard-link truncation path
3. Fix multi-member gzip reading and complete archive validation
4. Establish the immutable-source, output-validation and durable commit protocol
5. Fix atomic destination ownership, source aliases and disposal identity
6. Repair cancellation/supersession cleanup, resume, undo and active-record pinning
7. Add crash/storage-fault tests and platform-specific validation before making a safety claim

Changing the partial-completion and resume-retention policies needs an explicit specification decision, because existing documentation/tests encode the current destructive behavior.

Until then, do not use rename/deferred conversion on the only copy of important data. A separate, independent backup is necessary; simply enabling undo is insufficient. The ordinary non-overwriting CLI conversion path is less destructive, but aliases and unsafe staging still require caution.

## Reproducing the archived checks

Use a separate worktree. The tests use only dependencies already declared by `retype-cli`; no new packages are needed. Copy both archived `.rs` files into that worktree's `crates/retype-cli/tests/` under the same names, refusing to overwrite existing files. Set `CARGO_TARGET_DIR` to the main repository's `target/` directory.

Run separately so Cargo's early stop after a failed test binary does not hide the second suite:

```sh
cargo test -p retype-cli --test data_safety_audit -- --test-threads=1
cargo test -p retype-cli --test data_safety_audit_additional -- --test-threads=1
```

Both commands are expected to exit 101 at the reviewed revision. The scripts have Unix-specific checks and are gated with `cfg(unix)`; a Windows run would not exercise them. The log files are the final runs against the reviewed production revision.
