#!/usr/bin/env python3
"""Install/remove only files owned by this user-prefix LibrePods installation."""

import argparse
import hashlib
import json
import os
import re
import stat
from pathlib import Path
import sys
import tempfile

ROOT = Path(__file__).resolve().parents[1]
UNIT = "librepods-rs.service"


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def atomic_write(path, content, mode):
    path.parent.mkdir(parents=True, exist_ok=True)
    fd, temporary = tempfile.mkstemp(prefix=".librepods-install-", dir=path.parent)
    try:
        with os.fdopen(fd, "wb") as output:
            output.write(content)
            output.flush()
            os.fsync(output.fileno())
        os.chmod(temporary, mode)
        os.replace(temporary, path)
    finally:
        if os.path.exists(temporary):
            os.unlink(temporary)


def manifest_path(prefix):
    return prefix / "share/librepods-rs/install.json"


def read_manifest(prefix):
    path = manifest_path(prefix)
    if path.is_symlink():
        raise ValueError("installation manifest is a symlink")
    try:
        descriptor = os.open(path, os.O_RDONLY | os.O_NONBLOCK | os.O_NOFOLLOW | os.O_CLOEXEC)
    except FileNotFoundError:
        return {"version": 1, "files": {}, "unit_dir": None}
    with os.fdopen(descriptor, "rb") as source:
        metadata = os.fstat(source.fileno())
        if not stat.S_ISREG(metadata.st_mode) or metadata.st_uid != os.getuid() or metadata.st_mode & 0o077:
            raise ValueError("installation manifest must be a private owned regular file")
        content = source.read(64 * 1024 + 1)
    if len(content) > 64 * 1024:
        raise ValueError("installation manifest exceeds 64KiB")
    result = json.loads(content)
    if not isinstance(result, dict) or result.get("version") != 1 or not isinstance(result.get("files"), dict):
        raise ValueError("unknown installation manifest version/shape")
    if any(not isinstance(name, str) or not isinstance(value, str) or not re.fullmatch(r"[0-9a-f]{64}", value)
           for name, value in result["files"].items()):
        raise ValueError("invalid installation manifest file records")
    if result.get("unit_dir") is not None and (not isinstance(result["unit_dir"], str) or not Path(result["unit_dir"]).is_absolute()):
        raise ValueError("invalid installation manifest unit directory")
    return result


def owned_destination(path, manifest):
    if path.is_symlink():
        raise ValueError(f"refusing symlink destination: {path}")
    if path.exists():
        recorded = manifest["files"].get(str(path))
        if not path.is_file() or not recorded or digest(path) != recorded:
            raise ValueError(f"refusing to overwrite an unowned or modified file: {path}")


def unit_escape(value):
    value = str(value)
    if any(ord(character) < 32 for character in value):
        raise ValueError("service path contains control characters")
    return value.replace("%", "%%").replace("\\", "\\\\").replace('"', '\\"')


def prepare_legacy_state(state_home):
    state_home = state_home.expanduser()
    if not state_home.is_absolute():
        raise ValueError("legacy state home must be absolute")
    unit_escape(state_home)
    native, legacy = state_home / "librepods-rs", state_home / "librepods"
    for directory in [native, legacy]:
        if os.path.lexists(directory):
            metadata = directory.lstat()
            if not stat.S_ISDIR(metadata.st_mode) or metadata.st_uid != os.getuid() or metadata.st_mode & 0o077:
                raise ValueError("legacy opt-in requires private owned real state directories")
    if os.path.lexists(legacy / "status.json"):
        raise ValueError("legacy status already exists; review the previous owner's file before opting in")
    for directory in [native, legacy]:
        if not os.path.lexists(directory):
            directory.mkdir(parents=True, mode=0o700)
    return state_home, native, legacy


