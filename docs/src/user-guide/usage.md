# Usage

| Command | Purpose | Available from |
| --- | --- | --- |
| `ont-offload watch [--until-idle]` | Daemon: discover runs under the source root, copy live and completed runs; see [Running the daemon](watch.md) | v0.3 |
| `ont-offload run <path> [--completed]` | Copy one completed run in the foreground; see [Offloading a run](offload.md) | v0.2 |
| `ont-offload verify <run> [--source]` | Re-hash a destination run against its manifest; writes `VERIFIED.json` | v0.2 |
| `ont-offload verify <run> --against <folder>` | Check another copy of the run (the legacy service's, in a [parallel run](deployment.md#6-parallel-run-beside-the-legacy-service)) against the run's manifest; writes nothing | v0.7 |
| `ont-offload status [--json]` | Print the live state that `watch` keeps in `state_dir/state.json`; see [Live state and status](watch.md#live-state-and-status) | v0.4 |
| `ont-offload init [--preset local\|test\|production] [--share DIR] [...] [--print]` | Write a configuration from a preset, create its folders, prepare and probe the share, check the result and print the next steps; see [Setting up with init](init.md) | v0.9 |
| `ont-offload check-config` | Validate config and paths, print the effective settings, the detected runs with their destination folders, the destination guard result and (v0.7) a write probe on the share showing the owner and mode the copies will get | v0.1 |
| `ont-offload bench [--size 2GB] [--max-streams 16] [--no-chunks]` | Measure the share; suggest `workers`, `rate_limit` and `chunk_size`, recorded in `state_dir/bench.json`; see [Tuning with bench](watch.md#tuning-with-bench) | v0.4 |
| `ont-offload cleanup [--dry-run] [<run>]` | Gated deletion of offloaded source runs; see [Deleting offloaded runs](cleanup.md) | v0.5 |
| `ont-offload tui [--state-dir] [--log] [--theme] [--refresh] [--bell on\|off] [--check]` | Read-only terminal dashboard of the daemon (build feature `tui`, included in releases); settings also from `ONT_OFFLOAD_*` variables; see [Monitoring dashboard](tui.md) | v0.6 |
| `ont-offload report status [--month M \| --last 7d \| --from --to] [--out] [--format]` | Status report of a period from the records: JSON and PDF; see [Reports](reports.md#status-report) | v0.9 |
| `ont-offload report verify --sim <out> \| --minknow <run>... [--protocol] [--operator]` | Verification report of an execution against an independent reference; exits 0, 3 or 5; see [Reports](reports.md#verification-report) | v0.9 |

Global options: `--config`, `--log-level`, `--log-file`, `--log-format text|json`. `watch` takes its log file and format from `[logging]` in the config when these flags are not given; every other command logs to the console only unless you pass them.

## Exit codes

| Code | Meaning |
| --- | --- |
| 0 | All done and verified |
| 1 | Internal error, or command not implemented yet |
| 2 | Configuration or destination guard refused (also usage errors) |
| 3 | One or more files or runs failed; `report verify`: the verdict is FAIL |
| 4 | Integrity mismatch not resolved |
| 5 | `report verify`: the verdict is INCONCLUSIVE (a required check had no input) |
| 75 | Restart needed (`run`: the share failed mid-run; `watch`: every worker stalled); safe to resume |
| 130 | Interrupted; journal consistent, safe to resume |
