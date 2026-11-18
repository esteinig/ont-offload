#!/usr/bin/env bash
# SPDX-License-Identifier: AGPL-3.0-or-later
#
# Set up (or remove) a local test bench for ont-offload on a laptop, with
# a tmpfs folder standing in for the NFS share. See
# docs/src/user-guide/local-testing.md.
#
#   scripts/local-test.sh setup      # create share, work folder, config, sentinel
#   scripts/local-test.sh env        # print the variables to eval in a new terminal
#   scripts/local-test.sh teardown   # remove both folders
#
# Environment:
#   SHARE  stand-in share (default /dev/shm/ont-share, must be on tmpfs)
#   WORK   work folder with source data, state and logs (default ~/ont-local)
set -euo pipefail

share=${SHARE:-/dev/shm/ont-share}
work=${WORK:-$HOME/ont-local}
cmd=${1:-}

print_env() {
  echo "export S=\"$share\" W=\"$work\" CFG=\"$work/config.toml\""
  echo "export ONT_OFFLOAD_CONFIG=\"$work/config.toml\""
}

case "$cmd" in
  setup)
    if [ -e "$work/config.toml" ]; then
      echo "local-test: $work already set up (run teardown first)" >&2
      exit 1
    fi
    mkdir -p "$share" "$work/state" "$work/data"
    fstype=$(stat -f -c %T "$share")
    if [ "$fstype" != "tmpfs" ]; then
      echo "local-test: $share is on $fstype, not tmpfs; set SHARE to a tmpfs folder" >&2
      exit 1
    fi
    echo "laptop-test" > "$share/.ont-offload-target"
    echo "made by scripts/local-test.sh; teardown removes this folder" > "$work/.local-test-bench"
    cat > "$work/notify.sh" <<'SH'
#!/bin/sh
# Appends every event that needs a person to notify.log next to this script.
printf '%s %s %s\n' "$ONT_OFFLOAD_LEVEL" "$ONT_OFFLOAD_EVENT" "$ONT_OFFLOAD_MESSAGE" \
  >> "$(dirname "$0")/notify.log"
SH
    chmod 0755 "$work/notify.sh"
    cat > "$work/config.toml" <<TOML
dest_root = "$share"
target_id = "laptop-test"
state_dir = "$work/state"

[source]
# Stands in for /data on the sequencer; ont-runsim writes runs here.
root = "$work/data"

[bandwidth]
workers = 2
rate_limit = "200MB/s"

[transfer]
min_free = "100MB"
settle = "5s"
settle_after_complete = "20s"

[outage]
source_free_alert = "0B"
notify_command = "$work/notify.sh"
check_every_max = "20s"
stable_before_resume = "15s"
log_every = "30s"
warn_after = "60s"

[guard]
allowed_fstypes = ["tmpfs"]

[logging]
file = "$work/ont-offload.log"

[cleanup]
delete = "dry-run"
delete_after = "0s"
TOML
    echo "local-test: share $share, work folder $work"
    echo "local-test: in each terminal run: eval \"\$(scripts/local-test.sh env)\""
    ;;
  env)
    print_env
    ;;
  teardown)
    # Only remove folders this script made.
    if [ -e "$work" ] && [ ! -f "$work/.local-test-bench" ]; then
      echo "local-test: $work was not made by this script; not removing it" >&2
      exit 1
    fi
    if [ -e "$share" ] && [ "$(cat "$share/.ont-offload-target" 2>/dev/null)" != "laptop-test" ]; then
      echo "local-test: $share has no laptop-test sentinel; not removing it" >&2
      exit 1
    fi
    rm -rf -- "$share" "$work"
    echo "local-test: removed $share and $work"
    ;;
  *)
    echo "usage: scripts/local-test.sh setup|env|teardown" >&2
    exit 2
    ;;
esac
