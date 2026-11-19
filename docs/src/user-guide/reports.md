# Reports

`ont-offload report` writes two kinds of clinical operations report, each as a JSON record and a PDF made from it:

- **Status report** (`report status`): what ont-offload did over a period, such as a month, taken from its own records. It is for the periodic operations review and for looking back at what happened to a run.
- **Verification report** (`report verify`): whether one execution offloaded every run completely and correctly, checked against a reference that ont-offload did not produce. It is for site acceptance, for revalidation after a change, and as release evidence.

Reports only read. They never change the state folder, the log or the share, and they write only into `--out`. They can run while the daemon is running, on the sequencer or on any host that can read the records.

## Outputs

| File | What it is |
| --- | --- |
| `<id>.json` | The record: every number, verdict and check result. Schemas: [`status-report-v1.json`](https://github.com/esteinig/ont-offload/blob/dev/schema/status-report-v1.json), [`verification-report-v1.json`](https://github.com/esteinig/ont-offload/blob/dev/schema/verification-report-v1.json) |
| `<id>.pdf` | A4, PDF/A-2b, rendered from the record by the embedded typst template. The footer of every page names the SHA-256 of the JSON record |
| `<id>.typst/` | With `--format typ`: the templates and the record, to compile elsewhere with `typst compile` |
| `<id>.sha256` | `sha256sum -c` file of every output |

`--format` takes `pdf`, `json` and `typ`, comma-separated; the default is `pdf,json`. A PDF is always written with its JSON record. Every decision (counts, verdict, check results) is made in Rust and tested; the template only formats the record, so the PDF and the JSON never disagree. The same record and time give the same PDF on every host.

File names: `OOF-SR-<host>-<first day>-<last day>` for a status report and `OOF-VR-<host>-<day>-<hash>` for a verification report. `OOF` is `[report] prefix`.

PDF rendering needs the build feature `report`, which the release binaries include. It compiles typst 0.13 and the fonts IBM Plex Sans and IBM Plex Mono (SIL Open Font License 1.1, in `src/report/fonts/`) into the binary, so nothing has to be installed on the sequencer. A binary built without it writes the JSON record only (`--format json`); the record can be rendered later with a binary that has the feature or with `--format typ` and typst.

### Site name and logo

```toml
[report]
site = "Clinical Genomics, Example Hospital"   # printed in every title block
prefix = "OOF"                                  # report ids and file names (letters, digits, '-')
logo = "/etc/ont-offload/logo.png"              # PNG, JPEG or SVG, top right of the title block
```

All three are optional. `report status` works without a configuration when `--state-dir` (and `--log`) are given.

## Status report

```console
$ ont-offload report status --month 2026-10 --out /srv/reports
status report OOF-SR-20261031-e10172: ACTION REQUIRED (2026-10-01 to 2026-10-31, UTC): 12 runs, 1.31 TB committed; ...
```

The period is one of `--month 2026-10`, `--last 7d` (up to now) or `--from` and `--to` (a date or an RFC 3339 time; a date in `--to` includes that day). Without any of them it is the last 30 days. Periods are in UTC. `--check-destination` also reads each completed run's `VERIFIED.json` on the share and counts the runs without a matching receipt. `--appendix` adds every event of the period (except per-file commits) and every warning and error line of the log.

The report reads every event segment (`state_dir/events/`, `events.jsonl.1`, `events.jsonl`) and the log with all its rotations, plain or gzip. Since v0.9 none of these are ever deleted (see [Live state and status](watch.md#live-state-and-status)), so a report can be made for any period since installation.

**What it shows**

1. A title block with the period, host, target id, sources, destination, ont-offload versions seen and the site.
2. The verdict, with every reason:
   - **Action required** if a run or file failed and was not resolved, a condition is still open at the end of the period (outage, low space on the share, the source or the records), or a record has a gap (missing event numbers, unreadable lines, no events at all).
   - **Attention** for points to review that resolved themselves: outages, integrity mismatches that were retried, reopened runs, worker stalls, guard refusals, refused deletions, records that start after the period starts, completed runs without a receipt.
   - **OK** otherwise.
3. Key figures: runs, files and bytes committed, failures, outages and their total time, availability, stalls, cleanup.
4. Data committed per day (per week for periods over two months), with outage days marked.
5. Every run with activity in the period: flow cell, position, discovered, completed, duration, files and bytes, manifest digest, destination and status.
6. Incidents, with start, end, duration and outcome.
7. Service sessions: every start, its version, its last record and the time to the next start.
8. Source cleanup: every deletion, refusal and stop.
9. Record completeness: each file read, its time span and its gaps.
10. A review block with sign-off lines.

Make it monthly with a systemd timer or cron, for example `ont-offload report status --month "$(date -u -d 'last month' +%Y-%m)" --out /srv/reports` on the 1st, and file the PDF with the site's quality records.

## Verification report

```console
$ ont-offload report verify --sim /data/ont-runsim --operator "J. Smith" --out /srv/reports
verification report OOF-VR-20261118-95125b: PASS (10 of 10 required checks passed); ...
```

The report compares what ont-offload put on the share, and what it recorded, with an independent reference:

| Mode | Reference | Use |
| --- | --- | --- |
| `--sim <out>` | The manifests [`ont-runsim`](simulator.md) wrote as it closed each file, and its status timeline | A simulated execution, on site or in CI |
| `--minknow <run>...` | The source runs re-hashed at report time, and the file counts in MinKNOW's `final_summary_*.txt` | A real MinKNOW run: a configuration test cell or a playback run |

Every file on the share is re-hashed and compared with the reference, not with ont-offload's own manifest or receipt, so a copy that changed after it was verified fails even though `manifest.sha256` and `VERIFIED.json` still agree with each other. With `--minknow` the source runs must still exist: run the report before cleanup deletes them. A `--minknow` folder that does not exist is refused without a report.

The execution window is `--from` and `--to`; by default it starts when the simulation started (or at the first event about a reference run) and ends now.

### Checks

| Id | Check | Expected |
| --- | --- | --- |
| VC-01 | Every run discovered | Each reference run has a `run_discovered` event |
| VC-02 | Every finished run completed | `COMPLETE`, `manifest.sha256` and `run.json` on the share for every run that finished at the source |
| VC-03 | Content identical to the reference | 0 missing, 0 changed, 0 extra files; every file re-hashed on the share |
| VC-04 | ont-offload's manifest agrees with the reference | `manifest.sha256` lists the same files with the same digests |
| VC-05 | Files written after the final summary copied | Every file newer than the final summary is on the share and matches |
| VC-06 | Rewritten files copied with their final content | Every file the simulator rewrote after closing it matches its final content |
| VC-07 | Aborted runs copied but not completed | Files of a run without a final summary are copied; no `COMPLETE` |
| VC-08 | Outage detected and recovered | Each `outage_started` has its `outage_ended`; content still identical |
| VC-09 | Restart resumes without copying again | After each restart no file is copied again with the same content |
| VC-10 | No unresolved failures | 0 `file_failed` and 0 `run_failed` for the reference runs |
| VC-11 | Receipts on the share | `VERIFIED.json` per completed run with the digest of its manifest |
| VC-12 | Time to COMPLETE | `COMPLETE` within the protocol's limit after the run ended at the source |
| VC-13 | Event stream complete | Every event of the execution parses; no sequence gaps |
| VC-14 | Records cover the execution | Events from before the first run to after the last completion, without gaps |
| VC-15 | Cleanup only deletes verified copies | No source run deleted whose copy failed a check |
| VC-16 | File counts agree with MinKNOW | pod5, fast5, fastq and bam files on the share match the final summary's counts |

Each check is **PASS**, **FAIL**, **INCONCLUSIVE** (its input is missing or unreadable, for example a drill that was not performed or a reference that cannot be read) or **N/A** (nothing to check, for example no aborted run). Each check names the requirements it verifies.

### Protocols and verdict

A protocol says which checks are required and which are shown for information, and sets the limit for VC-12. The built-in protocols:

| Protocol | Required | For |
| --- | --- | --- |
| `simulation` (default with `--sim`) | VC-01 to 07, VC-10, VC-13, VC-14 | Any simulated execution |
| `p2i-acceptance` | as `simulation`, plus VC-08, VC-09, VC-11, VC-12 | Site acceptance: run `ont-runsim --preset outage-drill`, take the share away and back during the run, and restart the service once |
| `minknow-mock` (default with `--minknow`) | VC-01 to 05, VC-10 to 14, VC-16 | A real MinKNOW run |

A site protocol is a TOML file passed with `--protocol <file>`:

```toml
name = "Site revalidation"
version = "2"
description = "What the protocol shows, printed on the report."
max_minutes_to_complete = 20
required = ["VC-01", "VC-02", "VC-03", "VC-10", "VC-13", "VC-14"]
informational = ["VC-11", "VC-12"]
```

The report prints the protocol's name, version and SHA-256. The verdict is **FAIL** if a required check fails, else **INCONCLUSIVE** if a required check is inconclusive, else **PASS**. The report is written in every case; the exit code is 0 (PASS), 3 (FAIL) or 5 (INCONCLUSIVE).

`--operator` and every `--deviation "<text>"` are printed on the report, and the sign-off block has lines for executed by, reviewed by and approved for clinical use. Reports of a simulation carry a SIMULATED DATA watermark.

### What the PDF shows

1. Title block: protocol, execution window, reference, mode, host and operator; the verdict with the number of required checks passed.
2. Acceptance checks: expected, requirements, observed and result for each check.
3. Runs against the reference: files expected, matched, missing, changed and extra, time to `COMPLETE`, and the files that differ (up to 200).
4. Timeline of the simulator's and ont-offload's events.
5. Test environment: version, destination and its file system, sources, settings and the SHA-256 of the configuration.
6. Record completeness, deviations, and execution and approval.

### Release evidence: `cargo xtask verify-report`

`cargo xtask verify-report` runs a simulated execution end to end on a development machine or in CI: `ont-runsim` writes runs while `watch --until-idle` offloads them to a temporary share on tmpfs, a second `watch` session restarts the daemon, `verify` writes the receipts, and `report verify --sim` checks it all. It fails unless the verdict is PASS. The release workflow runs it, so the verification report is part of every release's records. See `cargo xtask verify-report --help` for the share, preset and speed.
