# Deleting offloaded runs

ont-offload can delete a run from the sequencer once a verified copy is on the share. Deletion is **off by default**. Turn it on in two steps: first `delete = "dry-run"` for at least two weeks, with the reports reviewed, then `delete = "on"`. Deleting a run cannot be undone, so every gate below must pass, and every deleted file is recorded first (SRS-DEL-001 to 006, hazard HAZ-009).

```toml
[cleanup]
delete = "dry-run"               # "off" (default), "dry-run" or "on"
gate = "storage-verify"          # "storage-verify", "readback" or "both"
delete_after = "7d"              # keep each run at least this long after completion
rehash_source = true             # re-hash the source just before deleting
only_when_free_below = "always"  # or e.g. "2TB": delete oldest first, only while space is short
```

## The gates

A run is deleted only when all four pass, checked in this order:

| Gate | Passes when | Otherwise |
| --- | --- | --- |
| Finished | The journal is complete, `COMPLETE` on the share names the same manifest, and `delete_after` has passed since completion | waiting (too young, or `COMPLETE` withdrawn); refused if `COMPLETE` names another manifest |
| Share present | The guard passes (sentinel, mount, device); `manifest.sha256` on the share is the completed one; every entry is a committed file with the same digest; every destination file has its committed size | the pass stops when the share is unavailable; refused on any difference |
| Content proof | `storage-verify`: `VERIFIED.json` says `ok` for the same manifest digest, file count and bytes, and is not older than the completion. `readback`: every destination file is read back now (within `rate_limit`) and matches. `both`: both | waiting for a receipt (or a newer one); refused on a mismatch |
| Source unchanged | Every source file still has its committed size and modification time, and with `rehash_source` its committed digest | refused |

**Waiting** is normal: the next pass checks again. **Refused** needs a person: the run is kept, a `deletion_refused` alert is raised once (with `notify_command`) and stays in `status` until the problem is gone.

`readback` and `both` need `transfer.verify = "readback"`.

### Storage-side verification

With `gate = "storage-verify"` (the default) the receiving side proves the copy. It runs `ont-offload verify <run>` on each completed run (or anything that writes the same `VERIFIED.json`). This can be a pipeline step on the analysis server, a cron job on the storage host, or a person by hand. For example, an hourly cron job on a host that mounts the share:

```sh
# Verify every completed run on the share that has no receipt yet.
find /mnt/nas/ont -name COMPLETE -printf '%h\n' | while read -r run; do
  [ -e "$run/VERIFIED.json" ] || ont-offload verify "$run"
done
```

`verify` writes `VERIFIED.json` only when every file matches its manifest. A receipt written before the run was last completed (for example before it was reopened) does not count.

## What is deleted

- Only files listed in the run's `manifest.sha256`. Files in sub-folders go first, and the files at the top of the run (where the run markers are) go last.
- Before each file is removed, the guard, its size on the share and its size and time at the source are checked again. Then a line goes to `state_dir/deletions.jsonl`, flushed to disk, with the path, size, time, digest, the destination copy and the gate evidence.
- Folders are removed only when empty, deepest first, and the run folder last. Anything not in the manifest stays: files added after completion, excluded files, symlinks. The run folder stays too when it is not empty, and the folders above the run (experiment, sample) always stay.
- Then `DELETED.json` is written into the destination run folder. It records when, how many files and bytes, the evidence, whether the source folder was removed, and what was kept. `verify` ignores it, so the copy still verifies.

`deletions.jsonl` is never rotated or trimmed: it is the record of what was deleted.

## When it runs

`watch` runs a pass every hour, the first one five minutes after it starts, on its own thread:

- never during an outage, and never on a run it is still offloading or still watching (`watch_completed_for`). A run is deleted at the earliest when it is no longer watched and `delete_after` has passed;
- every pass is appended to `state_dir/cleanup-reports.jsonl` with each run's outcome (`deleted`, `would_delete`, `held`, `waiting`, `refused`, `stopped`) and reason;
- `status` shows the mode, the next pass and the last pass's counts. Events are `run_deleted`, `deletion_dry_run` (one per dry-run pass), `deletion_refused` and `deletion_stopped` (alerts).

A failure while deleting (the share lost, a file that changed, a removal error) stops the pass with a `deletion_stopped` alert. The run is left partly deleted and the next pass finishes it: files already gone are accepted only because `deletions.jsonl` lists them. A run that is being deleted, or was deleted, is never offloaded again. If a run marker appears again at the path of a deleted run, `watch` raises a `run_failed` alert instead of offloading it into the old destination.

### By hand

```sh
ont-offload cleanup --dry-run            # report every run, delete nothing
ont-offload cleanup --dry-run /data/exp/sample/run   # one run
ont-offload cleanup                      # delete (needs delete = "on")
```

`cleanup` without `--dry-run` refuses (exit 2) while deletion is off. Deleting by hand takes the state folder lock, so it refuses while `watch` runs (`watch` already deletes every hour). A dry run by hand works beside `watch`, but it is not added to `cleanup-reports.jsonl`. `cleanup` exits 3 when a run was refused or the pass stopped, and 2 when the share is unavailable.

## Deleting only when space is short

With `only_when_free_below = "2TB"`, runs that pass every gate are deleted oldest first, and only while the sequencer's free space (plus what the pass has freed) is below 2 TB. The others are reported as `held`. A run that is already partly deleted is always finished.

## Reviewing the dry-run period

Each line of `cleanup-reports.jsonl` is one pass. To list every run that a dry run would have deleted, with its evidence:

```sh
jq -r 'select(.mode=="dry-run") | .runs[] | select(.outcome=="would_delete")
       | [.source, .evidence.verified_at, .evidence.manifest_sha256] | @tsv' \
   /var/lib/ont-offload/cleanup-reports.jsonl | sort -u
```

Or use `scripts/review-cleanup-reports.py <state_dir> --check`. It lists each run that qualified, with when it first did and its evidence, flags any run refused by a later pass, and re-hashes each destination against its manifest without writing to the share. It exits 1 on any problem; keep its output with the release evidence.

Check each run against the share (the copy is complete and verified, and the analysis has what it needs) before you set `delete = "on"`. The release gate for v1.0 needs two weeks of these reports reviewed with no wrong "would delete".
