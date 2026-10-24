#!/usr/bin/env python3
"""Capture, switch or restore service activity without altering pairing/config."""

import argparse
import json
import os
from pathlib import Path
import re
import subprocess
import sys

NEW_UNIT = "librepods-rs.service"


def systemctl(*args):
    return subprocess.run(["systemctl", "--user", *args], capture_output=True,
                          text=True, timeout=8, check=False)


def validate_old_unit(unit):
    if not re.fullmatch(r"[A-Za-z0-9_.@-]+\.service", unit) or unit == NEW_UNIT:
        raise ValueError("old unit must be a distinct user .service name")


def inspect(unit, run=systemctl):
    active = run("is-active", unit)
    enabled = run("is-enabled", unit)
    return {"unit": unit, "active": active.returncode == 0,
            "enabled": enabled.stdout.strip() or "unknown"}


def capture(path, old_unit, run=systemctl):
    validate_old_unit(old_unit)
    snapshot = {"version": 1, "uid": os.getuid(), "old": inspect(old_unit, run),
                "new": inspect(NEW_UNIT, run)}
    path.parent.mkdir(parents=True, exist_ok=True)
    # Never replace the only rollback record on a second invocation.
    fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    with os.fdopen(fd, "w") as output:
        json.dump(snapshot, output, indent=2)
        output.write("\n")
        output.flush()
        os.fsync(output.fileno())


def load(path):
    if path.is_symlink() or path.stat().st_uid != os.getuid():
        raise ValueError("snapshot is not an owned regular file")
    snapshot = json.loads(path.read_text())
    if snapshot.get("version") != 1 or snapshot.get("uid") != os.getuid():
        raise ValueError("unknown snapshot version or owner")
    validate_old_unit(snapshot["old"]["unit"])
    if snapshot["new"]["unit"] != NEW_UNIT:
        raise ValueError("snapshot names an unexpected replacement service")
    for key in ["old", "new"]:
        if not isinstance(snapshot[key]["active"], bool):
            raise ValueError("invalid snapshot activity state")
    return snapshot


def checked(run, *args):
    result = run(*args)
    if result.returncode != 0:
        raise ValueError(f"systemctl {' '.join(args)} failed: {result.stderr.strip()}")


def restore_failed_switch(snapshot, run):
    """Restore only when the replacement is proved stopped; report every failure."""
    errors = []
    replacement_stopped = False
    try:
        checked(run, "stop", NEW_UNIT)
        replacement_stopped = True
    except (ValueError, OSError, subprocess.TimeoutExpired) as error:
        errors.append(f"replacement cleanup: {error}")
        try:
            state = run("is-active", NEW_UNIT)
            replacement_stopped = (state.returncode == 3 and
                                   state.stdout.strip() in {"inactive", "failed"})
            if not replacement_stopped:
                errors.append("replacement inactivity could not be verified")
        except (ValueError, OSError, subprocess.TimeoutExpired) as error:
            errors.append(f"replacement activity check: {error}")
    if snapshot["old"]["active"]:
        if replacement_stopped:
            try:
                checked(run, "start", snapshot["old"]["unit"])
            except (ValueError, OSError, subprocess.TimeoutExpired) as error:
                errors.append(f"previous service restoration: {error}")
        else:
            errors.append("previous service was not restored because the replacement may still own the connection")
    return errors


def switch(snapshot, run=systemctl):
    if snapshot["new"]["active"]:
        raise ValueError("replacement was already active; resolve connection ownership first")
    try:
        checked(run, "stop", snapshot["old"]["unit"])
        checked(run, "daemon-reload")
        checked(run, "start", NEW_UNIT)
    except (ValueError, OSError, subprocess.TimeoutExpired) as error:
        failures = restore_failed_switch(snapshot, run)
        detail = f"; rollback: {'; '.join(failures)}" if failures else "; previous service activity restored"
        raise ValueError(f"switch failed: {error}{detail}") from error


def rollback(snapshot, run=systemctl):
    checked(run, "stop", NEW_UNIT)
    if snapshot["old"]["active"]:
        checked(run, "start", snapshot["old"]["unit"])
    if snapshot["new"]["active"]:
        checked(run, "stop", snapshot["old"]["unit"])
        checked(run, "start", NEW_UNIT)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("action", choices=["capture", "switch", "rollback"])
    parser.add_argument("--snapshot", type=Path, required=True)
    parser.add_argument("--old-unit", default="librepods.service")
    args = parser.parse_args()
    try:
        if args.action == "capture":
            capture(args.snapshot, args.old_unit)
            print("Saved service activity and enabled-state evidence; nothing was changed.")
        else:
            snapshot = load(args.snapshot)
            (switch if args.action == "switch" else rollback)(snapshot)
            print("Service activity updated. Enablement, panel configuration, audio routes and pairings were not changed.")
    except (ValueError, KeyError, OSError, json.JSONDecodeError, subprocess.TimeoutExpired) as error:
        print(f"Migration error: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
