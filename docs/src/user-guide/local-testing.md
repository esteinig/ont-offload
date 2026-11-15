# Testing on a laptop

This runbook tries out ont-offload end to end on one computer, without a sequencer or an NFS share. A folder in `/dev/shm` (tmpfs, so a different filesystem from your home folder) stands in for the share, `ont-runsim` writes simulated runs into a local folder that stands in for `/data`, and the dashboard shows what happens. Every step below was run as written; a full pass takes about 20 minutes.

It does **not** test the NFS mount itself (root squash, `soft`/`hard` mount behaviour, network drops); that is done on the sequencer with the [deployment guide](deployment.md), and nightly by `scripts/real-share-test.sh` against real NFS and Samba servers.

## What you need

| Need | Why |
| --- | --- |
| Linux, or WSL2 on Windows | the destination guard reads `/proc/self/mountinfo`, so it cannot pass on macOS. On a Mac use a Linux VM or `docker run -it --tmpfs /dev/shm:rw,size=2g rust:1.85` and follow the steps inside it |
| Rust 1.85 or later | to build the binaries (or unpack a release archive instead) |
| About 300 MB free in `/dev/shm` and in your home folder | each `quick-2h` run is 67 MB; check with `df -h /dev/shm` |
| `sha256sum` | the independent checksum check (coreutils) |

Run everything as your normal user, not root. As root, `check-config` and `watch` warn that copies would be owned by root, which is correct but noisy.

## 1. Build

```sh
cd ont-offload
cargo build --release --features tui -p ont-offload -p ont-runsim
export PATH="$PWD/target/release:$PATH"
ont-offload --version && ont-runsim --version
```

## 2. Set up the test bench

```sh
scripts/local-test.sh setup
eval "$(scripts/local-test.sh env)"     # sets S, W, CFG and ONT_OFFLOAD_CONFIG
```

`setup` creates:

| Path | Stands in for |
| --- | --- |
| `$S` = `/dev/shm/ont-share` with the sentinel `.ont-offload-target` containing `laptop-test` | the NFS share (`dest_root`) |
| `$W` = `~/ont-local/data` | the sequencer's `/data` (`source.root`) |
| `$W/state` | `/var/lib/ont-offload` |
| `$W/ont-offload.log` | `/var/log/ont-offload/ont-offload.log` |
| `$W/notify.sh`, writing to `$W/notify.log` | your `notify_command` (e-mail, Teams) |
| `$W/config.toml` | `/etc/ont-offload/config.toml` |

Set `SHARE` or `WORK` before `setup` to use other folders. The share must be on tmpfs. The config differs from a sequencer's only in what a laptop needs: `allowed_fstypes = ["tmpfs"]`, 2 workers, a 100 MB free-space reserve, `source_free_alert = "0B"` (a laptop never has 200 GB free for the sequencer), short outage timings (warning after 1 minute), and `[cleanup] delete = "dry-run"` with `delete_after = "0s"`.

Open a second terminal for the dashboard and run the `export PATH` and `eval` lines there too.

## 3. Check the configuration

```sh
ont-offload check-config
```

