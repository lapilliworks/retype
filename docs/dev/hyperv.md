# Hyper-V information collection

Run `tests/hyperv-info.ps1` on the Windows host that owns the VM, in an elevated Windows PowerShell 5.1 or newer session. PowerShell Direct requires a local running Windows 10 / Windows Server 2016 or newer guest and valid guest credentials. The Hyper-V PowerShell module and its VHD queries must be available on the host.

```powershell
.\tests\hyperv-info.ps1 -VMName 'Retype disposable VM'
```

The script prompts locally for guest credentials and the absolute Retype checkout path inside the guest. It does not discover the checkout by scanning guest disks. Do not send credentials in chat or put passwords on the command line. To specify paths explicitly:

```powershell
.\tests\hyperv-info.ps1 -VMName 'Retype disposable VM' `
	-GuestRepository 'C:\Works\retype' -ReportDirectory 'C:\RetypeReports'
```

The collector uses the selected VM's GUID for guest queries. It saves reports in a uniquely created directory on the host. Each host or guest query has a 60-second deadline; `-TimeoutSeconds` accepts 1 through 300. Interactive credential and path prompts are outside those deadlines. The PowerShell Direct session normally uses the guest's default Windows PowerShell endpoint; `guest.json` also records whether `pwsh` is available on its PATH, not the version of that separate executable.

## Reports

- `request.json`: Requested VM name, query deadline, collector SHA-256 and evidence scope
- `host.json`: Host OS and PowerShell, VM identity and configuration, attached VHD/VHDX metadata and host volumes
- `guest.json`: Guest OS, architecture, PowerShell, `pwsh` paths and volumes
- `repository.json`: Guest checkout path, Git revision, clean status and changed/untracked paths
- `summary.json`: Completion or failing stage, error and timestamp

A failed query fails the script; reports already collected remain on the host. An unsuccessful or empty query is not reported as a success. Pass-through disks are refused rather than presented as VHD-backed storage. A dirty checkout is reported with a warning: the report does not capture its changed bytes or ignored files. Use a clean checkout for fault tests, or preserve and review its diffs separately.

The collector does not start Retype, run conversions, change VM power state, create/apply/delete checkpoints, modify Git trust configuration or save credentials. Git status runs with optional index writes and filesystem-monitor hooks disabled. PowerShell Direct executes under the supplied guest account, so its observations are not automatically non-administrator guest evidence. Reports contain local paths and computer/VM names; inspect them before sharing.

## Evidence boundary

This is read-only preflight, not the checkpoint-driven crash harness. An abrupt `Stop-VM -TurnOff` tests interruption of the guest; it does not remove host power or establish physical device flush compliance. Preserve the same virtual disks without checkpoint reversion between a crash and inspection. Capture pre-recovery state before starting Retype. Windows destructive cleanup remains disabled.

## Synthetic checks

```powershell
.\tests\hyperv-check.ps1
```

These checks exercise job completion, failure, empty/multiple results, deadlines, cleanup, report serialization and clean/dirty/invalid repositories without contacting a VM. They do not execute Hyper-V APIs or prove native PowerShell Direct behavior.
