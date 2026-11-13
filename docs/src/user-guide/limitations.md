# Known limitations

Every known limitation of the current release is listed here with the hazard it relates to, what controls it today, and the control planned for a later release. Limitations that are already controlled are listed too, because the control is often a rule for the operator. Hazards and requirements are in [`compliance/`](https://github.com/esteinig/ont-offload/tree/dev/compliance); the planned versions follow the [staged plan](../developer-guide/architecture.md#staged-implementation-plan).

Status as of v0.3.

## Open: controls planned

| Limitation | Hazard | Controlled today by | Planned control |
| --- | --- | --- | --- |
| **Two instances on one share.** Destination claims are kept in each `state_dir`. Two ont-offload processes with separate state folders (two sequencers, or two configurations on one host) that write to the same share cannot see each other's claims. | HAZ-010 (critical) | SRS-CPY-002: a file the process did not write is never overwritten, so the clash shows up as conflicts and the run is not completed. SRS-RES-007: one process per state folder on a host. **Operator rule:** give each sequencer its own `dest_root` (or its own top folder) on the share. | SRS-SEL-008 (v0.4): a claim file in each destination run folder on the share itself, checked by every instance. |
| **No fairness between runs.** Ready files are queued in source-path order, so when the workers are busy, a large run that sorts first can hold them and delay a later run. | HAZ-011 (minor) | Nothing beyond `workers`; per-run progress is visible only in the log. | SRS-PAR-003 (v0.4): round-robin scheduling across runs. SRS-STATE-001 (v0.4): per-run backlog in the state snapshot. |
| **Files added after `COMPLETE` are missed until restart.** A completed run is not rescanned. A file written into it after `settle_after_complete` (re-basecalling output, a regenerated report) is not copied, and the copy stays marked complete without it. | HAZ-012 (serious) | SRS-LIVE-003: files arriving within `settle_after_complete` (default 5 min) are included. SRS-RES-004: at the next start, a completed run whose files changed is offloaded again with a new manifest. **Operator rule:** raise `settle_after_complete` if anything writes into runs later, and restart after re-processing a run in place. | SRS-LIVE-004 (v0.4): completed runs stay under watch for `watch_completed_for` (24 h); on a change, `COMPLETE` and `VERIFIED.json` are withdrawn first, then the run is offloaded again. |
| **Memory, journal growth and full rescans.** The journals of all runs seen are kept in memory for the life of the process, journals are never compacted, and every scan (up to every 10 s) walks the whole source tree and every file of every unfinished run. | HAZ-013 (minor) | SRS-RES-003: the systemd watchdog and the stall exit (75) restart a daemon that hangs. SRS-RES-004: a restart resumes without copying again, and loads only runs still on the source. | SRS-RES-008 (v0.4): finished runs dropped from memory, journals compacted on completion. SRS-RES-009 (v0.4): rescan only changed folders, full rescan every 10 min. 72 h soak with memory and scan-time bounds (v0.5). |

## Controlled: rules and behaviour to know

| Limitation | Hazard | Control |
| --- | --- | --- |
| Only one ont-offload process per `state_dir`; a second `watch` or `run` exits with 2. | HAZ-004, HAZ-006, HAZ-010 | SRS-RES-007 (v0.3). By design: one `watch` handles every run under `source.root`. |
| A run that stops without a complete marker (aborted, crashed sequencer) is never marked complete. | HAZ-003 | SRS-LIVE-003 (v0.3). Finish it by hand with `ont-offload run <path> --completed` once you know it is over. |
| A worker blocked on a hard-mounted share stays blocked until the share answers. | HAZ-007 | SRS-RES-003 (v0.3): the stall is reported; when every worker is stalled, `watch` exits with 75 and systemd restarts it. Destination checks run off the controller thread with a 30 s timeout (SRS-RES-005). |
| Failed files (permission denied, conflicts, repeated mismatches) are retried only after a restart; their run is not completed. | HAZ-003, HAZ-007 | SRS-RES-001 (v0.3): the run is never completed with a failed file, and the failure is logged as an error. Alerts with `notify_command` and per-run state come in v0.4 (SRS-STATE-001, SRS-STATE-003). |
| Stopping or killing the process discards the file being copied. | HAZ-004 | SRS-CPY-001, SRS-RES-004: temp files never appear under final names; the next start removes them and resumes. |
| Configuration changes need a restart; there is no reload. | none | Operational. `sudo systemctl restart ont-offload`. |
| `watch --until-idle` exits at once when no run is found. | none | Operational: start it after the run folder exists. |
| The nightly NFS and Samba test has not run on GitHub yet. | HAZ-007 | Verification gap: SRS-RES-005 is verified on a fake clock and by a local tmpfs rehearsal; the first nightly run closes it. |
