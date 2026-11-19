# Configuration

The configuration is TOML; unknown keys are rejected. A complete example with every setting is in [`packaging/ont-offload.example.toml`](https://github.com/esteinig/ont-offload/blob/dev/packaging/ont-offload.example.toml).

| Section | What it controls |
| --- | --- |
| top level | `dest_root`, `target_id` (must match `dest_root/.ont-offload-target`), `state_dir`, `dest_starts_at` (`experiment`, `sample` or `run`: the MinKNOW level that becomes the top folder on the share; see [Offloading a run](offload.md#where-a-run-goes-dest_starts_at)) |
| `[source]` or `[[source]]` | One `[source]` table, or several `[[source]]` entries (v0.8; see [Several sources and user transfer folders](transfer-folders.md)), each with `name`, `dest` (folder below `dest_root`), `dest_starts_at` and `cleanup`, and: `root` (default `/data`), `layout` (`minknow`, `any-depth`, or `ready` for user datasets marked with a `READY` file), `run_markers` (default `sample_sheet_*.csv`; `READY` for `ready`), `complete_markers` (default `final_summary_*.txt`; for `ready` the run markers), `ignore`, `full_rescan_every` (default `"10m"`: how often every run folder is listed in full; in between only changed folders are). A marker ending in `/` matches a folder, e.g. `"pod5/"` for runs without a sample sheet |
| `[selection]` | `include` / `exclude` globs per run, `defer_until_complete` |
| `[bandwidth]` | `workers`, `rate_limit` (total, e.g. `"100MB/s"`), optional `network_speed` check |
| `[transfer]` | chunk size, `verify` (`inflight` or `readback`), `on_existing`, `settle` (a live file is copied once unchanged this long), `settle_after_complete` (quiet time after the complete marker before a run is completed), `stall_timeout`, `max_attempts`, `min_free` (default `"500GiB"`: copying pauses rather than leave less than this free on the share), `watch_completed_for` (default `"24h"`: how long a completed run is watched for changes) |
| `[outage]` | `check_every_max` (longest re-check interval), `warn_after`, `alert_after`, `remind_every`, `log_every` (summary interval), `stable_before_resume`, `source_free_alert` (default `"200GB"`; `"0B"` turns it off), `notify_command` (program and arguments run for every event that needs a person; see [Events and notifications](watch.md#events-and-notifications)). See [Running the daemon](watch.md#when-the-share-goes-away) |
| `[cleanup]` | `delete` (`off`, `dry-run`, `on`; default `off`), `gate` (`storage-verify`, `readback`, `both`), `delete_after` (default `"7d"`), `rehash_source` (default `true`), `only_when_free_below` (default `"always"`). See [Deleting offloaded runs](cleanup.md) |
| `[guard]` | `allowed_fstypes` (default `nfs`, `nfs4`, `cifs`, `smb3`) |
| `[state]` | `interval` (default `"2s"`: how often `state.json` is rewritten, at least 1 s), `events_max_size` (default `"64MB"`: `events.jsonl` is rotated at this size, at least 1 MiB; older segments move to `state_dir/events/` and are never deleted), `free_alert` (default `"5GB"`: raise `records_space_low` when the disk of `state_dir` or of `logging.file` has less free space; `"0B"` turns it off). See [Live state and status](watch.md#live-state-and-status) |
| `[logging]` | `file` (the daemon reopens it by name after logrotate renames it; see `packaging/ont-offload.logrotate`) and `format`. Only `watch` uses them; other commands log to the console unless you pass `--log-file` or `--log-format`, which always win |
| `[tui]` | the dashboard: `theme` (`plain`, `matrix`, `phosphor`, `synthwave`; default `plain`), `refresh` (200ms to 60s; default `"1s"`), `bell` (default `true`). Flags and `ONT_OFFLOAD_TUI_*` variables override them; see [Monitoring dashboard](tui.md#settings-and-where-they-come-from) |
| `[report]` | `site` (printed in every title block), `prefix` (default `"OOF"`: report ids and file names; letters, digits and `-`) and `logo` (PNG, JPEG or SVG). See [Reports](reports.md#site-name-and-logo) |

Sizes and rates need explicit units: `KB`, `MB`, `GB`, `TB` (powers of 1000), `KiB`, `MiB`, `GiB`, `TiB` (powers of 1024), rates as `"100MB/s"`, `"800Mbit/s"`, `"1Gbit/s"` (also `Mbps`, `Gbps`) or `"unlimited"`. Durations are written as `"30s"`, `"5m"`, `"1h"`, `"7d"`.

Run `ont-offload check-config` after every change. It lists every problem at once with its key, and otherwise prints the effective settings: resolved paths and devices, workers, the total and per-worker rate, and the share of `network_speed`. A `rate_limit` above `network_speed` is refused; above 90% of it gives a warning.

Suggested bandwidth settings, as a starting point before `ont-offload bench` measures the real share (see [Tuning with bench](watch.md#tuning-with-bench)):

| Network connection | workers | rate_limit | Share of the link |
| --- | --- | --- | --- |
| 100 Mbit/s | 2 | `"10MB/s"` | 80% |
| 1 Gbit/s | 4 | `"100MB/s"` | 80% |
| 10 Gbit/s | 8 | `"1000MB/s"` | 80% |

`rate_limit` is the one setting that changes without a restart: edit it and run `sudo systemctl reload ont-offload`.
