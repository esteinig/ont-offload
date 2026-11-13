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
| live | The run has no complete marker yet. Each file is copied once its size and modification time have not changed for `transfer.settle` (default 30 s). Files matching `selection.defer_until_complete` wait for the complete marker. |
| settling | The complete marker (`final_summary_*.txt`) is there; waiting until nothing has changed for `transfer.settle_after_complete` (default 5 min), so late files are not missed. |
| reconciling | Settled; the last files are being committed. A file that changed after it was copied is copied again. |
| finalising | Every selected file is committed as it is now: size check, then `manifest.sha256`, `run.json` and `COMPLETE`. |

A run that stops without a complete marker (aborted, crashed sequencer) stays live and is never marked complete. Its files are on the share, but there is no `COMPLETE`. Finish it with `ont-offload run <path> --completed` once you know it is over.

Each file goes through the same copy pipeline as `run` (see [Offloading a run](offload.md)), and the journal makes every step resumable.

## Workers and bandwidth

`bandwidth.workers` files are copied at the same time. `bandwidth.rate_limit` is the total for all workers together, not per worker: one shared token bucket releases at most `rate_limit` per second, plus at most one chunk of burst.

## When the share goes away

Any sign that the share is unavailable stops all writes within one chunk. That includes a missing sentinel, an I/O error such as EIO, ESTALE, ENOTCONN or a timeout, a full share (ENOSPC), or `dest_root` resolving elsewhere. Then:

- Runs keep being tracked on the local disk; nothing is lost and nothing is retried in a loop.
- The share is re-checked after 1 s, 2 s, 4 s and so on, at most every `outage.check_every_max` (default 5 min). Each check runs on its own thread with a 30 s timeout, so a hung NFS mount cannot freeze the daemon.
- Writing resumes once checks have passed for `outage.stable_before_resume` (default 60 s), so a flapping link does not cause a storm of half-written temp files.
- The log gets one line when the outage starts, a summary every `outage.log_every` (15 min), a warning after `outage.warn_after` (10 min), an alert after `outage.alert_after` (1 h) repeated every `outage.remind_every`, and one line when it ends, with its duration and backlog. It never gets one line per failed check.

Outages of any length are fine: a share that is away over a weekend is caught up when it returns.

If `watch` starts while the share is not mounted, it starts in the outage state and waits. If the share is mounted but its sentinel names a different `target_id`, it refuses to start (exit 2): that is the wrong share.

## Errors by class

| Class | Examples | Action |
| --- | --- | --- |
| Transient | EIO, ESTALE, ENOTCONN, ETIMEDOUT, guard failure | Pause everything; outage handling above; not counted as an attempt |
| Capacity | ENOSPC, EDQUOT | Pause and alert like an outage |
| Integrity | Source changed during the copy, size or read-back mismatch | Retry the file, up to `transfer.max_attempts` |
| Permanent | Permission denied, anything else | Retry up to `transfer.max_attempts`, then the file fails and its run is not completed |
| Conflict | A file on the share that ont-offload did not write | The file fails at once (or is adopted with `on_existing = "hash"`) |

Failed files are tried again after a restart.

## Stalls

A worker with no progress for `transfer.stall_timeout` (default 120 s) is reported once, with the file and run. When every worker is stalled (typically writes blocked on a hard-mounted share), `watch` exits with 75: committed files are kept and systemd restarts it (`RestartForceExitStatus=75`).

## systemd

The unit uses `Type=notify`: `watch` reports ready after its first destination check and feeds the systemd watchdog from the controller loop with a status line (`systemctl status ont-offload` shows runs, backlog and any outage). If the controller itself hangs, systemd restarts the service after `WatchdogSec`.

Stopping the service (SIGTERM) or killing it at any moment is safe: temp files are never visible under their final names, and the next start resumes from the journal. Configuration changes need a restart.

Only one ont-offload process may use a `state_dir`; a second `watch` or `run` on the same state folder exits with 2.

## Limits in v0.3

- A file that appears in a run after the run was completed (later than `settle_after_complete`) is picked up at the next start, which re-offloads the changed run.
- `status`, `state.json`, `events.jsonl` and `notify_command` alerts come in v0.4; until then, follow the log or `systemctl status`.
