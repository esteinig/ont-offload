# Setting up with init

`ont-offload init` writes a configuration for you, creates the folders it names, prepares the share (its folder and the sentinel), probes it, checks the result as `check-config` would, and prints a numbered list of what to do next. It starts from a preset, and every value can be changed with a flag.

```sh
ont-offload init                                                  # laptop test bench
ont-offload init --preset test --share /mnt/HPC/ont-offload-test  # rehearsal on the sequencer, as yourself
sudo ont-offload init --preset production --share /mnt/HPC/ont-offload --data /data/data-offload
```

Run `ont-offload init --print` (with the same flags) to see the file first: it changes nothing.

## Presets

| | `local` (default) | `test` | `production` |
| --- | --- | --- | --- |
| For | trying ont-offload on a laptop ([Testing on a laptop](local-testing.md)) | a rehearsal against the real share as a normal user, without the service | the service on a sequencer ([Deploying on a sequencer](deployment.md)) |
| Share (`dest_root`) | `/dev/shm/ont-share` (tmpfs) | `--share`, required: a folder of its own on the share | `--share`, required |
| `target_id` | `local-test` | `<host>-test` | `<host>` |
| Work folder | `~/ont-local` | `~/ont-offload-test` | none |
| Configuration | `<work>/config.toml` | `<work>/config.toml` | `/etc/ont-offload/config.toml` |
| `state_dir` | `<work>/state` | `<work>/state` | `/var/lib/ont-offload` |
| Log file | `<work>/logs/ont-offload.log` | `<work>/logs/ont-offload.log` | `/var/log/ont-offload/ont-offload.log` |
| Source folders | `<work>/data` | `/data` | `/data` |
| Allowed filesystems | `tmpfs` | `nfs`, `nfs4`, `cifs`, `smb3` | `nfs`, `nfs4`, `cifs`, `smb3` |
| Workers, rate limit | 2, 200MB/s | 4, 100MB/s | 4, 100MB/s |
| `min_free` on the share | 100MB | 500GiB | 500GiB |
| Timings | seconds (settle 5s) | short (settle 10s) | production (settle 30s) |
| `notify_command` | `<work>/notify.sh`, which appends to `notify.log` | as `local` | empty: fill it in |
| `cleanup.delete` | `dry-run` | `dry-run` | `off` |

## Sources

Every preset writes the three sources of the [P2i layout](transfer-folders.md):

| Source | Root | Layout | On the share | Cleanup |
| --- | --- | --- | --- | --- |
| `runs` | `<data>` | `minknow` | `dest_root/<experiment>/<sample>/<run>/` | may delete |
| `transfer` | `<data>/transfer` | `ready`: a folder with a `READY` file is one dataset | `dest_root/transfer/...` | never |
| `simulation` | `<data>/simulation` | `any-depth`, for `ont-runsim` campaigns | `dest_root/simulation/...` | may delete |

`transfer` and `simulation` lie inside `runs`; `check-config` notes this as a warning, which is expected. Leave a source out with `--no-runs`, `--no-transfer` or `--no-simulation`, or move one with `--runs`, `--transfer` or `--simulation DIR`.

## What it does, in order

1. Builds the file from the preset, the flags and `--set`, and parses it as strictly as the daemon does. A typo in a `--set` key stops here, before anything is written.
2. Refuses to go on if the configuration file exists (use `--force`; the old file is kept as `<file>.bak`).
3. Checks the share without writing to it: the mount below the share folder must have an allowed filesystem type (so a share that is not mounted is refused instead of filling the local disk), and a sentinel already in the folder must name this `target_id`. A sentinel naming another target is never replaced.
4. Creates the work, state, log, configuration and source folders, and the notify script of the `local` and `test` presets.
5. Refuses a share on the same device as a source, then creates the share folder and the sentinel, runs the write probe (who will own the copies, and with what mode) and reports the free space.
6. Writes the configuration and checks it with the destination guard, as `check-config` does.
7. Prints the next steps: the `export ONT_OFFLOAD_CONFIG=...` line, settings worth a manual look (lines marked `REVIEW` in the file), `check-config`, `bench`, starting the daemon or the service, a simulated run, a transfer, and a cleanup dry run.

With `--no-probe`, steps 3 and 5 are skipped and nothing is written to the share; use it when the share is not mounted yet. The next steps then say how to create the folder and the sentinel by hand.

Folders under `/etc`, `/var/lib` and `/var/log` need root: run the `production` preset with `sudo` (or give `--work-dir` and the path flags), then give the state and log folders to the service user as the next steps show.

## Changing values

| Flag | Setting |
| --- | --- |
| `--share DIR`, `--target-id ID` | `dest_root`, `target_id` |
| `--work-dir DIR` | the folder for the configuration, `state/`, `logs/` and `notify.sh` |
| `--data DIR`, `--runs DIR`, `--transfer DIR`, `--simulation DIR` | source roots |
| `--state-dir DIR`, `--log FILE`, `--output FILE` | `state_dir`, `logging.file`, where the file is written |
| `--fstype TYPE` (repeat) | `guard.allowed_fstypes` |
| `--workers N`, `--rate-limit RATE`, `--min-free SIZE` | `bandwidth.workers`, `bandwidth.rate_limit`, `transfer.min_free` |
| `--cleanup off\|dry-run\|on` | `cleanup.delete` |
| `--notify-command PROGRAM` | `outage.notify_command` (`""` for none) |
| `--set KEY=VALUE` (repeat) | anything else: `key`, `section.key`, or `source.<name>.key` |

`--set` takes a TOML value; a bare word is taken as text, so `--set transfer.settle=10s`, `--set transfer.max_attempts=5`, `--set source.runs.dest_starts_at=run` and `--set 'guard.allowed_fstypes=["nfs4"]'` all work. Relative paths are taken from the current folder.

## Exit codes

0 when the configuration was written and passed the guard; 2 when the plan is refused, the share is refused (nothing has been written to it then), a folder cannot be created, or the written configuration does not validate (the file stays, and the next steps start with what to fix).
