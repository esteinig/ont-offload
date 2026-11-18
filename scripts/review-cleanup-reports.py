#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-or-later
"""v0.5 qualification: review the dry-run cleanup reports.

Reads state_dir/cleanup-reports.jsonl and lists every run that a pass said
it would delete (or deleted), with when it first qualified, its evidence and
any later pass that refused it. With --check, each such run's destination is
re-hashed against its manifest.sha256 here (read-only: nothing is written to
the share), so a wrong "would delete" shows up as a failed check.

    scripts/review-cleanup-reports.py /var/lib/ont-offload [--check] [--since 2026-11-01]

Exit status: 0 when every listed run checks out (or --check was not given),
1 when a run was refused after it qualified or a destination check failed,
2 for usage errors. Keep the output with the release evidence.
"""

import argparse
import hashlib
import json
import sys
from pathlib import Path

QUALIFIED = ("would_delete", "deleted")


def manifest_entries(text):
    for line in text.splitlines():
        if not line:
            continue
        escaped = line.startswith("\\")
        if escaped:
            line = line[1:]
        digest, name = line.split("  ", 1)
        if escaped:
            name = name.replace("\\n", "\n").replace("\\r", "\r").replace("\\\\", "\\")
        yield digest.lower(), name


def check_destination(dest):
    """Re-hash a destination run against its manifest; return problems."""
    dest = Path(dest)
    problems = []
    try:
        text = (dest / "manifest.sha256").read_text()
    except OSError as e:
        return [f"cannot read manifest.sha256: {e}"]
    for digest, name in manifest_entries(text):
        path = dest / name
        h = hashlib.sha256()
        try:
            with open(path, "rb") as f:
                for chunk in iter(lambda: f.read(8 << 20), b""):
                    h.update(chunk)
        except OSError as e:
            problems.append(f"{name}: {e}")
            continue
        if h.hexdigest() != digest:
            problems.append(f"{name}: differs from manifest")
    for marker in ("COMPLETE", "VERIFIED.json"):
        if not (dest / marker).exists():
            problems.append(f"{marker} missing")
    return problems


def main():
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("state_dir")
    ap.add_argument("--check", action="store_true", help="re-hash each qualified destination")
    ap.add_argument("--since", default="", help="only passes started at or after this RFC 3339 prefix")
    args = ap.parse_args()

    path = Path(args.state_dir) / "cleanup-reports.jsonl"
    try:
        lines = path.read_text().splitlines()
    except OSError as e:
        print(f"cannot read {path}: {e}", file=sys.stderr)
        return 2

    runs = {}
    passes = 0
    for line in lines:
        try:
            rep = json.loads(line)
        except json.JSONDecodeError:
            continue
        if rep.get("started_at", "") < args.since:
            continue
        passes += 1
        for r in rep.get("runs", []):
            e = runs.setdefault(
                r["run_id"],
                {"source": r["source"], "destination": r["destination"], "first": None,
                 "passes": 0, "refused_after": [], "deleted": False, "evidence": None},
            )
            if r["outcome"] in QUALIFIED:
                e["passes"] += 1
                e["first"] = e["first"] or rep["started_at"]
                e["evidence"] = r.get("evidence")
                e["deleted"] |= r["outcome"] == "deleted"
            elif r["outcome"] == "refused" and e["first"]:
                e["refused_after"].append(f'{rep["started_at"]}: {r["reason"]}')

    bad = 0
    qualified = [e for e in runs.values() if e["first"]]
    print(f"{passes} pass(es), {len(runs)} run(s) seen, {len(qualified)} qualified for deletion")
    for e in sorted(qualified, key=lambda e: e["first"]):
        ev = e["evidence"] or {}
        status = "deleted" if e["deleted"] else "would delete"
        print(f'\n{e["source"]}\n  -> {e["destination"]}')
        print(f'  {status}; first {e["first"]}; {e["passes"]} pass(es); '
              f'verified_at {ev.get("verified_at")}; readback {ev.get("readback")}; '
              f'source re-hashed {ev.get("source_rehashed")}')
        for r in e["refused_after"]:
            bad += 1
            print(f"  REFUSED LATER {r}")
        if args.check:
            problems = check_destination(e["destination"])
            if problems:
                bad += 1
                for p in problems[:20]:
                    print(f"  CHECK FAILED {p}")
            else:
                print("  destination re-hashed: OK")
    print(f"\n{'OK' if bad == 0 else f'{bad} PROBLEM(S)'}")
    return 0 if bad == 0 else 1


if __name__ == "__main__":
    sys.exit(main())
