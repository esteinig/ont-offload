# Run simulator (ont-runsim)

`ont-runsim` writes MinKNOW-shaped runs to disk over time, so ont-offload can be tested end to end, in CI and by hand on a sequencer. It is a test tool, not part of the medical software, and it does not depend on the `ont-offload` crate.

Every simulated run folder contains `SIMULATED_RUN.txt`, and every file the simulator writes is recorded in a SHA-256 manifest, so an offloaded copy can be checked against exactly what was written.

## Quick start

```sh
ont-runsim presets
ont-runsim run --preset quick-2h --out /data/ont-runsim      # 2 simulated hours in 2 minutes
ont-runsim status --out /data/ont-runsim
ont-runsim check --dest /mnt/nas/ont/runsim_quick/sample01/<run folder> --out /data/ont-runsim
ont-runsim clean --out /data/ont-runsim
```

`/data/ont-runsim` is ignored by ont-offload's default `[source] ignore`, so simulated runs are only offloaded when you point a test configuration's `source.root` at it.

## Commands

| Command | What it does |
| --- | --- |
| `run --preset NAME` or `run --scenario FILE` | Writes runs over time. Overrides: `--hours`, `--speed`, `--seed`, `--size-scale`. `--keep-free 500GB` stops when the disk gets full (exit 3). `--dry-run` prints the plan only. |
| `presets` | Lists `quick-2h`, `promethion-24h` and `outage-drill`. |
| `profile RUN -o profile.json` | Records the timeline of a finished real run from file metadata only (no content is read). |
| `status [--json]` | Progress of the simulation in an output folder. |
| `check --dest DIR` | Compares a copy with the manifest: missing, changed and extra files. Exit 1 on any difference. ont-offload's own `manifest.sha256`, `COMPLETE` and `VERIFIED.json` are ignored. |
| `clean` | Removes only the run folders the simulator created (listed in `.ont-runsim/runs.txt` and labelled), then empty parent folders and its state folder. Anything else in the folder stays. |

The output folder must be new, empty, or a previous simulator folder; the simulator refuses any other folder, and refuses to start while another simulation runs there.

## Scenarios

```toml
seed = 42
hours = 72.0              # simulated run length, any number of hours
speed = 60.0              # simulated seconds per wall-clock second
content = "fake"          # or "replay": bytes taken from an existing run
timeline = "synthetic"    # or "profile": timing recorded with `ont-runsim profile`
profile = "run.json"
extend = "repeat-batches" # or "stretch", when hours differ from the profile

[fake]
fill = "random"           # or "zero"
size_scale = 1.0

[replay]
run = "/data/exp/sample/20240101_1200_1A_PAW12345_abcdef12"

[synthetic]
batch_every = "10m"
pod5_batch = "500MB"
fastq_pass_batch = "40MB"
fastq_fail_batch = "5MB"
barcodes = 0

[[runs]]
experiment = "exp1"
sample = "sample1"
position = "1A"
start_after = "0s"

[faults]
abort_after = "3h"        # run stops without a final summary
rewrite_closed = 2        # closed batch files rewritten a minute later
late_files = 1            # files that appear after the final summary
```

**Fake content** has the right names, extensions, folders and sizes; text files get text, binary files random or zero bytes. The sample sheet and final summary carry real fields (flow cell, sample, file counts), and the final summary is always written last. **Replay content** takes bytes from an existing run, repeating a source file when the planned file is larger.

**Profiles** record when each file of a real run appeared, how long it grew and how big it ended. `repeat-batches` keeps the batch rhythm and repeats batches (renamed `_r1`, `_r2`, ...) to simulate longer runs than the original, or cuts them for shorter ones; `stretch` scales all times.

## Live state

`<out>/.ont-runsim/status.json` is rewritten atomically about once a second (state, simulated time, files and bytes per run); `<out>/.ont-runsim/events.jsonl` logs run start, completion, aborts and faults. Manifests are in `<out>/.ont-runsim/manifests/<run folder>.sha256` and list closed files only while a run is live.
