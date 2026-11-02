# Data safety fixes and regression evidence

Initial implementation response to [F1–F15](../README.md), at the revision recorded below. See [the subsequent platform-test follow-up](../platform-follow-up/README.md) for later fixes, expanded Windows coverage and real-tool validation. The original review remains a historical record of the pre-fix implementation.

## Disposal policy

Original disposal requires all requested non-keep outputs to have completed, passed supported content validation, and been durably published. The retained original, publication ledger and completion record must be durable. Recheck the retained bytes and every committed output, then claim and verify the source before unlinking it. Cancellation and destructive decisions share a commit gate.

Partial, failed, cancelled and superseded jobs retain originals. Explicit keep-source groups retain originals. Disabled undo, insufficient quota, failed preservation, changed files and unavailable directory durability retain originals. Resume retains its backup for undo. Unfinished records remain pinned against quota and expiry.

External backends currently never authorize original disposal: decoding checks alone do not establish all source-to-output properties. They can publish a fresh result after successful execution, but retain the input. Validated native ZIP/TAR/TAR.GZ conversions exercise the positive disposal path. Windows currently refuses destructive cleanup because directory-entry durability is not implemented.

## Validation results

Fixed code and test revision: `b0385f9`. Subsequent changes in this response are documentation and evidence only.

Executed on Linux with Rust 1.97.1:

- Workspace unit and integration tests: **713 passed**, no failures or ignored tests
- Focused safety suites: **57 passed**, including controls and subprocess helper entry points
- Baseline-compatible suites at `b0c3b474ac4d2b0bdbfdaa9085c435dcc8f6e485b220e479949797d286c0b99a`: **30 failed assertions, 3 passed**; both binaries compiled and exited 101
- The same 33 baseline-compatible tests with the fixes: **33 passed**
- Clippy: passed with `-D warnings -A clippy::type_complexity`; the waiver covers the existing warning in `crates/retype-cli/src/list.rs`

Thirty failed assertions are not thirty independent defects. The original two passing controls and an inactive subprocess helper account for the three baseline passes. The additional transaction and ownership suites exercise new recovery APIs, real archive contents, concurrent operations and process exits.

Evidence:

- [Baseline filesystem and runner failures](baseline-audit.log)
- [Baseline archive and undo failures](baseline-additional.log)
- [Passing CLI safety suites](safety-tests.log)
- [Passing ownership suite](ownership-tests.log)

Full workspace and clippy execution logs are retained in `worktrees/data-safety-fixes/workspace-tests-final.log` and `worktrees/data-safety-fixes/clippy-changes.log`.

## Finding coverage

Test prefixes below identify the live suites:

- `A`: [data_safety_audit.rs](../../../../../crates/retype-cli/tests/data_safety_audit.rs)
- `B`: [data_safety_audit_additional.rs](../../../../../crates/retype-cli/tests/data_safety_audit_additional.rs)
- `T`: [data_safety_transactions.rs](../../../../../crates/retype-cli/tests/data_safety_transactions.rs)
- `S`: [core ownership tests](../../../../../crates/retype-core/tests/safety.rs)

### F1: Temporary input truncates original

Private, exclusively created input snapshots replace predictable hard links and overwrite-prone copy fallbacks. Occupied source names, including dangling symlinks, are never replaced during restoration.

- `A::a_stale_temporary_input_is_not_truncated_through_a_hard_link`
- `A::a_dangling_symlink_is_an_occupied_restore_destination`
- `S::snapshot_creation_never_overwrites_an_existing_copy`

### F2: Staging lacks freshness and ownership

Every stage has an exclusively owned private directory on the destination filesystem. Publication requires a fresh regular file. Cleanup owns only that stage; ordinary publication is atomic no-replace.

- `A::does_not_accept_a_stale_staged_file_from_an_earlier_run`
- `A::output_staging_does_not_share_a_path_between_jobs`
- `B::staged_output_symlinks_cannot_truncate_the_original_or_its_backup`
- `B::a_failed_competing_stage_cannot_delete_another_jobs_output`

### F3: TAR.GZ repack omits entries or ignores corruption

Read every gzip member, drain and validate trailers, reject unexpected trailing data, and check entry completion. Native writer finalization is checked before publication.

