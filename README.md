# ont-offload

Verified offload of Oxford Nanopore sequencing runs (live or completed) from a sequencer's local disk to mounted network storage, built for clinical use.

[![CI](https://github.com/esteinig/ont-offload/actions/workflows/ci.yml/badge.svg?branch=dev)](https://github.com/esteinig/ont-offload/actions/workflows/ci.yml)
[![Docs](https://img.shields.io/badge/docs-mdBook-blue)](https://esteinig.github.io/ont-offload)
[![License: AGPL-3.0-or-later](https://img.shields.io/badge/license-AGPL--3.0--or--later-blue)](LICENSE)

> **Status: v0.8 (in development).** `ont-offload watch` copies live and completed runs with parallel workers shared fairly between runs and a total rate limit, rides out share outages of any length, keeps completed runs under watch, publishes its live state (`state.json`, `events.jsonl`, `notify_command`) for `ont-offload status`, and, when enabled, deletes source runs that pass every deletion gate (off by default; run it in dry-run first). `bench` suggests settings for a share; `run` copies one completed run, `verify` checks a copy and writes the receipt, and `cleanup` runs a deletion pass by hand. `ont-offload tui` is a read-only terminal dashboard of the daemon, with alerts, staleness warnings and four themes (`plain`, `matrix`, `phosphor`, `synthwave`), started with flags, `ONT_OFFLOAD_*` environment variables or the `ont-offload-tui` launcher. v0.7 prepares deployment: a dedicated service user, a share write probe in `check-config`, log rotation, `verify --against` for a parallel run beside a legacy service, and a [deployment guide](docs/src/user-guide/deployment.md). v0.8 watches several sources in one daemon, such as the MinKNOW runs plus a [transfer folder](docs/src/user-guide/transfer-folders.md) where users drop datasets marked with a `READY` file. Known limitations and their controls are listed in [the user guide](docs/src/user-guide/limitations.md). See the [staged plan](docs/src/developer-guide/architecture.md#staged-implementation-plan).

## What it does

- Copies each run with its original directory structure, from the MinKNOW layout under `/data`, to an NFS or SMB share.
- Watches several sources at once, for example the runs and a transfer folder where users drop datasets marked `READY`.
- Hashes every file with SHA-256 while copying, commits by atomic rename, and writes a `sha256sum -c` compatible manifest and `run.json` per run, then `COMPLETE`.
- Refuses to write unless the share proves it is mounted (sentinel file, filesystem type, device id), and re-checks before every file.
- Pauses through network outages of any length and resumes on its own.
- Caps total bandwidth with one plain `rate_limit` across parallel workers.
- Publishes live, machine-readable state (`state.json`, `events.jsonl`) for a dashboard.
- Optionally deletes source runs, only after a storage-side verify receipt or read-back, a retention period and a source re-hash.

The workspace also contains `ont-runsim`, a test tool that writes simulated MinKNOW runs to disk over time.

## Quick start

```sh
cargo install --locked --features tui --git https://github.com/esteinig/ont-offload ont-offload
ont-offload --help
sudo ont-offload init --preset production --share /mnt/nas/ont   # writes the config, prepares the share, prints next steps
ont-offload check-config --config /etc/ont-offload/config.toml
ont-offload watch --config /etc/ont-offload/config.toml        # or: systemctl enable --now ont-offload
ont-offload run --config /etc/ont-offload/config.toml /data/<experiment>/<sample>/<run>
ont-offload verify /mnt/nas/ont/<experiment>/<sample>/<run>
```

An example configuration is in [`packaging/ont-offload.example.toml`](packaging/ont-offload.example.toml) and a systemd unit in [`packaging/ont-offload.service`](packaging/ont-offload.service).

## Exit codes

| Code | Meaning |
| --- | --- |
| 0 | All done and verified |
| 1 | Internal error, or command not implemented yet |
| 2 | Configuration or destination guard refused (also usage errors) |
| 3 | One or more files or runs failed |
| 4 | Integrity mismatch not resolved |
| 75 | Restart needed (workers stuck after an outage; safe to resume) |
| 130 | Interrupted (journal consistent; safe to resume) |

## Development

```sh
cargo xtask test      # unit, integration, doc tests
cargo xtask lint      # rustfmt + clippy -D warnings
cargo xtask trace     # traceability (medtrace, or the built-in check without it)
cargo xtask ci        # everything CI runs
```

See [CONTRIBUTING.md](CONTRIBUTING.md) for the branching model, commit rules and traceability tags, and the [developer guide](docs/src/developer-guide/setup.md).

## Regulatory

Developed under IEC 62304 (software safety class B) with [medtrace](docs/src/developer-guide/compliance.md) traceability: requirements, software items and hazards live in [`compliance/`](compliance/), code carries `Implements:` tags and tests carry `Verifies:` tags.

## License

Copyright (C) 2026 Eike Steinig. Licensed under the GNU Affero General Public License v3.0 or later; see [LICENSE](LICENSE).
