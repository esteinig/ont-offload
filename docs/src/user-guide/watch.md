# Running the daemon

`ont-offload watch` discovers runs under `source.root` and copies them while MinKNOW is still writing, so a run is on the share minutes after it ends. It is meant to run as a systemd service (see [Installation](installation.md#as-a-service)).

```sh
ont-offload watch                 # until stopped
ont-offload watch --until-idle    # exit once every detected run is complete or failed
```

`--until-idle` exits with 0 when every run it found is complete, and with 3 when files or runs failed. It exits at once when there is no run, so start it after the run folder exists.

## What happens to a run

| Phase | Meaning |
| --- | --- |
| claiming | Before anything else, `.ont-offload-claim.json` is written into the destination run folder on the share. A folder already claimed by a different source run (another sequencer, another configuration) refuses the run: it is reported as failed and nothing of it is written. |
| live | The run has no complete marker yet. Each file is copied once its size and modification time have not changed for `transfer.settle` (default 30 s). Files matching `selection.defer_until_complete` wait for the complete marker. |
| settling | The complete marker (`final_summary_*.txt`) is there; waiting until nothing has changed for `transfer.settle_after_complete` (default 5 min), so late files are not missed. |
| reconciling | Settled; the last files are being committed. A file that changed after it was copied is copied again. |
| finalising | Every selected file is committed as it is now. The run folder is listed in full once more, then: size check, `manifest.sha256`, `run.json` and `COMPLETE`. |
| complete | Done. The run stays under watch for `transfer.watch_completed_for` (default 24 h), then is forgotten. |
| withdrawing | A completed run changed at the source. `COMPLETE` and then `VERIFIED.json` are removed from the share before anything is copied; then the run goes back to live and gets a new manifest. |

A run that stops without a complete marker (aborted, crashed sequencer) stays live and is never marked complete. Its files are on the share, but there is no `COMPLETE`. Finish it with `ont-offload run <path> --completed` once you know it is over.

Each file goes through the same copy pipeline as `run` (see [Offloading a run](offload.md)), and the journal makes every step resumable.

### Completed runs stay under watch

Re-basecalling output or a regenerated report can be written into a run after it finished. For `watch_completed_for` after completion, `watch` compares the run's folder modification times on every scan (a few status calls per run) and the size and time of every file every `source.full_rescan_every` (default 10 min). When something was added or changed:

1. `COMPLETE`, then `VERIFIED.json`, are removed from the destination, so nothing downstream treats the old copy as final;
2. a `run_reopened` event and alert are raised (and `notify_command` runs);
3. the run is offloaded again: unchanged files are not copied again, the new and changed ones are, and a new manifest and `COMPLETE` are written.

Analysis that already started on the old copy is not stopped; that is what the alert is for. After the window the run is forgotten; a change after that is found at the next start of the service.

### Memory and scan cost

A completed run keeps only a fingerprint in memory; its file list and journal are dropped and its journal file is compacted to one line per file. Rescans list only folders whose modification time changed and check only files that are not already copied as they are, so scan work follows what is happening, not how big the runs are. Every `source.full_rescan_every`, and always before a manifest is written, the whole run is listed again.

## Workers and bandwidth

`bandwidth.workers` files are copied at the same time. `bandwidth.rate_limit` is the total for all workers together, not per worker: one shared token bucket releases at most `rate_limit` per second, plus at most one chunk of burst.

When several runs have files ready, the workers take them from the runs in turn, oldest waiting file first within each run, so a large run cannot hold every worker while another waits. `ont-offload status` shows each run's backlog and estimated time left.

To change `rate_limit` without a restart, edit the file and reload: `sudo systemctl reload ont-offload` (or send SIGHUP). The new limit applies at once to all workers. Other changed settings are logged as waiting for a restart, and a file that does not validate changes nothing.

To choose `workers`, `rate_limit` and `chunk_size` for a site, run `ont-offload bench` once on the sequencer (see [Tuning with bench](#tuning-with-bench)).

## When the share goes away

Any sign that the share is unavailable stops all writes within one chunk. That includes a missing sentinel, an I/O error such as EIO, ESTALE, ENOTCONN or a timeout, a full share (ENOSPC), less free space than `transfer.min_free`, or `dest_root` resolving elsewhere. While the share is available it is checked every minute, so the state shows its free space and a lost share is noticed between copies. Then:

- Runs keep being tracked on the local disk; nothing is lost and nothing is retried in a loop.
- The share is re-checked after 1 s, 2 s, 4 s and so on, at most every `outage.check_every_max` (default 5 min). Each check runs on its own thread with a 30 s timeout, so a hung NFS mount cannot freeze the daemon.
- Writing resumes once checks have passed for `outage.stable_before_resume` (default 60 s), so a flapping link does not cause a storm of half-written temp files.
- The log and the event stream get one line when the outage starts, a summary every `outage.log_every` (15 min), a warning after `outage.warn_after` (10 min), an alert after `outage.alert_after` (1 h) repeated every `outage.remind_every`, and one line when it ends, with its duration and backlog. Never one line per failed check. The warning and the alert also appear in `state.json` and run `notify_command`.

Outages of any length are fine: a share that is away over a weekend is caught up when it returns.

If `watch` starts while the share is not mounted, it starts in the outage state and waits. If the share is mounted but its sentinel names a different `target_id`, it refuses to start (exit 2): that is the wrong share.

## Errors by class

| Class | Examples | Action |
| --- | --- | --- |
| Transient | EIO, ESTALE, ENOTCONN, ETIMEDOUT, guard failure | Pause everything; outage handling above; not counted as an attempt |
| Capacity | ENOSPC, EDQUOT, free space below `min_free` | Pause and alert (`capacity_low`) like an outage; resume when space is back |
| Integrity | Source changed during the copy, size or read-back mismatch | Retry the file, up to `transfer.max_attempts`; a mismatch is reported as `integrity_mismatch` |
| Permanent | Permission denied, anything else | Retry up to `transfer.max_attempts`, then the file fails and its run is not completed |
| Conflict | A file on the share that ont-offload did not write | The file fails at once (or is adopted with `on_existing = "hash"`) |

Failed files are tried again after a restart.

## Stalls

A worker with no progress for `transfer.stall_timeout` (default 120 s) is reported once, with the file and run, as a `worker_stalled` event and alert. When every worker is stalled (typically writes blocked on a hard-mounted share), `watch` exits with 75: committed files are kept and systemd restarts it (`RestartForceExitStatus=75`).

## systemd

The unit uses `Type=notify`: `watch` reports ready after its first destination check and feeds the systemd watchdog from the controller loop with a status line (`systemctl status ont-offload` shows runs, backlog and any outage). If the controller itself hangs, systemd restarts the service after `WatchdogSec`.

Stopping the service (SIGTERM) or killing it at any moment is safe: temp files are never visible under their final names, and the next start resumes from the journal. `systemctl reload` applies a changed `rate_limit`; other configuration changes need a restart.

At start-up the log records the version, the effective configuration, and with the first destination check the mount (file system, server and options) and the guard result.

Only one ont-offload process may use a `state_dir`; a second `watch` or `run` on the same state folder exits with 2.

## Live state and status

`watch` keeps `state_dir/state.json` up to date, replacing it atomically every `state.interval` (default 2 s), so a reader never sees half a file. It is meant for dashboards and for `ont-offload status`:

```text
$ ont-offload status
ont-offload 0.4.0 on gridion-01 (pid 2213), started 2026-11-12T08:00:03Z
updated 1s ago
status: running; destination /mnt/nas/ont ok (nfs4), 18.21 TB free
source: 2.10 TB free
speed: 97.40 MB/s (limit 100.00 MB/s); workers 4 busy of 4
  worker 0: exp42/sampleA/20261112_0812_X1_FAX12345_abcd1234 pod5/FAX12345_0042.pod5 (524.29 MB of 1.07 GB)
  ...
since start: 812 file(s), 790.00 GB committed; 3 run(s) complete, 0 reopened, 0 failed; 0 failed file(s); 1 outage(s), 0 stall(s)
runs: 2
  exp42/sampleA/20261112_0812_X1_FAX12345_abcd1234  live  790/812 file(s), 812.00 GB; 22 waiting (22.00 GB), eta 4m
  exp41/sampleB/20261111_1530_X2_FAX12346_ef567890  complete  1204/1204 file(s), 1.20 TB
alerts: none
```

`status --json` prints the file as it is. Its format is described by [`schema/state-v1.json`](https://github.com/esteinig/ont-offload/blob/dev/schema/state-v1.json): overall status, destination (guard result, file system, free space and any outage with its duration, level, checks, backlog and stuck workers), source free space, busy workers with their file and progress, rate limit and current speed, totals, every run with its phase, backlog, failed files, integrity mismatches, skipped symlinks and estimated time left, and the open alerts. A snapshot older than three intervals (at least 30 s) is shown as STALE: the daemon is not running or is stuck.

## Events and notifications

Every state change is also appended to `state_dir/events.jsonl`, one JSON object per line ([`schema/event-v1.json`](https://github.com/esteinig/ont-offload/blob/dev/schema/event-v1.json)) with a sequence number, time, kind, level, run, file, message and details. The file is rotated to `events.jsonl.1` when it reaches `state.events_max_size` (64 MB).

| Kinds | When |
| --- | --- |
| `started`, `destination_ok` | `watch` started; the first destination check passed |
| `run_discovered`, `run_state_changed`, `run_complete`, `run_reopened`, `run_failed`, `run_retired` | A run's life, from detection to the end of its watch window |
| `file_committed`, `file_failed`, `integrity_mismatch` | One file |
| `paused`, `resumed`, `guard_failed`, `capacity_low`, `capacity_ok` | Writes stopped or started again, and why |
| `outage_started`, `outage_summary`, `outage_warning`, `outage_alert`, `outage_reminder`, `outage_recovering`, `outage_ended` | A share outage |
| `worker_stalled`, `source_space_low`, `source_space_ok` | A stuck worker; the sequencer's own disk below `outage.source_free_alert` |
| `run_deleted`, `deletion_dry_run`, `deletion_refused`, `deletion_stopped` | Source cleanup: a run deleted; what a dry-run pass would delete; a run kept because a gate failed (alert, once); a pass stopped by an error (alert). See [Deleting offloaded runs](cleanup.md) |

`outage.notify_command` runs for every event that needs a person: outage start, warning, alert, reminder and end, stalls, failed files and runs, reopened runs, integrity mismatches, guard failures and low space. It is a program and its arguments, run without a shell, with the event as one JSON line on stdin and in `ONT_OFFLOAD_EVENT_JSON`, and the kind, level and message in `ONT_OFFLOAD_EVENT`, `ONT_OFFLOAD_LEVEL` and `ONT_OFFLOAD_MESSAGE`. It is not waited for, and is killed after 60 s. For example, a script that sends a Teams message:

```sh
#!/bin/sh
# /usr/local/bin/ont-offload-notify
curl -fsS -H 'Content-Type: application/json' \
  -d "{\"text\": \"$(hostname): $ONT_OFFLOAD_MESSAGE\"}" "$TEAMS_WEBHOOK_URL"
```

## Tuning with bench

`ont-offload bench` measures the share from the sequencer and suggests settings. It checks the guard first, writes test files to a hidden folder below `dest_root` with 1, 2, 4, 8 and 16 parallel streams, then three chunk sizes, and removes them afterwards:

```text
$ ont-offload bench
bench: 2.00 GB per step to /mnt/nas/ont
   1 stream(s): 61.20 MB/s
   2 stream(s): 98.70 MB/s
   4 stream(s): 112.40 MB/s
   8 stream(s): 113.10 MB/s
  16 stream(s): 108.90 MB/s
  chunk  1MiB x 4: 104.20 MB/s
  chunk  4MiB x 4: 112.80 MB/s
  chunk 16MiB x 4: 113.00 MB/s
suggested settings (recorded in /var/lib/ont-offload/bench.json):
  [bandwidth]
  workers = 4
  rate_limit = "90MB/s"   # 80% of the best measured
  [transfer]
  chunk_size = "4MiB"
```

`workers` is the fewest streams within 90% of the best, `rate_limit` 80% of the best (leaving the rest of the link to others), and `chunk_size` the smallest within 5% of the best. Options: `--size` (data per step, default 2 GB), `--max-streams` (default 16), `--no-chunks`. Run it when the network is as busy as usual, and keep `bench.json` with the site's records: it is the evidence for the settings (see [Testing](../developer-guide/testing.md#site-acceptance)).

## Limits

The open limitations and the rules that come with controlled ones are listed, with their hazards and controls, in [Known limitations](limitations.md).
