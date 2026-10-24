#!/usr/bin/env python3
"""Install/remove only files owned by this user-prefix LibrePods installation."""

import argparse
import hashlib
import json
import os
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
    if not path.exists():
        return {"version": 1, "files": {}}
    result = json.loads(path.read_text())
    if result.get("version") != 1 or not isinstance(result.get("files"), dict):
        raise ValueError("unknown installation manifest version/shape")
    return result


def owned_destination(path, manifest):
    if path.is_symlink():
        raise ValueError(f"refusing symlink destination: {path}")
    if path.exists():
        recorded = manifest["files"].get(str(path))
        if not path.is_file() or not recorded or digest(path) != recorded:
            raise ValueError(f"refusing to overwrite an unowned or modified file: {path}")


def install(prefix, binary, unit_dir):
    prefix = prefix.expanduser().absolute()
    unit_dir = unit_dir.expanduser().absolute()
    if not binary.is_file():
        raise ValueError(f"binary is missing: {binary}; build --release first")
    manifest = read_manifest(prefix)
    destination = prefix / "bin/librepods-cli"
    unit_path = unit_dir / UNIT
    owned_destination(destination, manifest)
    owned_destination(unit_path, manifest)
    # systemd performs percent expansion even inside double-quoted arguments.
    escaped = str(destination).replace("%", "%%").replace("\\", "\\\\").replace('"', '\\"')
    unit = (ROOT / "packaging/systemd/librepods-rs.service.in").read_text()
    files = [(destination, binary.read_bytes(), 0o755),
             (unit_path, unit.replace("@BINARY@", escaped).encode(), 0o644)]
    for path, data, mode in files:
        atomic_write(path, data, mode)
        manifest["files"][str(path)] = digest(path)
    atomic_write(manifest_path(prefix), (json.dumps(manifest, indent=2) + "\n").encode(), 0o600)
    return [path for path, _, _ in files]


def uninstall(prefix):
    prefix = prefix.expanduser().absolute()
    manifest = read_manifest(prefix)
    preserved = []
    for name, expected in list(manifest["files"].items()):
        path = Path(name)
        # A manifest is not permission to delete arbitrary filesystem paths.
        valid_binary = path == prefix / "bin/librepods-cli"
        valid_unit = path.name == UNIT and path.parent.name == "user" and path.parent.parent.name == "systemd"
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
    parser.add_argument("--binary", type=Path, default=ROOT / "target/release/librepods-cli")
    parser.add_argument("--unit-dir", type=Path)
    args = parser.parse_args()
    unit_dir = args.unit_dir or args.prefix / "share/systemd/user"
    try:
        if args.action == "install":
            for path in install(args.prefix, args.binary, unit_dir):
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
