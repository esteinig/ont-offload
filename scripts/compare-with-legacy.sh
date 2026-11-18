#!/usr/bin/env bash
# SPDX-License-Identifier: AGPL-3.0-or-later
#
# Parallel run: check the legacy service's copy of every run that
# ont-offload completed against ont-offload's manifest. Reads only.
#
#   scripts/compare-with-legacy.sh <ont-offload dest_root> <legacy root> [<report file>]
#
# The legacy copy of a run is looked up at the same path relative to the
# legacy root. LEGACY_SED rewrites that relative path when the layouts
# differ, for example LEGACY_SED='s#^\([^/]*\)/[^/]*/#\1/#' drops the
# sample level. BIN names the ont-offload binary (default: on PATH).
#
# Exit status: 0 when every legacy copy matches, 1 when any is missing or
# differs (each run's result is in the report).
set -uo pipefail

new_root=${1:?ont-offload dest_root}
legacy_root=${2:?legacy root}
report=${3:-/dev/stdout}
bin=${BIN:-ont-offload}
sed_expr=${LEGACY_SED:-}

ok=0; bad=0; missing=0
while IFS= read -r -d '' complete; do
    run=$(dirname "$complete")
    rel=${run#"$new_root"/}
    [[ -n "$sed_expr" ]] && rel=$(printf '%s' "$rel" | sed -e "$sed_expr")
    legacy="$legacy_root/$rel"
    if [[ ! -d "$legacy" ]]; then
        printf 'MISSING  %s  (no legacy copy at %s)\n' "$run" "$legacy"
        missing=$((missing + 1))
    elif out=$("$bin" --log-level error verify "$run" --against "$legacy" 2>&1); then
        printf 'OK       %s\n' "$out"
        ok=$((ok + 1))
    else
        printf 'DIFFERS  %s\n%s\n' "$run" "$out"
        bad=$((bad + 1))
    fi
done < <(find "$new_root" -mindepth 2 -name COMPLETE -type f -print0 | sort -z) > "$report"
printf 'runs compared: %d match, %d differ, %d without a legacy copy\n' "$ok" "$bad" "$missing" >&2
[[ $bad -eq 0 && $missing -eq 0 ]]
