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


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("report", type=Path)
    parser.add_argument("--core-min", type=float, default=90)
    parser.add_argument("--service-min", type=float, default=80)
    parser.add_argument("--report-only", action="store_true", help="measure baseline without claiming release readiness")
    args = parser.parse_args()
    groups = measure(json.loads(args.report.read_text()))
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
