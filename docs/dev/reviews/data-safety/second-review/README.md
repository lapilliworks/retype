# Second review follow-up

A focused re-review after the [platform-test follow-up](../platform-follow-up/README.md) found five ways the safety rules could still fail. The transaction, validation and pinning structure from the [initial response](../fixes/README.md) held; the gaps were in what it checked and in the order of two state changes.

## Findings and fixes

| Problem | Fix | Regression evidence |
|---|---|---|
| A native archive was published and could authorize disposal without being reopened, so a writer that dropped, duplicated or truncated an entry still passed | Reopen the staged output and compare its inventory with the source before publication. Backslash separators, absolute and drive-qualified names and `..` are refused, entries other than regular files and directories are refused, and a PAX size override is authoritative over the raw header | `archive_safety::a_pax_size_override_is_carried_into_the_new_container`, `archive_safety::a_backslash_parent_component_is_refused`, `archive_safety::a_zip_fifo_is_refused_instead_of_becoming_a_regular_file`, `data_safety_transactions::a_pax_size_override_is_complete_before_the_original_is_disposed` |
| The inventory comparison covered names and sizes, so an entry with the same extent but different bytes matched | Hash every entry as it is read and compare the digest as part of the inventory | The archive safety suite and the native conversion path both compare content-addressed inventories |
| A failed disposal could leave the only complete copy in the backup while the recovery record was unpinned and eligible for eviction | Keep the record pinned unless the live source, or the occupied target that held it, still matches the snapshot the conversion read | `data_safety_transactions::a_changed_source_keeps_the_original_backup_pinned` |
| After a restart, a queued consuming job ran against whatever the source path held then, and could delete a replacement | Requeue only when the live path is still a regular file whose recorded format, size and mtime match; otherwise fail it with `E_JOB_INTERRUPTED` | `reconcile::a_queued_job_does_not_run_against_a_replacement_source` |
| Quota eviction removed old records before the new record was durable, so a failed save could leave neither the old backup nor the new one | Reserve every selected record, create the new record, then remove the old ones | `undo::tests::a_failed_new_record_does_not_evict_an_old_one` |

These are regression checks, not a repeated platform validation. The [test preparation](../../../testing.md) prerequisites and the validation limits recorded in the earlier reviews still apply.