- `B::carries_every_member_of_a_valid_multi_member_gzip_tar`
- `B::rejects_a_tar_gzip_source_with_an_invalid_gzip_checksum`
- `T::a_verified_native_conversion_disposes_only_after_all_entries_are_committed`

### F4: Commit is not crash-durable

Sync native outputs and manifests on their writing handles, check file and directory syncing, and persist publication intent and completion before disposal. Recovery metadata failure prevents publication or source removal.

- `A::loss_of_the_journal_directory_prevents_publication_and_source_disposal`
- `T::a_manifest_write_failure_prevents_output_publication`
- `T::crash_before_publication_leaves_a_verified_source_and_resumable_intent`
- `T::crash_after_source_disposal_still_leaves_complete_outputs_and_an_undoable_original`

These are filesystem-failure and process-exit checks, not power-loss or storage-device fault injection.

### F5: Invalid output counts as disposable success

Reject missing, empty, nonregular and changed results. Native archive completion is validated; external backend reports remain unvalidated for disposal even when basic decoding succeeds.

- `A::rejects_an_empty_output_before_deleting_the_source`
- `A::an_unvalidated_custom_output_cannot_authorize_source_deletion`
- `T::a_verified_native_conversion_disposes_only_after_all_entries_are_committed`

### F6: Partial or keep-source conversions consume originals

Disposal requires all requested outputs, not merely partial success. An explicit keep-source group overrides disposal.

- `A::keeps_the_original_when_one_requested_output_fails`
- `A::honors_an_explicit_keep_source_group`
- `T::a_partial_native_conversion_retains_the_original_even_with_a_valid_first_output`
- `T::a_native_keep_source_group_prevents_disposal`

### F7: Preservation failure permits permanent deletion

Missing capacity, disabled undo and unexpected backup failures retain the original. Input protection also covers an original still occupying the requested target after source-name restoration was blocked.

- `A::keeps_the_source_when_undo_save_fails`
- `A::an_occupied_source_name_and_missing_undo_capacity_do_not_allow_replacing_the_original`
- `T::failed_backup_creation_cannot_authorize_even_a_valid_native_conversion_to_delete`
- `T::an_undo_policy_without_a_record_retains_the_native_source`

### F8: Parent exit is mistaken for writer completion

Track Unix process groups and Windows Job Objects, reject surviving helpers, bound stderr draining, and serialize cancellation with publication. Windows children start suspended until containment is assigned.

- `A::rejects_a_parent_that_exits_before_its_detached_writer_finishes`
- `T::cancellation_before_native_finalization_prevents_publication`
- `T::cancellation_racing_a_commit_is_ordered_before_any_later_disposal`

### F9: Cancellation or supersession discards original-bearing recovery

Cancellation stops the worker without deleting recovery needed for committed outputs. Supersession no longer removes the earlier worker's backup while it may still be active.

- `A::cancellation_after_one_output_keeps_the_input_and_undo_for_committed_outputs`
- Existing `signals.rs` and `supersede.rs` integration tests now require retained source bytes and recovery records

### F10: Destructive operations use unowned pathnames

Atomic no-replace publication prevents late destination clobbering. Destructive cleanup first claims a pathname into a persistent recovery directory, then verifies identity and content before unlinking. Changed occupants are restored without clobbering, or retained at a reported recovery path. Disposal rechecks every committed output.

- `S::a_late_destination_cannot_be_clobbered_by_publication`
- `S::disposal_claims_and_checks_a_replacement_before_unlinking`
- `S::disposal_detects_an_in_place_edit_even_with_the_same_length`
- `T::valid_native_output_does_not_authorize_removing_a_replacement_source`
- `T::a_published_output_that_changes_cannot_authorize_source_disposal`

### F11: CLI output aliases bypass source protection

Effective parent paths and file identities protect the input, including directory symlinks and hard links. Explicit force does not override source protection. Dry-run uses the same alias checks.

- `A::output_paths_through_a_directory_symlink_cannot_overwrite_the_input`
- `A::a_forced_output_hard_link_to_the_input_is_refused`

### F12: Resume reads the wrong bytes or deletes the backup

Resume uses a private working copy of the digest-verified retained original. It does not trust a replacement at the old source name or pass the recovery copy directly to a converter. Successful resume retains normal undo recovery.

