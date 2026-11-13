# Architecture and plan

One controller thread owns all decisions and worker threads do all destination writes. Files move from the source scan, through a queue of stable files, to workers that stream, hash, write a temp file, `fdatasync`, check size and rename, only while the destination guard passes. A shared pause flag stops all workers when the share is unavailable; the controller re-checks with backoff and resumes automatically.

| Module | Software item | Responsibility |
| --- | --- | --- |
| `cli`, `main`, `error`, `lib` | SWI-CLI | clap CLI, dispatch, exit codes |
| `logging` | SWI-LOG | fern console and file logging |
| `config` | SWI-CONFIG | TOML loading, strict validation, path checks |
| `guard` | SWI-GUARD | sentinel, fstype, device id, re-resolution, free space |
| `fsio` | SWI-FSIO | `Fs` trait, `RealFs`, `FaultyFs` |
| `run` | SWI-RUN | run detection, selection, path mapping, lifecycle |
| `copy` | SWI-COPY | in-flight SHA-256, size check, optional read-back, atomic commit |
| `sched` | SWI-SCHED | `watch` controller, clock, workers, rate limiter, pause and retry, outages, stall detection, state-folder lock, systemd notification |
| `journal` | SWI-JOURNAL | journal, manifest, `run.json`, `COMPLETE`, `VERIFIED.json` |
| `state` | SWI-STATE | `state.json`, `events.jsonl` |
| `cleanup` | SWI-CLEANUP | gated source deletion |

`run` copies one file at a time in the foreground. `watch` (`sched::watch`) is one controller thread on a `Clock`: each step collects worker outcomes, runs or collects an off-thread guard probe, rescans the source, updates each run's `Lifecycle` (live, settling, reconciling, finalising) and hands stable files and finished runs to the `Pool`. The controller owns the journals and never touches the share itself; workers copy files and write the end-of-run outputs, sharing one `RateLimiter` and one pause flag, and publish progress for stall detection. The outage `Tracker` is a pure state machine (backoff, stability window, log events), so the whole controller runs on a fake clock in tests. The journal (`state_dir/runs/<run id>/journal.jsonl`) records the claim, every committed file and completion, and is what a restart resumes from.

The full design (fault analysis of the legacy scripts, per-file pipeline, outage handling, paths and symlinks, live state schema, simulator) is in the project's design plan.

## Staged implementation plan

| Version | Delivers | Done when |
| --- | --- | --- |
| v0.1 Setup and simulator | Scaffold, CI, register; `ont-runsim` (fake content, profiles, hours, speed, presets, check, clean); `Fs`/`FaultyFs`; config and path validation | CI green incl. trace; 72 h fake run matches its manifest; simulator run by hand on a sequencer |
| v0.2 Completed-run offload | Guard, copy pipeline, journal and resume, manifest, `run.json`, `COMPLETE`, run detection, `dest_starts_at` mapping with destination claims, `run --completed`, `verify` + `VERIFIED.json`, `check-config` listing runs and destinations; built-in traceability check | Simulated runs pass `sha256sum -c` on a test share; kill-and-restart resumes; missing share writes nothing |
| v0.3 Live runs, throughput, outages | `watch`, lifecycle, workers, `rate_limit`, pause/retry, long outages, stall detection, systemd | 12 h simulated live runs with faults offload exactly; nightly NFS/Samba green |
| v0.4 Observability and tuning | `state.json`, `events.jsonl` (JSON Schemas in `schema/`), `status`, JSON logs, `notify_command`, `bench` (workers, `rate_limit`, `chunk_size`), `rate_limit` reload on SIGHUP (SRS-RATE-003), free-space floor (SRS-RES-010), `readback`, simulator replay; controls for the v0.3 limitations: claims on the share (SRS-SEL-008), fair scheduling (SRS-PAR-003), completed runs kept under watch (SRS-LIVE-004), bounded memory and journals (SRS-RES-008), incremental rescan (SRS-RES-009) | Schema checked in CI (unit tests validate live snapshots and events); 24 h replay soak on the fake clock in every CI run; on site: `bench` recorded and the 24 h replay soak (`scripts/replay-soak.sh`) clean on the staging share |
| v0.5 Gated deletion and qualification | `[cleanup]`, gates, dry-run, `cleanup`; 72 h soaks; parallel run with the legacy service | Release gate passes with approved items; two weeks of dry-run deletion reports reviewed; v1.0 sign-off |
| v0.6 Monitoring dashboard (planned) | Read-only Ratatui `tui` (feature `tui`): live state, runs, workers, throughput, pinned alerts with banner and bell, event and log tail, cleanup panel, STALE and NO DATA states; themes `matrix`, `phosphor`, `synthwave`, `plain` | Golden renders per theme in CI; contrast check; no writes with writes denied; every injected fault shown within two refreshes in a 24 h replay soak; HAZ-014 to 017 controls approved |