Expect `destination guard OK: sentinel matches, tmpfs mounted from tmpfs at /dev/shm`, `share write probe OK as uid <you> ...` and `configuration OK`. If the guard fails, see [Troubleshooting](#troubleshooting).

## 4. Offload a live run

Terminal 1:

```sh
ont-runsim run --preset quick-2h --out "$W/data" --keep-free 1GB -q &
ont-offload watch --until-idle
```

`quick-2h` writes a 2-hour run at 60x speed: 47 files and 67 MB over about 2 minutes, with pod5, FASTQ and BAM batches, a sample sheet at the start and the final summary at the end. `watch` logs to `$W/ont-offload.log` (from `[logging] file`) and to the terminal. It copies files as they settle, then writes the manifest and `COMPLETE`, logs `run ...: COMPLETE, 47 files, 66930304 bytes`, and with `--until-idle` exits 0 once nothing is left to copy.

Terminal 2, while it runs:

```sh
ont-offload status                    # one-shot summary
ont-offload tui                       # dashboard; q quits
ont-offload tui --theme phosphor      # or matrix, see the dashboard guide
ont-offload tui --check               # shows the settings it found and that the log is readable
```

The dashboard only reads the state folder, so you can start and quit it at any time.

## 5. Check the copy

```sh
RUN=$(ls -d "$S"/runsim_quick/sample01/*/ | tail -1)
ont-runsim check --out "$W/data" --dest "$RUN"         # 47 of 47 files match
(cd "$RUN" && sha256sum --quiet -c manifest.sha256 && echo "sha256sum OK")
ont-offload verify "$RUN" --source                     # OK ... source matches too
```

`ont-runsim check` compares against what the simulator wrote, independently of ont-offload; `sha256sum` checks the manifest with a tool that is not ours; `verify --source` re-reads both sides and writes `VERIFIED.json`.

## 6. Outage drill

Taking the sentinel away makes the share look unmounted to the guard, which is what happens when NFS drops, without needing root. Start a fresh run first (`ont-runsim` needs a new or empty folder):

```sh
ont-runsim clean --out "$W/data" && mkdir "$W/data"
ont-runsim run --preset quick-2h --out "$W/data" --keep-free 1GB -q &
ont-offload watch --until-idle &
sleep 30
mv "$S/.ont-offload-target" "$W/sentinel.away"     # the share "goes away"
sleep 75
ont-offload status                                  # status: outage, workers paused, files waiting
cat "$W/notify.log"                                 # outage_started, then outage_warning after 1 minute
mv "$W/sentinel.away" "$S/.ont-offload-target"      # the share is back
wait                                                # watch resumes, completes and exits 0
tail -1 "$W/notify.log"                             # outage_ended ... resuming N file(s)
```

Then repeat step 5 for the new run. Expect one `destination unavailable` line when the outage starts and summaries every 30 seconds, not a line per failed check; the dashboard shows the outage banner and the alert. Copies already on the share are never touched during the outage.

The whole drill is also scripted (it writes its own config and sentinel in a temporary folder):

```sh
mkdir -p /dev/shm/ont-drill
ALLOWED='["tmpfs"]' OUTAGE_S=60 BIN=target/release \
  DOWN_CMD='mv /dev/shm/ont-drill/.ont-offload-target /dev/shm/drill.away' \
  UP_CMD='mv /dev/shm/drill.away /dev/shm/ont-drill/.ont-offload-target' \
  scripts/real-share-test.sh /dev/shm/ont-drill tmpfs    # ends with "nightly: tmpfs OK"
rm -r /dev/shm/ont-drill
```

## 7. Kill and restart

A crash or reboot in the middle of a run must not lose or duplicate anything:

```sh
ont-runsim clean --out "$W/data" && mkdir "$W/data"
ont-runsim run --preset quick-2h --out "$W/data" --keep-free 1GB -q &
ont-offload watch & sleep 40; kill -9 $!     # hard kill mid-run
ont-offload watch --until-idle               # picks the run up from the journal
```

Then repeat step 5: still 47 of 47 files match. A partly written file left by the kill is never mistaken for a finished copy; it is copied again.

## 8. Cleanup dry run

```sh
ont-offload cleanup --dry-run
```

Expect `would_delete .../data/runsim_quick/...: every gate passed` and `1 would be deleted`. Nothing is deleted. The warning that `delete_after` is shorter than `watch_completed_for` is expected with this test config. To see a real deletion of simulated data only, set `delete = "on"` in `$CFG` and run `ont-offload cleanup`; the source run folder goes away and the copy on the share stays. Set it back to `dry-run` afterwards.

## 9. Parallel-run rehearsal

`scripts/compare-with-legacy.sh` checks the legacy service's copy of each run against ont-offload's manifest. On the laptop the source folder can play the legacy copy, since its runs sit at the same relative path:

```sh
BIN=ont-offload scripts/compare-with-legacy.sh "$S" "$W/data" "$W/compare.txt"
cat "$W/compare.txt"       # OK for runs still in $W/data, MISSING for runs you cleaned
```

To see a mismatch, copy a run with `cp -r` to a third folder, delete or edit one file in it, and compare against that folder: the report shows `DIFFERS` and the script exits 1.

## 10. A longer test (optional)

```sh
ont-runsim presets                                      # all presets with sizes
ont-runsim run --preset outage-drill --out "$W/data" --keep-free 5GB --dry-run
```

`outage-drill` simulates 6 hours with late files (3.9 GB). Add `--size-scale 0.1` to fit a small `/dev/shm`, and take the share away for a few minutes during it as in step 6. Run `watch` without `--until-idle` and stop it with `Ctrl-C` to check that it stops cleanly.

## 11. Tear down

```sh
pkill ont-runsim; pkill -f "ont-offload watch"    # if anything is still running
scripts/local-test.sh teardown                    # removes $S and $W
```

`teardown` refuses to remove folders that `setup` did not make (a work folder without its `.local-test-bench` marker, or a share without the `laptop-test` sentinel), so put the sentinel back if you stopped mid-drill.

## Troubleshooting

| Message | Cause and fix |
| --- | --- |
| `source.root ... does not exist` | the data folder was removed by `ont-runsim clean`; run `mkdir "$W/data"` |
| `... is not empty and was not created by ont-runsim` | `--out` must be new, empty or an earlier ont-runsim folder; `ont-runsim clean --out "$W/data" && mkdir "$W/data"` |
| guard: `mount table /proc/self/mountinfo cannot be read` | you are on macOS; use Linux, WSL2, a VM or Docker |
| guard: `it is on a ext4 filesystem ...; allowed: tmpfs` or `on the same device as source.root` | the share folder is not on tmpfs; use `/dev/shm`, or set `SHARE` to another tmpfs mount |
| `sentinel .ont-offload-target cannot be read` | the sentinel is still moved away from an outage drill; move it back |
| `below min_free` or `would go below transfer.min_free` | `/dev/shm` is nearly full; remove old runs from `$S` or shrink with `--size-scale` |
| `tui --check` says the log is not readable | `watch` has not run yet, so the log file does not exist |