- `A::resume_reads_the_recorded_original_not_a_new_file_at_the_source_name`
- `A::a_resumed_tool_cannot_modify_the_retained_original_through_its_input`
- `A::finishing_a_partial_resume_keeps_the_original_backup_for_undo`
- Existing `jobs_resume.rs` verifies undo after successful resume

### F13: Undo removes unrelated files or removes outputs before restoration

Undo verifies committed identities and digests, restores or verifies the source durably first, and removes only unchanged job-owned outputs. A per-job OS lease refuses undo while execution or recovery can still publish or dispose files.

- `A::undo_does_not_remove_a_blocking_file_the_partial_job_never_produced`
- `B::undo_does_not_delete_a_new_file_at_an_old_output_path`
- `T::undo_restore_failure_after_a_cross_directory_rename_does_not_delete_outputs`
- `A::undo_cannot_remove_a_running_jobs_snapshot_or_race_its_publication`

### F14: Backups mutate or are evicted while active

Independent copies replace hard links. Content fingerprints detect changes. Process and OS locks serialize quota reservation and store mutations; persistent pins protect unfinished records, and OS leases exclude active execution from destructive undo or removal.

- `B::undo_backup_is_a_snapshot_not_mutable_through_the_live_source`
- `A::quota_eviction_does_not_remove_an_active_jobs_backup`
- `T::competing_runners_cannot_evict_each_others_newly_reserved_snapshots`
- `T::unfinished_recovery_records_survive_expiry`
- `A::undo_cannot_remove_a_running_jobs_snapshot_or_race_its_publication`

### F15: Interrupted recovery lacks commit evidence

Persist staged identities and fingerprints before publication, sync running checkpoints, and reconstruct only verified commits at startup. Partial jobs and failed interrupted jobs can resume from retained input; a substituted output is not trusted.

- `A::an_actual_interrupted_runner_recovers_its_first_output_instead_of_losing_resume_state`
- `T::crash_after_publication_recovers_the_committed_identity_and_resumes_only_the_missing_output`
- `T::recovery_does_not_trust_a_substituted_output_at_a_committed_path`

The baseline-compatible test kills a real runner while its second output is running, then opens the same state directory and checks recovery and resume. Additional child-process tests exit before publication, after publication and after disposal.

## Reproduction

From the repository root, run the fixed suites:

```sh
export CARGO_TARGET_DIR="$PWD/target"
cargo test -p retype-cli --test data_safety_audit --test data_safety_audit_additional --test data_safety_transactions
cargo test -p retype-core --test safety
cargo test --workspace --tests --no-fail-fast
cargo clippy --workspace --all-targets -- -D warnings -A clippy::type_complexity
```

To reproduce the baseline failures, use a fresh isolated worktree. Copy only the two baseline-compatible test files; the transaction suite depends on new recovery APIs.

```sh
git worktree add --detach worktrees/safety-original b0c3b474ac4d2b0bdbfdaa9085c435dcc8f6e485b220e479949797d286c0b99a
cp crates/retype-cli/tests/data_safety_audit.rs worktrees/safety-original/crates/retype-cli/tests/
cp crates/retype-cli/tests/data_safety_audit_additional.rs worktrees/safety-original/crates/retype-cli/tests/
cargo test --manifest-path worktrees/safety-original/Cargo.toml -p retype-cli \
	--test data_safety_audit --test data_safety_audit_additional --no-fail-fast -- --test-threads=1
```

The baseline command is expected to exit 101 because assertions fail, not because compilation fails. All synthetic files belong to isolated test fixtures. The focused adversarial suites require Unix; platform-neutral ownership checks also run in the core suite.

## Remaining release validation

This work does not establish safety against power loss, arbitrary storage-device faults, privileged writers, remote filesystems or cloud-provider behavior. Those require separate fault-injection and environment testing.

Windows and macOS have not been built or run in this environment. Windows containment APIs were inspected against the local bindings, but runtime containment and recovery behavior still require platform testing. Destructive Windows cleanup remains disabled.

Real encoder/codec coverage is incomplete. External originals remain preserved until backend-specific validation establishes the requested source-to-output properties. These limits prevent a blanket cross-platform data-safety sign-off.
