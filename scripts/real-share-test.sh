#!/usr/bin/env bash
# SPDX-License-Identifier: AGPL-3.0-or-later
#
# Offload a simulated live run to a real mounted share through an outage,
# then check the copy. Used by the nightly workflow (NFS and Samba).
#
#   scripts/real-share-test.sh <mount point> <fstype>
#
# Environment:
#   DOWN_CMD   command that takes the share away (e.g. stop the server)
#   UP_CMD     command that brings it back
#   OUTAGE_S   outage length in seconds (default 120)
#   BIN        folder with release binaries (default target/release)
#   ALLOWED    allowed_fstypes as a TOML list (default NFS and SMB types)
set -euo pipefail

mnt=${1:?mount point}
fstype=${2:?fstype}
bin=${BIN:-target/release}
outage=${OUTAGE_S:-120}
work=$(mktemp -d)
data="$work/data"
state="$work/state"
mkdir -p "$data" "$state"
target="nightly-$fstype"

echo "$target" > "$mnt/.ont-offload-target"
cat > "$work/config.toml" <<TOML
dest_root = "$mnt"
target_id = "$target"
state_dir = "$state"

[source]
root = "$data"

[bandwidth]
workers = 4
rate_limit = "50MB/s"

[transfer]
settle = "5s"
settle_after_complete = "30s"

[outage]
check_every_max = "20s"
stable_before_resume = "15s"
log_every = "30s"
warn_after = "60s"

[guard]
allowed_fstypes = ${ALLOWED:-["nfs", "nfs4", "cifs", "smb3"]}
TOML

"$bin/ont-offload" --config "$work/config.toml" check-config

# A 2 h run at 60x speed: about two minutes of writing.
"$bin/ont-runsim" run --preset quick-2h --out "$data" --keep-free 1GB -q &
sim=$!
# Wait for the run folder before starting the daemon.
for _ in $(seq 60); do
  if compgen -G "$data/*/*/*/sample_sheet_*.csv" > /dev/null; then break; fi
  sleep 1
done

"$bin/ont-offload" --config "$work/config.toml" --log-file "$work/offload.log" watch --until-idle &
watch=$!

sleep 40
echo "nightly: taking the share away for ${outage}s"
eval "${DOWN_CMD:?DOWN_CMD}"
sleep "$outage"
eval "${UP_CMD:?UP_CMD}"
echo "nightly: share is back"

wait "$sim"
timeout 1200 tail --pid="$watch" -f /dev/null
wait "$watch"

grep -q "destination unavailable" "$work/offload.log" || {
  echo "nightly: the outage was not noticed" >&2
  exit 1
}
for run in "$data"/*/*/*/; do
  rel=${run#"$data"/}
  dest="$mnt/$rel"
  "$bin/ont-runsim" check --out "$data" --dest "$dest"
  (cd "$dest" && sha256sum --quiet -c manifest.sha256)
  "$bin/ont-offload" verify "$dest" --source
done
echo "nightly: $fstype OK"
