# Deploying on a sequencer

This is the order in which ont-offload goes onto a sequencer: install, a service user the share accepts, the configuration, checks, a parallel run beside the legacy service, the site acceptance steps, and go-live. Keep the outputs named under **Evidence** with the release records.

## 1. Before you start

| You need | Why | Who |
| --- | --- | --- |
| The share mounted on the sequencer (NFS or SMB, in `/etc/fstab` or an automount) | ont-offload never mounts anything | site IT |
| A decision on who owns files on the share | the service writes as its own user, not root (step 3) | site IT, storage owner |
| The group that owns the run folders under `source.root` (`stat -c %G /data/<run>`) | the service user must read them, and write them if cleanup will delete | sequencer admin |
| A `notify_command` script that reaches a person (e-mail, Teams, a ticket) | the dashboard is not watched at night | lab |
| A folder on the share for the parallel run, next to the legacy service's | the two services must never write to the same folders | storage owner |

## 2. Install

```sh
sha256sum -c SHA256SUMS --ignore-missing
tar -xzf ont-offload-vX.Y.Z-x86_64-unknown-linux-gnu.tar.gz
cd ont-offload-vX.Y.Z-x86_64-unknown-linux-gnu
sudo install -m 0755 ont-offload packaging/ont-offload-tui /usr/local/bin/
```

The archive holds `packaging/` (unit, sysusers, logrotate, example configuration, dashboard launcher) and `scripts/` (parallel-run comparison, replay soak, cleanup review). The commands below run from this folder.

The release binaries include the [dashboard](tui.md). From source: `cargo install --locked --features tui ...` (see [Installation](installation.md)).

## 3. Service user and permissions

The service runs as the user and group `ont-offload`, never as root. On an NFS export with `root_squash`, root's files arrive on the share as `nobody`; a dedicated user gives the share one known owner.

```sh
sudo install -m 0644 packaging/ont-offload.sysusers /usr/lib/sysusers.d/ont-offload.conf
sudo systemd-sysusers                       # or: useradd --system --user-group ont-offload
sudo usermod -aG <group of the run folders> ont-offload
```

- **Reading runs.** The user needs read access to every run folder. MinKNOW folders are usually group-readable; if not, add an ACL: `sudo setfacl -R -m g:ont-offload:rX -m d:g:ont-offload:rX /data`.
- **Deleting runs (cleanup on only).** Deleting a file needs write access to its folder. Give the group write access on the run folders (`setfacl -R -m g:ont-offload:rwX -m d:g:ont-offload:rwX /data`), or leave cleanup off.
- **Writing to the share.** Agree with the storage owner how the share sees `ont-offload`:
  - NFS: the same uid on the sequencer and the server, or `all_squash,anonuid=<uid>,anongid=<gid>` on the export for this client. The receiving side (analysis accounts, the storage-side `verify` job) must be able to read files owned by that user with `transfer.file_mode` (default `0640`, so: the same group, or a wider mode).
  - SMB: mount with `uid=ont-offload,gid=ont-offload,file_mode=0640,dir_mode=0750` (or the modes the receiving side needs). The server owns the files; `check-config` shows what it reports.
- **Operators.** People who run `ont-offload status` or `ont-offload tui` need read access to the state and log folders, which the unit makes `0750` for the group: `sudo usermod -aG ont-offload <operator>`.

### Running as an existing user (for example `prom`)

The service can run as any user that is not root, such as the standard `prom` account on a PromethION. Nothing in ont-offload depends on the name `ont-offload`. Use the drop-in in `packaging/run-as-existing-user.conf` instead of creating a user:

```sh
sudo install -D -m 0644 packaging/run-as-existing-user.conf /etc/systemd/system/ont-offload.service.d/user.conf
sudoedit /etc/logrotate.d/ont-offload        # "create 0640 prom prom" and "su prom prom"
sudo chgrp prom /etc/ont-offload/config.toml
sudo systemctl daemon-reload
sudo -u prom ont-offload check-config
```

What changes:

- Copies on the share are owned by `prom`'s uid (or what the share maps it to): `check-config` shows it. The share and the receiving side must accept that user, as in the bullets above.
- `prom` must read the run folders (and write them, with cleanup on), as above.
- State and logs are readable by the `prom` group. Operators who log in as `prom` see them without further setup; others need to join that group.
- The trade-off: `prom` is a login account, so anyone logged in as `prom` can change or remove the offload state and journals, and files written in a `prom` session are indistinguishable from the service's on the share. A dedicated user keeps the two apart. Both are supported; the share's owner and audit needs decide.

## 4. Configuration, logs and the service

```sh
sudo install -D -m 0640 -g ont-offload packaging/ont-offload.example.toml /etc/ont-offload/config.toml
sudoedit /etc/ont-offload/config.toml        # dest_root, target_id, source, notify_command; cleanup stays off
echo "<target_id>" | sudo tee <dest_root>/.ont-offload-target
sudo install -m 0644 packaging/ont-offload.logrotate /etc/logrotate.d/ont-offload
sudo install -m 0644 packaging/ont-offload.service /etc/systemd/system/
sudo install -m 0644 packaging/tui.env /etc/ont-offload/tui.env
```

