# Several sources and user transfer folders

One ont-offload daemon can watch several folders on the sequencer, each with its own rules, and copy them all to the same share. The common case is a sequencer that has the usual MinKNOW runs under `/data` plus a folder such as `/data/transfer`, where people drop datasets of their own (basecalls, analysis output, files from another instrument) to be sent to the share. Both are watched by one service, with one rate limit, one outage handling, one state folder and one dashboard.

Each folder is a `[[source]]` entry in the configuration. A configuration with a single `[source]` table, as in earlier versions, is still one source and behaves exactly as before.

## Example

```toml
dest_root = "/mnt/nas/ont"
target_id = "lab-nas-01"
state_dir = "/var/lib/ont-offload"

# MinKNOW runs, as before: /data/<experiment>/<sample>/<run>/ -> dest_root/<experiment>/<sample>/<run>/
[[source]]
name = "runs"
root = "/data"
layout = "minknow"

# Datasets dropped by users: /data/transfer/<anything>/ with a READY file -> dest_root/transfer/<anything>/
[[source]]
name = "transfer"
root = "/data/transfer"
layout = "ready"
dest = "transfer"
```

Run `ont-offload check-config` after the change. It prints each source with its settings and every run or dataset it found with the folder it will go to:

```text
  source[1] root             /data (device 2049, layout minknow, name runs)
  source[1] dest_starts_at   experiment (runs go to dest_root/<experiment>/<sample>/<run>/)
  source[1] run markers      sample_sheet_*.csv
  source[1] complete markers final_summary_*.txt
  source[1] cleanup          may delete its runs
  source[2] root             /data/transfer (device 2049, layout ready, name transfer)
  source[2] dest_starts_at   experiment (runs go to dest_root/transfer/<path below the root>/)
  source[2] run markers      READY
  source[2] complete markers READY
  source[2] cleanup          never deletes its runs
  dest_root                  /mnt/nas/ont (device 53, target_id lab-nas-01)
  ...
runs detected: 2
  runs/PROJ01/S1/20261115_1012_1A_PAW12345_ab12cd34  [sample_sheet_PAW12345_20261115_1012_ab12cd34.csv; running or no complete marker] -> /mnt/nas/ont/PROJ01/S1/20261115_1012_1A_PAW12345_ab12cd34
  transfer/alice/basecalls-0315  [READY; complete: READY] -> /mnt/nas/ont/transfer/alice/basecalls-0315
```

`/data/transfer` lies inside `/data`. That is allowed: the inner source owns that folder and the outer one never looks into it (`check-config` notes this as a warning). With the default MinKNOW layout the outer source would not have picked up user datasets anyway, because they have no sample sheet at the run level.

## Settings of a source

| Key | Default | Meaning |
| --- | --- | --- |
| `name` | the root's folder name | Shown before every run of the source (`transfer/alice/set1`) in logs, events, alerts, `status` and the dashboard, and recorded in `run.json` and `state.json` (`source_name`). Letters, digits, `-`, `_` and `.` |
| `root` | `/data` | The folder to watch |
| `layout` | `minknow` | `minknow` (runs at `<experiment>/<sample>/<run>/`), `any-depth` (any folder with a run marker) or `ready` (user datasets, below) |
| `run_markers`, `complete_markers` | by layout | `minknow` and `any-depth`: `sample_sheet_*.csv` and `final_summary_*.txt`. `ready`: `READY` for both |
| `ignore`, `full_rescan_every` | as for `[source]` | Folders never treated as runs, and the full rescan interval |
| `dest` | empty | Folder below `dest_root` for this source's runs. Empty means `dest_root` itself |
| `dest_starts_at` | the top-level setting | Which level becomes the top folder below `dest` |
| `cleanup` | off for `ready`, on otherwise | Whether `[cleanup]` may delete this source's runs from the sequencer (see below) |

`[selection]`, `[bandwidth]`, `[transfer]`, `[outage]`, `[cleanup]` and `[guard]` stay shared by all sources.

The rules that keep sources apart are checked by `check-config` and at start-up (requirement SRS-SRC-003, hazard HAZ-021):

