# Testing

| Layer | What | Where |
| --- | --- | --- |
| Unit | Path validation (incl. BEL), selection, token bucket on a fake clock, backoff, errno classes, state and event schemas, bench suggestions, `notify_command` | every PR |
| Property | Path mapping never escapes `dest_root`; byte streams round-trip with equal digests | every PR |
| Fault injection | `FaultyFs`: EIO/ESTALE bursts, ENOSPC, short writes, blocking writes, bit flips, vanishing sentinel, repointed `dest_root`, source appended mid-copy, failed rename; v0.2 covers these in `src/run/tests.rs` | every PR |
| Simulator | `ont-runsim` on a fake clock: MinKNOW-shaped runs, faults on, destination compared with the simulator manifest | every PR |
| Real shares | NFS and Samba on the CI runner, server stopped mid-run (`scripts/real-share-test.sh`) | nightly |
| Laptop bench | a tmpfs folder as the share, simulated runs, outage by moving the sentinel (`scripts/local-test.sh`, [Testing on a laptop](../user-guide/local-testing.md)) | by hand |
| Soak and qualification | Fake-clock soaks in CI (24 h replay; 72 h with 200 runs, hourly storage-side verification and cleanup, memory bounds); 24 to 72 h runs on staging shares, independent `sha256sum -c`; two weeks of dry-run cleanup reports reviewed on site | before each release |

Every test carries `/// Verifies: SRS-...`; `cargo xtask trace --strict` fails when a requirement has no passing test.

## Traceability without medtrace

Traceability is also checked by plain `cargo test`: `xtask/tests/traceability.rs` runs the built-in check in `xtask/src/trace.rs` (standard library only) against the repository. It fails when

- a `#[test]` in `src/` or `tests/` has no `Verifies:` tag;
- an `Implements:` or `Verifies:` tag names a requirement that is not in `compliance/requirements/`;
- a requirement is not allocated to an existing software item, or a software item's paths are missing once its first requirement is due;
- a requirement due in or before the milestone (`MILESTONE` in `trace.rs`, now `v0.7`) has no `Implements:` tag or no verifying test;
- a hazard has no control, or names a requirement that does not exist.

It warns when a requirement is implemented outside its allocated software items, or when a hazard and a requirement do not list each other.

`cargo xtask trace --builtin --results` runs `cargo test`, adds each requirement's test outcomes, fails on any requirement due by the milestone without a passing test, and writes the matrix to `target/trace/matrix.md`. `cargo xtask trace` uses medtrace when it is installed and falls back to this check otherwise. Raise `MILESTONE` when work on the next release starts.

## The daemon on a fake clock

`src/sched/tests.rs` drives the `watch` controller and the simulator together on a fake clock, step by step, with faults from `FaultyFs`: two live runs with rewritten files and late files; an aborted run (never completed); a 12 h live run through a 3 h outage, ESTALE and ENOSPC bursts and short writes; start with the share missing; a share of another target; stalled workers; errors by class; kill and restart. Every finished run is compared with the simulator's manifest.

v0.4 adds, on the same harness: a second controller (own source and state folder) refused by the claim on the share; two runs served in turn by one worker; a completed run that gains a file and one changed in place, withdrawn before it is copied again; completed runs dropped from memory, journals compacted and runs retired after the watch window; incremental rescans that list only changed folders and miss nothing; the share below `min_free`; low source space; `rate_limit` reload; and the **24 h replay soak**: two overlapping runs replaying a seed run's bytes for 24 simulated hours through a 2 h outage and an error burst, with every snapshot and event validated against the schemas every two simulated hours and both runs compared with the simulator's manifest. The rate limiter is checked on the fake clock (four workers, the total never above `rate + burst`) and with real threads; the outage tracker over a simulated 12 h outage (backoff cap, number of log lines) and a flapping link.

## Dashboard

The dashboard's logic is tested without a terminal. `src/tui/model/tests.rs` runs the model on an injected time against schema-valid fixtures (`src/tui/fixtures.rs`: normal, outage, stalled worker, failed run, refused deletion): live until three intervals, then stale; no data and other schema versions; alerts from the snapshot and from events, clearing, acknowledgement and the bell count; and a refresh with every write operation failing in `FaultyFs`, with the state folder compared before and after. `src/tui/source/tests.rs` covers torn lines, rotation, truncation, sequence gaps and restarts. `src/tui/view/tests.rs` draws each scenario and each theme into a Ratatui `TestBackend` and compares the text with golden files in `src/tui/view/golden/` (`UPDATE_GOLDEN=1 cargo test --features tui` rewrites them; review the diff); it also checks that failures stand out in every theme and without colour, that nothing is called OK while stale, and that decoration stops for alerts. The 24 h replay soak reads the state folder with the dashboard model every 5 simulated minutes and checks that every alerting event is shown by the next refresh and that the data never reads as stale. `tests/tui.rs` runs `tui --check` with environment variables through the binary. Test runs use `--features tui` (`cargo xtask test` does).