Set `logging.file = "/var/log/ont-offload/ont-offload.log"`. The daemon reopens the file by name within a second after logrotate renames it, so no signal or `copytruncate` is needed; logrotate keeps 12 compressed files, rotated weekly or at 100 MB.

## 5. Checks before the first start

Run the checks as the service user, so they see what the service will see:

```sh
sudo -u ont-offload ont-offload check-config
```

Read the end of its output:

```text
destination guard OK: sentinel matches, nfs4 mounted from nas01:/export/ont at /mnt/nas
share write probe OK as uid 991 gid 991: copies will be owned by uid 991 gid 991, mode 0640
configuration OK (0 warning(s))
```

- `share write probe FAILED`: the user cannot create a file on the share. Fix the export or mount (step 3) before going on.
- `WARNING: files written to the share are owned by uid ...`: the server maps the user to another owner. Make sure the receiving side can read files of that owner.
- `WARNING: the share keeps mode ...`: the share ignores `transfer.file_mode` (common on SMB). Check who can read the copies.
- `WARNING: running as root`: you ran the check with `sudo` alone; use `sudo -u ont-offload`.

Then copy one small finished run by hand and read it **from the receiving side**, as the analysis account would:

```sh
sudo -u ont-offload ont-offload run /data/<small finished run>
# on the analysis host:
cd <dest_root>/<run> && sha256sum -c --quiet manifest.sha256
```

Then `bench` (step 7) and start the service:

```sh
sudo systemctl daemon-reload
sudo systemctl enable --now ont-offload
systemctl status ont-offload
ont-offload status                 # as an operator in the ont-offload group
```

Test `notify_command` once: unmount or block the share for longer than `outage.warn_after` on a quiet day and check that the message arrives.

## 6. Parallel run beside the legacy service

Run ont-offload next to the legacy service for at least two weeks, or longer if that does not cover 20 completed runs, before go-live.

1. Give ont-offload its own folder on the share and its own target: for example `dest_root = "/mnt/nas/ont-offload-trial"` with its own sentinel and `target_id = "<site>-trial"`. The two services never write to the same folder.
2. Keep `[cleanup] delete = "off"`: during the trial the legacy service owns the runs on the sequencer.
3. Share the link: set `rate_limit` so that both services together stay below what `bench` measured.
4. Every day, compare the legacy copy of every run ont-offload completed with ont-offload's manifest:

   ```sh
   scripts/compare-with-legacy.sh /mnt/nas/ont-offload-trial /mnt/nas/ont-legacy trial-$(date +%F).txt
   ```

   It runs `ont-offload verify <run> --against <legacy copy>` for each run (reading only) and exits non-zero when a legacy copy is missing, has a different or missing file, or holds files ont-offload did not copy. If the legacy layout differs, `LEGACY_SED` rewrites the run's relative path (see the script's header).
5. Investigate every difference: a file that changed after ont-offload copied it shows as `run_reopened` in `events.jsonl`; a file the legacy service missed shows as missing there. Record the outcome.

**Pass:** every completed run matches or every difference is explained, no `file_failed` or `run_failed` event is unexplained, and the service ran without manual restarts.

## 7. Site acceptance

These steps need the site's own share; [Testing](../developer-guide/testing.md#site-acceptance) describes them in full.

| Step | Evidence |
| --- | --- |
| `bench` on each sequencer with the network as busy as usual; settings copied into the configuration | `state_dir/bench.json` |
| 24 h replay soak on the staging share (`scripts/replay-soak.sh`) | `<work>/evidence` |
| Parallel run (step 6) | the daily comparison reports |
| `notify_command` received for an outage | the message |
| Dashboard checked on the site's terminal, each theme in use, with an outage injected | a screenshot |
| Cleanup in `dry-run` for two weeks after go-live, reports reviewed before `on` ([Deleting offloaded runs](cleanup.md)) | `scripts/review-cleanup-reports.py` output |

## 8. Go-live

1. Stop the legacy service at a quiet time, with no run finishing in the next hour.
2. Stop ont-offload, point `dest_root` and `target_id` at the production location (with its sentinel), and use a new `state_dir` so that the trial's journals are not reused.
3. For the first week, set `transfer.on_existing = "hash"`: runs the legacy service already copied are checked and adopted file by file instead of failing as conflicts. Then set it back to `"fail"`.
4. Run `sudo -u ont-offload ont-offload check-config`, start the service and watch the first runs with `ont-offload tui`.
5. Switch cleanup to `dry-run` and start the two-week review.

To roll back, stop ont-offload and start the legacy service. ont-offload never deletes or overwrites what the legacy service wrote, so nothing needs undoing on the share.
