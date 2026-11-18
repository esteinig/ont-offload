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
| `sched` | SWI-SCHED | queue, workers, rate limiter, pause and retry, outages, stall detection |
| `journal` | SWI-JOURNAL | journal, manifest, `run.json`, `COMPLETE`, `VERIFIED.json` |
| `state` | SWI-STATE | `state.json`, `events.jsonl` |
| `cleanup` | SWI-CLEANUP | gated source deletion |

In v0.2 the controller copies one file at a time in the foreground (`run`); the worker pool, rate limiter and pause flag arrive with `watch` in v0.3. The journal (`state_dir/runs/<run id>/journal.jsonl`) records the claim, every committed file and completion, and is what a restart resumes from.

The full design (fault analysis of the legacy scripts, per-file pipeline, outage handling, paths and symlinks, live state schema, simulator) is in the project's design plan.

## Staged implementation plan

| Version | Delivers | Done when |
| --- | --- | --- |
| v0.1 Setup and simulator | Scaffold, CI, register; `ont-runsim` (fake content, profiles, hours, speed, presets, check, clean); `Fs`/`FaultyFs`; config and path validation | CI green incl. trace; 72 h fake run matches its manifest; simulator run by hand on a sequencer |
| v0.2 Completed-run offload | Guard, copy pipeline, journal and resume, manifest, `run.json`, `COMPLETE`, run detection, `dest_starts_at` mapping with destination claims, `run --completed`, `verify` + `VERIFIED.json`, `check-config` listing runs and destinations; built-in traceability check | Simulated runs pass `sha256sum -c` on a test share; kill-and-restart resumes; missing share writes nothing |
| v0.3 Live runs, throughput, outages | `watch`, lifecycle, workers, `rate_limit`, pause/retry, long outages, stall detection, systemd | 12 h simulated live runs with faults offload exactly; nightly NFS/Samba green |
| v0.4 Observability and tuning | `state.json`, `events.jsonl`, schema, `status`, JSON logs, `notify_command`, `bench`, `readback`, simulator replay | Schema in CI; bench recorded; 24 h replay soak clean |
| v0.5 Gated deletion and qualification | `[cleanup]`, gates, dry-run, `cleanup`; 72 h soaks; parallel run with the legacy service | Release gate passes with approved items; two weeks of dry-run deletion reports reviewed; v1.0 sign-off |
