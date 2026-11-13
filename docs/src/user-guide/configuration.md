# Configuration

The configuration is TOML; unknown keys are rejected. A complete example with every setting is in [`packaging/ont-offload.example.toml`](https://github.com/esteinig/ont-offload/blob/dev/packaging/ont-offload.example.toml).

| Section | What it controls |
| --- | --- |
| top level | `dest_root`, `target_id` (must match `dest_root/.ont-offload-target`), `state_dir`, `dest_starts_at` (`experiment`, `sample` or `run`: the MinKNOW level that becomes the top folder on the share; see [Offloading a run](offload.md#where-a-run-goes-dest_starts_at)) |
| `[source]` | `root` (default `/data`), `layout` (`minknow` or `any-depth`), `run_markers` (default `sample_sheet_*.csv`), `complete_markers` (default `final_summary_*.txt`), `ignore`. A marker ending in `/` matches a folder, e.g. `"pod5/"` for runs without a sample sheet |
| `[selection]` | `include` / `exclude` globs per run, `defer_until_complete` |
| `[bandwidth]` | `workers`, `rate_limit` (total, e.g. `"100MB/s"`), optional `network_speed` check |
| `[transfer]` | chunk size, `verify` (`inflight` or `readback`), `on_existing`, `settle` (a live file is copied once unchanged this long), `settle_after_complete` (quiet time after the complete marker before a run is completed), `stall_timeout`, `max_attempts`, free-space floor |
| `[outage]` | `check_every_max` (longest re-check interval), `warn_after`, `alert_after`, `remind_every`, `log_every` (summary interval), `stable_before_resume`; `notify_command` comes in v0.4. See [Running the daemon](watch.md#when-the-share-goes-away) |
| `[cleanup]` | `delete` (`off`, `dry-run`, `on`), `gate` (`storage-verify`, `readback`, `both`), `delete_after`, `rehash_source` |
| `[guard]` | `allowed_fstypes` (default `nfs`, `nfs4`, `cifs`, `smb3`) |
| `[logging]` | log file and format |

Sizes and rates need explicit units: `KB`, `MB`, `GB`, `TB` (powers of 1000), `KiB`, `MiB`, `GiB`, `TiB` (powers of 1024), rates as `"100MB/s"`, `"800Mbit/s"`, `"1Gbit/s"` (also `Mbps`, `Gbps`) or `"unlimited"`. Durations are written as `"30s"`, `"5m"`, `"1h"`, `"7d"`.

Run `ont-offload check-config` after every change. It lists every problem at once with its key, and otherwise prints the effective settings: resolved paths and devices, workers, the total and per-worker rate, and the share of `network_speed`. A `rate_limit` above `network_speed` is refused; above 90% of it gives a warning.

Suggested bandwidth settings:

| Network connection | workers | rate_limit | Share of the link |
| --- | --- | --- | --- |
| 100 Mbit/s | 2 | `"10MB/s"` | 80% |
| 1 Gbit/s | 4 | `"100MB/s"` | 80% |
| 10 Gbit/s | 8 | `"1000MB/s"` | 80% |
