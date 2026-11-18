# Configuration

The configuration is TOML; unknown keys are rejected. A complete example with every setting is in [`packaging/ont-offload.example.toml`](https://github.com/esteinig/ont-offload/blob/dev/packaging/ont-offload.example.toml).

| Section | What it controls |
| --- | --- |
| top level | `dest_root`, `target_id` (must match `dest_root/.ont-offload-target`), `state_dir` |
| `[source]` | `root` (default `/data`), `layout` (`minknow` or `any-depth`), `run_markers` (default `sample_sheet_*.csv`), `complete_markers` (default `final_summary_*.txt`), `ignore` |
| `[selection]` | `include` / `exclude` globs per run, `defer_until_complete` |
| `[bandwidth]` | `workers`, `rate_limit` (total, e.g. `"100MB/s"`), optional `network_speed` check |
| `[transfer]` | chunk size, `verify` (`inflight` or `readback`), `on_existing`, settle times, stall timeout, retries, free-space floor |
| `[outage]` | check interval, warning and alert times, log summaries, resume stability window, `notify_command` |
| `[cleanup]` | `delete` (`off`, `dry-run`, `on`), `gate` (`storage-verify`, `readback`, `both`), `delete_after`, `rehash_source` |
| `[guard]` | allowed filesystem types |
| `[logging]` | log file and format |

Sizes and rates need explicit units: `KB`, `MB`, `GB`, `TB` (powers of 1000), `KiB`, `MiB`, `GiB`, `TiB` (powers of 1024), rates as `"100MB/s"`, `"800Mbit/s"`, `"1Gbit/s"` (also `Mbps`, `Gbps`) or `"unlimited"`. Durations are written as `"30s"`, `"5m"`, `"1h"`, `"7d"`.

Run `ont-offload check-config` after every change. It lists every problem at once with its key, and otherwise prints the effective settings: resolved paths and devices, workers, the total and per-worker rate, and the share of `network_speed`. A `rate_limit` above `network_speed` is refused; above 90% of it gives a warning.

Suggested bandwidth settings:

| Network connection | workers | rate_limit | Share of the link |
| --- | --- | --- | --- |
| 100 Mbit/s | 2 | `"10MB/s"` | 80% |
| 1 Gbit/s | 4 | `"100MB/s"` | 80% |
| 10 Gbit/s | 8 | `"1000MB/s"` | 80% |
