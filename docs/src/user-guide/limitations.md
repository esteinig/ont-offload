# Known limitations

Every known limitation of the current release is listed here with the hazard it relates to, what controls it today, and the control planned for a later release. Limitations that are already controlled are listed too, because the control is often a rule for the operator or leaves a remainder worth knowing. Hazards and requirements are in [`compliance/`](https://github.com/esteinig/ont-offload/tree/dev/compliance); the planned versions follow the [staged plan](../developer-guide/architecture.md#staged-implementation-plan).

Status as of v0.4.

## Open: controls planned

| Limitation | Hazard | Controlled today by | Planned control |
| --- | --- | --- | --- |
| **Memory and scan-time bounds are not yet shown on real data.** Completed runs are dropped from memory, journals are compacted and rescans list only changed folders (see below), but this is verified on the fake clock with a few runs, not over days with hundreds of runs. | HAZ-013 (minor) | SRS-RES-008, SRS-RES-009 (v0.4); SRS-RES-003: the watchdog and the stall exit (75) restart a daemon that hangs; SRS-RES-004: a restart resumes without copying again. | 72 h soak with memory and scan-time bounds over 200 runs (v0.5). |
| **A change to a run after `watch_completed_for` is noticed only at the next start.** Completed runs are watched for 24 h by default, then forgotten. | HAZ-012 (serious) | SRS-RES-004: at every start, a completed run whose files changed is withdrawn and offloaded again. **Operator rule:** raise `watch_completed_for` if runs are re-processed later than a day after they finish, or restart the service after re-processing an old run. | None planned; the window is a setting. |
| **The nightly NFS and Samba test has not run on GitHub yet.** | HAZ-007 | SRS-RES-005 is verified on a fake clock and by a local tmpfs rehearsal. | The first nightly run closes the gap. |

## Controlled in v0.4: limitations of v0.3 and how they are handled now

These four limitations were open in v0.3. Each now has a control in the software, verified by tests; the hazards keep their records and their residual risk is set when the controls are reviewed.

| Limitation in v0.3 | Hazard | Control in v0.4 | What remains |
| --- | --- | --- | --- |
| **Two instances on one share** could mix two runs in one destination folder, because claims lived only in each `state_dir`. | HAZ-010 (critical) | SRS-SEL-008: before the first file, each run writes `.ont-offload-claim.json` into its destination folder on the share (create-new). A second instance, on any host, that maps a different source run to the same folder is refused: the run is reported as failed, with an alert, and nothing of it is written. `run` refuses the same way. SRS-CPY-002 still refuses to overwrite a file it did not write. | A claim file removed by hand removes the protection. Give each sequencer its own `dest_root` or top folder anyway; it keeps runs apart by name, not only by claim. |
| **No fairness between runs**: a large run that sorted first could hold every worker. | HAZ-011 (minor) | SRS-PAR-003: the workers take ready files from the runs in turn (oldest waiting file first within a run). SRS-STATE-001: `state.json` and `ont-offload status` show each run's backlog and estimated time left. | A run still waits for its share of the workers; more throughput needs more `workers` or bandwidth (see `bench`). |
| **Files added after `COMPLETE` were missed until a restart.** | HAZ-012 (serious) | SRS-LIVE-004: completed runs stay under watch for `transfer.watch_completed_for` (default 24 h). Folder times are compared every scan, and every file's size and time every `source.full_rescan_every` (10 min). On a change, `COMPLETE` and then `VERIFIED.json` are removed from the share before anything else, a `run_reopened` event and alert are raised, and the run is offloaded again with a new manifest. | An in-place change that keeps the folder times is found within `full_rescan_every`, not at once. Analysis that started on the old copy is not stopped; watch for `run_reopened`. |
| **Memory, journal growth and full rescans**: every journal stayed in memory, journals grew with every retry, and every scan walked every file. | HAZ-013 (minor) | SRS-RES-008: a completed run keeps only a fingerprint in memory and its journal is compacted to one record per file; it is forgotten once no longer watched. SRS-RES-009: rescans list only folders whose time changed and check only files not yet committed, with a full rescan every 10 min and always before a manifest is written. | See the open row above: bounds on real data come with the v0.5 soak. |

## Controlled: rules and behaviour to know

| Limitation | Hazard | Control |
| --- | --- | --- |
| Only one ont-offload process per `state_dir`; a second `watch` or `run` exits with 2. | HAZ-004, HAZ-006, HAZ-010 | SRS-RES-007 (v0.3). By design: one `watch` handles every run under `source.root`. |
| A run that stops without a complete marker (aborted, crashed sequencer) is never marked complete. | HAZ-003 | SRS-LIVE-003 (v0.3). Finish it by hand with `ont-offload run <path> --completed` once you know it is over. |
| A worker blocked on a hard-mounted share stays blocked until the share answers. | HAZ-007 | SRS-RES-003 (v0.3): the stall is reported, shown in `state.json` and sent to `notify_command`; when every worker is stalled, `watch` exits with 75 and systemd restarts it. Destination checks run off the controller thread with a 30 s timeout (SRS-RES-005). |
| Failed files (permission denied, conflicts, repeated mismatches) are retried only after a restart; their run is not completed. | HAZ-003, HAZ-007 | SRS-RES-001: the run is never completed with a failed file. Since v0.4 each failure is an event, an alert in `state.json` and a `notify_command` call (SRS-STATE-001, SRS-STATE-003). |
| When the share has less than `min_free` left, copying pauses until space is freed. | HAZ-007 | SRS-RES-010 (v0.4): no file fails for lack of space; a `capacity_low` alert is raised and copying resumes by itself. The default `min_free` is 500 GiB; set it to what the share's other users need. |
| `state.json` is written by `watch` only. `run` and `verify` do not update it. | none | By design: `run` prints its own result. `status` flags a snapshot older than three intervals as stale. |
| Only `rate_limit` changes on reload (SIGHUP, `systemctl reload`). Other settings need a restart. | none | SRS-RATE-003 (v0.4). The log says which changes wait for a restart; an invalid file changes nothing. |
| `notify_command` runs without a shell and is killed after 60 s. | none | By design: give a script path and arguments; the script does its own sending. |
| Stopping or killing the process discards the file being copied. | HAZ-004 | SRS-CPY-001, SRS-RES-004: temp files never appear under final names; the next start removes them and resumes. |
| `watch --until-idle` exits at once when no run is found. | none | Operational: start it after the run folder exists. |