def install(prefix, binary, unit_dir, legacy_state_home=None):
    prefix = prefix.expanduser().absolute()
    unit_dir = unit_dir.expanduser().absolute()
    if not binary.is_file():
        raise ValueError(f"binary is missing: {binary}; build --release first")
    manifest = read_manifest(prefix)
    destination = prefix / "bin/librepods-cli"
    unit_path = unit_dir / UNIT
    if any(ord(character) < 32 for character in str(destination)):
        raise ValueError("binary destination contains control characters")
    canonical = prefix / "bin/openpods-cli"
    daemon = prefix / "bin/openpodsd"
    for path in [destination, canonical, daemon, unit_path]:
        owned_destination(path, manifest)
    # systemd performs percent expansion even inside double-quoted arguments.
    escaped = unit_escape(destination)
    unit = (ROOT / "packaging/systemd/librepods-rs.service.in").read_text()
    if legacy_state_home is not None:
        state_home, native, legacy = prepare_legacy_state(legacy_state_home)
        extra = (f'Environment="XDG_STATE_HOME={unit_escape(state_home)}"\n'
                 f'ReadWritePaths="{unit_escape(native)}" "{unit_escape(legacy)}"')
        unit = unit.replace("ReadWritePaths=%t/librepods-rs", "ReadWritePaths=%t/librepods-rs\n" + extra)
    daemon_source = binary.with_name("openpodsd")
    daemon_bytes = daemon_source.read_bytes() if daemon_source.is_file() else b'#!/bin/sh\nexec "$(dirname -- "$0")/openpods-cli" serve "$@"\n'
    files = [(destination, binary.read_bytes(), 0o755),
             (canonical, binary.read_bytes(), 0o755),
             (daemon, daemon_bytes, 0o755),
             (unit_path, unit.replace("@BINARY@", escaped).encode(), 0o644)]
    backups = {path: (path.read_bytes(), path.stat().st_mode & 0o777) if path.exists() else None
               for path, _, _ in files}
    changed = []
    try:
        for path, data, mode in files:
            atomic_write(path, data, mode)
            changed.append(path)
            manifest["files"][str(path)] = digest(path)
        manifest["unit_dir"] = str(unit_dir)
        atomic_write(manifest_path(prefix), (json.dumps(manifest, indent=2) + "\n").encode(), 0o600)
    except OSError:
        for path in reversed(changed):
            if backups[path] is None:
                path.unlink(missing_ok=True)
            else:
                data, mode = backups[path]
                atomic_write(path, data, mode)
        raise
    return [path for path, _, _ in files]


def uninstall(prefix):
    prefix = prefix.expanduser().absolute()
    manifest = read_manifest(prefix)
    preserved = []
    for name, expected in list(manifest["files"].items()):
        path = Path(name)
        # A manifest is not permission to delete arbitrary filesystem paths.
        valid_binary = path in [prefix / "bin/librepods-cli", prefix / "bin/openpods-cli", prefix / "bin/openpodsd"]
        valid_unit = manifest.get("unit_dir") is not None and path == Path(manifest["unit_dir"]) / UNIT
        if not (valid_binary or valid_unit) or path.is_symlink():
            preserved.append(name)
            continue
        if path.exists() and (not path.is_file() or digest(path) != expected):
            preserved.append(name)
            continue
        path.unlink(missing_ok=True)
        del manifest["files"][name]
    if preserved:
        atomic_write(manifest_path(prefix), (json.dumps(manifest, indent=2) + "\n").encode(), 0o600)
        raise ValueError("preserved modified or unsafe files: " + ", ".join(preserved))
    manifest_path(prefix).unlink(missing_ok=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("action", choices=["install", "uninstall"])
    parser.add_argument("--prefix", type=Path, default=Path.home() / ".local")
    parser.add_argument("--binary", type=Path, default=ROOT / "target/release/openpods-cli")
    parser.add_argument("--unit-dir", type=Path)
    parser.add_argument("--legacy", action="store_true", help="opt in to a narrowly writable legacy status directory")
    parser.add_argument("--state-home", type=Path, help="state home used by --legacy; defaults to XDG_STATE_HOME")
    args = parser.parse_args()
    unit_dir = args.unit_dir or args.prefix / "share/systemd/user"
    if args.state_home and not args.legacy:
        parser.error("--state-home requires --legacy")
    state_home = args.state_home or Path(os.environ.get("XDG_STATE_HOME", Path.home() / ".local/state"))
    try:
        if args.action == "install":
            for path in install(args.prefix, args.binary, unit_dir, state_home if args.legacy else None):
                print(f"Installed {path}")
            print("No service was enabled or started. Run systemctl --user daemon-reload, then review docs/deployment.md.")
        else:
            uninstall(args.prefix)
            print("Removed owned program/unit files. User config, state, pairings and other services were preserved.")
    except (ValueError, OSError, json.JSONDecodeError) as error:
        print(f"Installation error: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
