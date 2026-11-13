# Usage

| Command | Purpose | Available from |
| --- | --- | --- |
| `ont-offload watch [--until-idle]` | Daemon: discover runs under the source root, copy live and completed runs; see [Running the daemon](watch.md) | v0.3 |
| `ont-offload run <path> [--completed]` | Copy one completed run in the foreground; see [Offloading a run](offload.md) | v0.2 |
| `ont-offload verify <run> [--source]` | Re-hash a destination run against its manifest; writes `VERIFIED.json` | v0.2 |
| `ont-offload status [--json]` | Print the live state that `watch` keeps in `state_dir/state.json`; see [Live state and status](watch.md#live-state-and-status) | v0.4 |
| `ont-offload check-config` | Validate config and paths, print the effective settings, the detected runs with their destination folders, and the destination guard result | v0.1 |
| `ont-offload bench [--size 2GB] [--max-streams 16] [--no-chunks]` | Measure the share; suggest `workers`, `rate_limit` and `chunk_size`, recorded in `state_dir/bench.json`; see [Tuning with bench](watch.md#tuning-with-bench) | v0.4 |
| `ont-offload cleanup [--dry-run] [<run>]` | Gated deletion of offloaded source runs | v0.5 |

Global options: `--config`, `--log-level`, `--log-file`, `--log-format text|json`.

## Exit codes

| Code | Meaning |
| --- | --- |
| 0 | All done and verified |
| 1 | Internal error, or command not implemented yet |
| 2 | Configuration or destination guard refused (also usage errors) |
| 3 | One or more files or runs failed |
| 4 | Integrity mismatch not resolved |
| 75 | Restart needed (`run`: the share failed mid-run; `watch`: every worker stalled); safe to resume |
| 130 | Interrupted; journal consistent, safe to resume |
