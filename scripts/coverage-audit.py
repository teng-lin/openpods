#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-only
"""Audit changed production lines and critical integration paths without exclusions."""
import argparse
import json
from pathlib import Path
import re
import subprocess

ROOT = Path(__file__).resolve().parents[1]
BASE = "399e7759053d4b0dfe1d37813b0ca9d0bde7af5c"
# Conservative file scopes: include whole orchestration/adapter/client files,
# rather than selecting only the easily covered branches inside them.
CRITICAL = {
    "librepods-core/src/protocol/": "packet validation and confirmed command state machines",
    "librepods-core/src/policy/": "recovery deadlines and reversible desktop ownership",
    "librepods-core/src/device/state.rs": "generation, freshness and device identity reducer",
    "librepods-core/src/device/ble_advert.rs": "untrusted optional advertisement decoding and identity",
    "librepods-core/src/device/battery.rs": "unavailable versus genuine battery measurements",
    "librepods-core/src/transport.rs": "classified transport errors and retry policy",
    "librepods-daemon/src/": "IPC, connection ownership, persistence, keys and desktop coordination",
    "librepods-linux/src/": "native bounded I/O, monitor cancellation and desktop side effects",
    "librepods-cli/src/": "direct ownership, typed requests, uncertainty and truthful status",
}


def changed_lines(diff):
    files = {}
    current = None
    for line in diff.splitlines():
        if line.startswith("+++ b/"):
            current = line[6:]
        elif line.startswith("+++ "):
            current = None
        elif current and (match := re.match(r"@@ -\d+(?:,\d+)? \+(\d+)(?:,(\d+))? @@", line)):
            start, count = int(match[1]), int(match[2] or 1)
            files.setdefault(current, set()).update(range(start, start + count))
    return files


def lcov_lines(text):
    files = {}
    current = None
    for line in text.splitlines():
        if line.startswith("SF:"):
            current = line[3:]
        elif current and line.startswith("DA:"):
            number, count, *_ = line[3:].split(",")
            rows = files.setdefault(current, {})
            rows[int(number)] = max(rows.get(int(number), 0), int(count))
    return files


def total(rows):
    return {"covered": sum(rows), "count": len(rows),
            "percent": 100 * sum(rows) / len(rows) if rows else None}


def audit(lcov, spans, changed, root=ROOT):
    result = {}
    all_changed, critical_changed = [], []
    for name, lines in lcov_lines(lcov).items():
        path = Path(name)
        if not any(f"/{crate}/src/" in name for crate in
                   ["librepods-core", "librepods-daemon", "librepods-linux", "librepods-cli"]):
            continue
        relative = str(path.relative_to(root))
        if name not in spans:
            raise ValueError(f"missing test-only syntax metadata: {name}")
        critical = any(relative == prefix or relative.startswith(prefix) for prefix in CRITICAL)
        production = {line: count for line, count in lines.items()
                      if not any(start <= line <= end for start, end in spans[name])}
        modified = {line: count for line, count in production.items() if line in changed.get(relative, set())}
        values = [count > 0 for count in modified.values()]
        all_changed.extend(values)
        if critical:
            critical_changed.extend(values)
        result[relative] = {"production": total([count > 0 for count in production.values()]),
                            "changed_production": total(values), "critical_scope": critical,
                            "uncovered_changed_lines": [line for line, count in modified.items() if count == 0]}
    return {"changed_production": total(all_changed), "critical_changed_production": total(critical_changed),
            "critical_scope_rationale": CRITICAL, "files": result}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--lcov", type=Path, required=True)
    parser.add_argument("--test-spans", type=Path, required=True)
    parser.add_argument("--base", default=BASE)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--critical-min", type=float, default=90)
    parser.add_argument("--report-only", action="store_true")
    args = parser.parse_args()
    base = subprocess.check_output(["git", "rev-parse", "--verify", "--end-of-options", args.base + "^{commit}"], cwd=ROOT, text=True).strip()
    diff = subprocess.check_output(["git", "diff", "--no-ext-diff", "--no-renames", "--src-prefix=a/", "--dst-prefix=b/", "--unified=0", base, "--"], cwd=ROOT, text=True)
    report = audit(args.lcov.read_text(), json.loads(args.test_spans.read_text()), changed_lines(diff))
    report["base_commit"] = base
    report["head_commit"] = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=ROOT, text=True).strip()
    report["critical_target_percent"] = args.critical_min
    percent = report["critical_changed_production"]["percent"]
    report["critical_target_met"] = percent is not None and percent >= args.critical_min
    args.output.write_text(json.dumps(report, indent=2) + "\n")
    print(json.dumps({name: report[name] for name in ["changed_production", "critical_changed_production", "critical_target_met"]}, indent=2))
    return int(not report["critical_target_met"] and not args.report_only)


if __name__ == "__main__":
    raise SystemExit(main())