## Deployment files

`tests/packaging.rs` checks the deployment files: the unit runs as `ont-offload` with `0750` state and log folders, umask `0027` and its restart rules; `sysusers` creates the user; logrotate renames (no `copytruncate`), creates the log `0640` and never removes or compresses an old log (`rotate -1`); the shell scripts parse. The log file's rotation, the share write probe and `verify --against` have unit tests, and `tests/offload.rs` runs `check-config` (probe), `verify --against` and `scripts/compare-with-legacy.sh` through the binary on a tmpfs share.

## State schemas

`schema/state-v1.json` and `schema/event-v1.json` are written by hand next to the types in `src/state/`. A small validator in `src/state/schema_check.rs` (test-only) checks sample and live documents against them, and a unit test checks that the event kinds in the schema and in the code are the same list, so CI fails when the two drift apart.

## Real shares (nightly)

`.github/workflows/nightly.yml` mounts a real NFS and a real Samba share on the runner and runs `scripts/real-share-test.sh`: a simulated live run (`ont-runsim --preset quick-2h`) is offloaded by `watch --until-idle` while the server is stopped for two minutes, and the copy is checked with `ont-runsim check`, `sha256sum -c` and `ont-offload verify --source`. The script runs anywhere a share is mounted; `DOWN_CMD` and `UP_CMD` say how to take it away and bring it back.

## End-to-end tests

`tests/watch.rs` runs `watch --until-idle` on a simulated run with a systemd notification socket, checks that another target's share and a second instance are refused, and that `state.json` and `events.jsonl` are written and read back by `status` and `status --json`. `tests/bench.rs` runs `bench` on tmpfs (measured, recorded in `bench.json`, test files removed) and checks that a share failing the guard gets nothing. `tests/offload.rs` runs the binary on runs made by `ont-runsim`, with `/dev/shm` (tmpfs, a different device) standing in for the share: `run` produces a copy that matches the simulator's manifest and passes `sha256sum -c`; a second `run` reports the run complete; `verify` writes `VERIFIED.json` and catches a tampered file; a missing sentinel exits 2 and leaves the share empty; `check-config` lists runs and their destinations. The tests skip where `/dev/shm` is missing or on the same device. `cargo xtask test` uses cargo-nextest profiles `unit` and `integration` and writes JUnit reports to `target/nextest/` for medtrace.

## Reports

`src/report/status/tests.rs` builds status records from hand-written event histories that cross the archive, rotation and live files (periods, gaps, outages, verdict rules, schema). `src/report/verification/tests.rs` builds verification records from hand-built MinKNOW runs: an exact copy passes; a changed, missing or extra file fails even when the manifest and receipt agree; without events or source nothing passes; records validate against `schema/verification-report-v1.json`. `src/report/render.rs` renders an empty record to PDF/A (feature `report`). `tests/report.rs` checks that reports leave the state folder unchanged and write only into `--out`; `tests/report_verify.rs` offloads simulated runs with `watch --until-idle`, passes `report verify`, gets INCONCLUSIVE from `p2i-acceptance` without a drill, and FAIL after a copy is changed. `cargo xtask verify-report` runs the same end to end with the binaries and keeps the PDF (see [Reports](../user-guide/reports.md#release-evidence-cargo-xtask-verify-report)).

## Site acceptance

Two v0.4 acceptance steps need the site's own share and are run by hand, with the evidence kept with the release records:

1. **Bench recorded.** On each sequencer, `ont-offload bench` with the network as busy as usual; copy the suggested settings into the configuration and keep `state_dir/bench.json`.
2. **24 h replay soak on the staging share.** `scripts/replay-soak.sh <dest_root> <target_id> <real run folder> <work folder>` replays the bytes of a real run as a new live run over 24 h (`HOURS`, `SPEED`, `SIZE_SCALE` to shorten a rehearsal), offloads it with `watch`, checks `status` every 10 minutes for a stale state, and at the end compares the copy with `ont-runsim check`, `sha256sum -c` and `ont-offload verify --source`. It fails on any difference or on any `file_failed` or `run_failed` event, and collects `state.json`, `events.jsonl`, `bench.json`, the JSON log and the check outputs in `<work>/evidence`. A one-hour rehearsal on tmpfs: `HOURS=1 SPEED=120 SIZE_SCALE=0.01 KEEP_FREE=1GB ALLOWED='["tmpfs"]' EXTRA_TOML=$'[transfer]\nmin_free = "100MB"' scripts/replay-soak.sh ...`.