- names and roots are distinct;
- each source has its own destination folder; folders may not lie inside each other, and at most one source may use `dest_root` itself (empty `dest`);
- the share must be on another device than every source root;
- a run whose folder would still land in another source's folder (for example a MinKNOW sample folder named `transfer` with `dest_starts_at = "sample"`) is refused with an alert, and nothing of it is written.

## How users drop a dataset (layout `ready`)

Give users these three rules:

1. Make a folder for the dataset below `/data/transfer`, at any depth (for example `/data/transfer/alice/basecalls-0315/`), and copy everything into it. Subfolders are fine.
2. **When the copy is finished**, create an empty file named `READY` in that folder: `touch /data/transfer/alice/basecalls-0315/READY`.
3. Leave the folder in place until the dataset shows as complete (`ont-offload status` or the dashboard). Files added or changed within a day of that are copied again; after that, start a new folder.

What ont-offload then does:

- A folder without `READY` is never touched, however long it sits there.
- Once `READY` is there, the folder and everything below it is one dataset. A `READY` in a subfolder is just a file of the dataset.
- Each file is copied once it has not changed for `transfer.settle`, and the dataset is completed (manifest, `run.json`, `COMPLETE`) after `transfer.settle_after_complete` without changes, exactly like a MinKNOW run. It goes to `dest_root/<dest>/<same path below the root>/`.
- A dataset that changes while it is still watched (`transfer.watch_completed_for`, default 24 h) has `COMPLETE` withdrawn and the changed files copied again.
- Hidden files and `*.tmp` are excluded by the default `[selection]`.

Writing `READY` before the copy is finished is the main risk (hazard HAZ-020). The settle times catch a copy that is still running, and later changes reopen the dataset, but a file added after the watch window is not copied. That is why rule 1 says copy first and rule 2 says `READY` last.

Mark only the top folder. If a subfolder was marked and offloaded on its own first, and its parent gets a `READY` later, the parent would write into the subfolder's copy; it is refused with the alert "destination … holds the destination … of run …; not mixing the two". Move one of the two away, or remove the parent's `READY`.

## Cleanup

Deletion is off by default for `ready` sources: user datasets stay on the sequencer until their owners remove them (hazard HAZ-022). The cleanup pass only looks at runs of sources with `cleanup` on, and `ont-offload cleanup <path>` refuses a dataset of a source with cleanup off. Runs of a source that has been removed from the configuration are never deleted either.

If you do want offloaded user datasets removed, set `cleanup = true` on that source. `check-config` then warns that datasets users dropped will be deleted once offloaded. All the usual gates of [Deleting offloaded runs](cleanup.md) still apply, and `[cleanup] delete = "dry-run"` shows what would happen first.

## Monitoring

With several sources, every run is named `<source name>/<path below its root>` in `ont-offload status`, the dashboard, the log, `events.jsonl` and alerts, and `state.json` has `source_name` for each run. The source free-space alert (`outage.source_free_alert`) uses the fullest of the source disks.

## Trying it on a laptop

On the [laptop test bench](local-testing.md), add a second source to `$CFG`. The single `[source]` table becomes the first `[[source]]`:

```sh
mkdir -p "$W/transfer"
sed -i 's/^\[source\]$/[[source]]/' "$CFG"
cat >> "$CFG" <<EOF

[[source]]
name = "transfer"
root = "$W/transfer"
layout = "ready"
dest = "transfer"
EOF
ont-offload check-config
```

Then, with `watch` running:

```sh
mkdir -p "$W/transfer/alice/set1/reads"
head -c 5000000 /dev/urandom > "$W/transfer/alice/set1/reads/a.bin"
echo "notes" > "$W/transfer/alice/set1/notes.txt"
ont-offload status          # the dataset is not listed: no READY yet
touch "$W/transfer/alice/set1/READY"
ont-offload status          # after about half a minute: transfer/alice/set1  complete  3/3 file(s), 5 MB
ont-offload verify "$S/transfer/alice/set1" --source
ont-offload cleanup --dry-run "$W/transfer/alice/set1"   # refused: cleanup is off for this source
```
