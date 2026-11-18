# Usage

| Command | Purpose | Available from |
| --- | --- | --- |
| `ont-offload watch` | Daemon: discover runs under the source root, copy live and completed runs | v0.3 |
| `ont-offload run <path> [--completed]` | Copy one completed run in the foreground; see [Offloading a run](offload.md) | v0.2 |
| `ont-offload verify <run> [--source]` | Re-hash a destination run against its manifest; writes `VERIFIED.json` | v0.2 |
| `ont-offload status [--json]` | Print the live-state snapshot | v0.4 |
| `ont-offload check-config` | Validate config and paths, print the effective settings, the detected runs with their destination folders, and the destination guard result | v0.1 |
| `ont-offload bench` | Measure the share; suggest `workers` and `rate_limit` | v0.4 |
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
| 75 | Restart needed (the share failed mid-run, or workers stuck after an outage); safe to resume |
| 130 | Interrupted; journal consistent, safe to resume |
