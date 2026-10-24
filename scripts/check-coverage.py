#!/usr/bin/env python3
"""Check measured LLVM line coverage without counting generated files as evidence."""

import argparse
import json
from pathlib import Path
import sys


def measure(report):
    groups = {name: {"count": 0, "covered": 0} for name in ["core", "service"]}
    for data in report["data"]:
        for file in data["files"]:
            name = file["filename"].replace("\\", "/")
            group = "core" if "/librepods-core/src/" in name else (
                "service" if any(f"/{crate}/src/" in name for crate in ["librepods-daemon", "librepods-linux", "librepods-cli"]) else None)
            if group is not None:
                lines = file["summary"]["lines"]
                groups[group]["count"] += lines["count"]
                groups[group]["covered"] += lines["covered"]
    for values in groups.values():
        values["percent"] = 100 * values["covered"] / values["count"] if values["count"] else None
    return groups


def production_measure(lcov, spans):
    """Use LLVM's DA records, excluding only parsed test-only syntax spans."""
    groups = {name: {"count": 0, "covered": 0, "test_lines_excluded": 0}
              for name in ["core", "service"]}
    files = {}
    current = None
    for line in lcov.splitlines():
        if line.startswith("SF:"):
            current = line[3:].replace("\\", "/")
        elif line.startswith("DA:") and current:
            number, count, *_ = line[3:].split(",")
            # Feature variants can emit multiple records; merge executed lines.
            rows = files.setdefault(current, {})
            rows[int(number)] = max(rows.get(int(number), 0), int(count))
    for name, lines in files.items():
        group = "core" if "/librepods-core/src/" in name else (
            "service" if any(f"/{crate}/src/" in name for crate in
                ["librepods-daemon", "librepods-linux", "librepods-cli"]) else None)
        if group is None:
            continue
        if name not in spans:
            raise ValueError(f"missing parsed source spans: {name}")
        excluded = spans[name]
        for number, count in lines.items():
            if any(start <= number <= end for start, end in excluded):
                groups[group]["test_lines_excluded"] += 1
            else:
                groups[group]["count"] += 1
                groups[group]["covered"] += count > 0
    for values in groups.values():
        values["percent"] = 100 * values["covered"] / values["count"] if values["count"] else None
    return groups


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("report", type=Path)
    parser.add_argument("--lcov", type=Path, help="LLVM LCOV per-line counters for production gate")
    parser.add_argument("--test-spans", type=Path, help="syn-derived test-only syntax ranges")
    parser.add_argument("--core-min", type=float, default=90)
    parser.add_argument("--service-min", type=float, default=80)
    parser.add_argument("--report-only", action="store_true", help="measure baseline without claiming release readiness")
    args = parser.parse_args()
    if bool(args.lcov) != bool(args.test_spans):
        parser.error("--lcov and --test-spans must be used together")
    if not args.lcov and not args.report_only:
        parser.error("production gate requires --lcov and --test-spans; summary-only is diagnostic")
    groups = (production_measure(args.lcov.read_text(), json.loads(args.test_spans.read_text()))
              if args.lcov else measure(json.loads(args.report.read_text())))
    print(json.dumps(groups, indent=2))
    failures = []
    for name, minimum in [("core", args.core_min), ("service", args.service_min)]:
        if groups[name]["percent"] is None or groups[name]["percent"] < minimum:
            failures.append(f"{name} line coverage is below required {minimum}%")
    if failures:
        print("; ".join(failures), file=sys.stderr)
    return int(bool(failures) and not args.report_only)


if __name__ == "__main__":
    raise SystemExit(main())
