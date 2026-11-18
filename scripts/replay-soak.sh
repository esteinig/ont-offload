#!/usr/bin/env bash
# SPDX-License-Identifier: AGPL-3.0-or-later
#
# v0.4 acceptance: a 24 h replay soak on a staging share. ont-runsim
# replays the bytes of a real run as a new live run over HOURS, `watch`
# offloads it, and the copy is checked file by file. Everything needed as
# evidence (state.json, events.jsonl, bench.json if present, the log and
# the checks) is collected in <work>/evidence.
#
#   scripts/replay-soak.sh <mounted dest_root> <target_id> <real run folder> <work folder>
#
# The share must already carry its sentinel (<dest_root>/.ont-offload-target
# with <target_id>). The work folder holds the simulated source runs and
# must be on the sequencer's local disk.
#
# Environment:
#   HOURS       simulated hours (default 24)
#   SPEED       simulated seconds per second (default 1: real time)
#   SIZE_SCALE  multiply every file size (default 1)
#   BIN         folder with release binaries (default target/release)
#   ALLOWED     allowed_fstypes as a TOML list (default NFS and SMB types)
#   EXTRA_TOML  extra configuration appended (e.g. the bench suggestions)
#   CHECK_EVERY seconds between status checks (default 600)
#   KEEP_FREE   ont-runsim stops before the local disk has less free (default 500GB)
set -euo pipefail

dest=${1:?dest_root}
target=${2:?target_id}
replay=${3:?real run folder to replay}
work=${4:?work folder}
bin=${BIN:-target/release}
hours=${HOURS:-24}
speed=${SPEED:-1}
scale=${SIZE_SCALE:-1}
data="$work/data"
state="$work/state"
evidence="$work/evidence"
mkdir -p "$data" "$state" "$evidence"

cat > "$work/config.toml" <<TOML
dest_root = "$dest"
target_id = "$target"
state_dir = "$state"

[source]
root = "$data"

[outage]
notify_command = ""

[guard]
allowed_fstypes = ${ALLOWED:-["nfs", "nfs4", "cifs", "smb3"]}
${EXTRA_TOML:-}
TOML

cat > "$work/scenario.toml" <<TOML
seed = 2404
hours = $hours
speed = $speed
content = "replay"

[fake]
size_scale = $scale

[replay]
run = "$replay"

[faults]
rewrite_closed = 2
late_files = 1

[[runs]]
experiment = "soak"
sample = "replay"
TOML

"$bin/ont-offload" --config "$work/config.toml" check-config | tee "$evidence/check-config.txt"

"$bin/ont-runsim" run --scenario "$work/scenario.toml" --out "$data" --keep-free "${KEEP_FREE:-500GB}" -q &
sim=$!
for _ in $(seq 120); do
  if compgen -G "$data/*/*/*/sample_sheet_*.csv" > /dev/null; then break; fi
  sleep 1
done

"$bin/ont-offload" --config "$work/config.toml" --log-file "$evidence/offload.log" \
  --log-format json watch --until-idle &
watch=$!

# Every CHECK_EVERY seconds: the state must be fresh and must parse.
while kill -0 "$watch" 2>/dev/null; do
  sleep "${CHECK_EVERY:-600}"
  if kill -0 "$watch" 2>/dev/null; then
    "$bin/ont-offload" --config "$work/config.toml" status > "$evidence/status-latest.txt" || true
    if grep -q STALE "$evidence/status-latest.txt"; then
      echo "soak: state.json is stale" >&2
      cp "$evidence/status-latest.txt" "$evidence/status-stale-$(date -u +%Y%m%dT%H%M%SZ).txt"
    fi
  fi
done

wait "$sim"
wait "$watch"

cp "$state/state.json" "$state"/events.jsonl* "$evidence/"
[ -f "$state/bench.json" ] && cp "$state/bench.json" "$evidence/"
if grep -qE '"kind":"(file_failed|run_failed)"' "$state"/events.jsonl*; then
  echo "soak: failed files or runs; see events.jsonl" >&2
  exit 1
fi
for run in "$data"/*/*/*/; do
  rel=${run#"$data"/}
  out="$dest/$rel"
  "$bin/ont-runsim" check --out "$data" --dest "$out" | tee -a "$evidence/runsim-check.txt"
  (cd "$out" && sha256sum --quiet -c manifest.sha256) | tee -a "$evidence/sha256sum.txt"
  "$bin/ont-offload" verify "$out" --source | tee -a "$evidence/verify.txt"
done
echo "soak: ${hours} h replay offloaded with no differences; evidence in $evidence"
