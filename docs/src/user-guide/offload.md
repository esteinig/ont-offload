# Offloading a run

`run` copies one completed run in the foreground. To copy runs while they are being written, use the daemon, `watch` (see [Running the daemon](watch.md)); it uses the same copy pipeline, journal and outputs.

## Before the first run

1. Mount the share and create the sentinel file with the configured `target_id`:

   ```sh
   echo "lab-nas-01" | sudo tee /mnt/nas/ont/.ont-offload-target
   ```

2. Set `dest_root`, `target_id` and, if needed, `dest_starts_at` in the configuration, then run `ont-offload check-config`. It prints the effective settings, every run it finds with its marker, whether it is complete, and the folder it will be copied to, and finally the destination guard result.

## Copying

```sh
ont-offload run /data/exp1/s1/20261113_1200_1A_PAW00001_abcd1234
```

The run needs its complete marker (`final_summary_*.txt` by default). For a run that finished without one, add `--completed`.

Before the first file, ont-offload writes `.ont-offload-claim.json` (target, host, source run, run id) into the destination run folder. A folder already claimed by a different source run, from any host or configuration, is refused and nothing is written (see [below](#where-a-run-goes-dest_starts_at)).

For every file, ont-offload:

1. streams it from the source into a hidden temp file (`.<name>.ontoff-<nonce>.partial`) next to its final name, hashing every byte with SHA-256 and re-reading the share's sentinel after every chunk;
2. flushes the data, checks the source did not change while it was read, and checks the size on the share (with `verify = "readback"` it also reads the copy back and compares digests);
3. re-runs the full destination guard and renames the temp file to the final name;
4. records the file in its journal under `state_dir`.

When every file is in, it checks every size once more and writes, in this order, `manifest.sha256`, `run.json` and `COMPLETE`. A run folder on the share without `COMPLETE` is not finished. The run's journal is then compacted to one line per file.

If a run was completed before and its files changed since, `COMPLETE` and `VERIFIED.json` are removed from the share first, then the run is offloaded again (only new and changed files are copied) with a new manifest.

`manifest.sha256` can be checked with standard tools on the storage side:

```sh
cd /mnt/nas/ont/exp1/s1/20261113_1200_1A_PAW00001_abcd1234
sha256sum -c manifest.sha256
```

`run.json` records the source and destination, `dest_starts_at`, host, markers, start and end time, files, bytes, the manifest digest, and any files that were excluded or skipped (symbolic links, special files, nested runs, names with characters the share cannot hold).

## Where a run goes: `dest_starts_at`

MinKNOW writes runs as `<experiment>/<sample>/<run>`. `dest_starts_at` chooses the level that becomes the top folder under `dest_root`:

| `dest_starts_at` | `/data/exp1/s1/RUN` is copied to |
| --- | --- |
| `experiment` (default) | `dest_root/exp1/s1/RUN` |
| `sample` | `dest_root/s1/RUN` |
| `run` | `dest_root/RUN` |

With `sample` or `run`, two runs can map to the same folder (two experiments with a sample of the same name, for example). Each destination run folder is claimed by one source run, both in `state_dir` and, since v0.4, by the claim file on the share itself; a second run that maps to a claimed folder fails before anything is written, even from another sequencer or another configuration. Changing `dest_starts_at` for a run that was already started is refused.

## Interruptions and failures

- **Stopped or killed:** run the same command again. Files already committed with an unchanged source are not copied again, and temp files left by the earlier attempt are removed.
- **Share unmounted, sentinel missing, wrong filesystem, same device as the source, or `dest_root` pointing elsewhere:** nothing is written and the command exits with 2. If this happens during a copy, writes stop within one chunk.
- **Share fails mid-run (EIO, ESTALE, timeouts):** exits with 75; committed files are kept and the next attempt resumes.
- **A file on the share that ont-offload did not write:** never overwritten; the file fails (exit 3). With `on_existing = "hash"`, an identical file is adopted instead.
- **Source changed during the copy, short write, read-back mismatch:** retried up to `max_attempts`.

## Verifying on the storage side

```sh
ont-offload verify /mnt/nas/ont/exp1/s1/RUN [--source]
```

`verify` re-hashes every file against `manifest.sha256`, reports missing, changed and extra files, and checks that `COMPLETE` is present. With `--source` it also re-hashes the source run named in `run.json`. Only when everything matches does it write `VERIFIED.json` (result, manifest digest, files, bytes, host, time); otherwise it exits with 4. `VERIFIED.json` is the receipt that gated source deletion requires with `gate = "storage-verify"` (see [Deleting offloaded runs](cleanup.md)).
