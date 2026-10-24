"""Migration failures preserve service availability without competing owners."""

import importlib.util
from pathlib import Path
import subprocess
import os
import sys
import json
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[2]
spec = importlib.util.spec_from_file_location("migration_failures", ROOT / "scripts/migrate.py")
migration = importlib.util.module_from_spec(spec)
spec.loader.exec_module(migration)


class Services:
    def __init__(self, *, reload_failure=False, failed_stop_active=None, probe_failure=False,
                 old_restore_failure=False):
        self.active = {"previous.service": True, migration.NEW_UNIT: False}
        self.calls = []
        self.reload_failure = reload_failure
        self.failed_stop_active = failed_stop_active
        self.probe_failure = probe_failure
        self.old_restore_failure = old_restore_failure
        self.snapshot = {"old": {"unit": "previous.service", "active": True},
                         "new": {"unit": migration.NEW_UNIT, "active": False}}

    def __call__(self, *args):
        self.calls.append(args)
        if args == ("daemon-reload",) and self.reload_failure:
            return subprocess.CompletedProcess(args, 1, "", "reload failed")
        if args == ("start", migration.NEW_UNIT):
            # systemctl can return failure after a process has already started.
            self.active[migration.NEW_UNIT] = bool(self.failed_stop_active)
            return subprocess.CompletedProcess(args, 1, "", "new start failed")
        if args == ("stop", migration.NEW_UNIT) and self.failed_stop_active is not None:
            self.active[migration.NEW_UNIT] = self.failed_stop_active
            return subprocess.CompletedProcess(args, 1, "", "new stop failed")
        if args == ("is-active", migration.NEW_UNIT):
            if self.probe_failure:
                raise subprocess.TimeoutExpired(args, 8)
            active = self.active[migration.NEW_UNIT]
            return subprocess.CompletedProcess(args, 0 if active else 3,
                                               "active" if active else "inactive", "")
        if args == ("start", "previous.service"):
            if self.active[migration.NEW_UNIT]:
                raise AssertionError("migration attempted concurrent connection owners")
            if self.old_restore_failure:
                return subprocess.CompletedProcess(args, 1, "", "old restore failed")
        if args[0] in {"start", "stop"}:
            self.active[args[1]] = args[0] == "start"
        return subprocess.CompletedProcess(args, 0, "", "")


class MigrationFailure(unittest.TestCase):
    def test_reload_failure_restores_previous_service(self):
        services = Services(reload_failure=True)
        with self.assertRaisesRegex(ValueError, "reload failed"):
            migration.switch(services.snapshot, services)
        self.assertTrue(services.active["previous.service"])
        self.assertNotIn(("start", migration.NEW_UNIT), services.calls)

    def test_failed_new_stop_with_proven_inactivity_still_restores_previous(self):
        services = Services(failed_stop_active=False)
        with self.assertRaises(ValueError) as failure:
            migration.switch(services.snapshot, services)
        self.assertTrue(services.active["previous.service"])
        self.assertIn(("is-active", migration.NEW_UNIT), services.calls)
        self.assertIn("new start failed", str(failure.exception))
        self.assertIn("new stop failed", str(failure.exception))

    def test_unstoppable_active_replacement_blocks_previous_restore(self):
        services = Services(failed_stop_active=True)
        with self.assertRaises(ValueError) as failure:
            migration.switch(services.snapshot, services)
        self.assertTrue(services.active[migration.NEW_UNIT])
        self.assertFalse(services.active["previous.service"])
        self.assertNotIn(("start", "previous.service"), services.calls)
        self.assertIn("was not restored", str(failure.exception))
        self.assertIn("new stop failed", str(failure.exception))

    def test_unknown_replacement_state_never_starts_previous_owner(self):
        services = Services(failed_stop_active=False, probe_failure=True)
        with self.assertRaisesRegex(ValueError, "activity check"):
            migration.switch(services.snapshot, services)
        self.assertFalse(services.active["previous.service"])
        self.assertNotIn(("start", "previous.service"), services.calls)

    def test_primary_and_restoration_failures_are_both_reported(self):
        services = Services(old_restore_failure=True)
        with self.assertRaises(ValueError) as failure:
            migration.switch(services.snapshot, services)
        self.assertIn("new start failed", str(failure.exception))
        self.assertIn("old restore failed", str(failure.exception))


class MigrationSnapshotInspection(unittest.TestCase):
    def test_fifo_snapshot_is_rejected_without_waiting_for_a_writer(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "rollback.fifo"
            os.mkfifo(path, 0o600)
            result = subprocess.run(
                [sys.executable, str(ROOT / "scripts/migrate.py"), "switch", "--snapshot", str(path)],
                capture_output=True, text=True, timeout=2)
            self.assertEqual(result.returncode, 1)
            self.assertIn("private owned regular file", result.stderr)

    def test_snapshot_validation_rejects_public_symlink_oversize_and_invalid_shapes(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "rollback.json"
            valid = {"version": 1, "uid": os.getuid(),
                     "old": {"unit": "previous.service", "active": True},
                     "new": {"unit": migration.NEW_UNIT, "active": False}}
            path.write_text(json.dumps(valid))
            path.chmod(0o644)
            with self.assertRaisesRegex(ValueError, "private"):
                migration.load(path)
            path.chmod(0o600)
            link = Path(directory) / "link.json"
            link.symlink_to(path)
            with self.assertRaises(OSError):
                migration.load(link)
            path.write_bytes(b"x" * (64 * 1024 + 1))
            with self.assertRaisesRegex(ValueError, "size limit"):
                migration.load(path)
            for invalid in [[], {**valid, "old": []}, {**valid, "new": {"unit": migration.NEW_UNIT, "active": 1}}]:
                path.write_text(json.dumps(invalid))
                with self.assertRaises(ValueError):
                    migration.load(path)
            path.write_text(json.dumps(valid))
            self.assertEqual(migration.load(path), valid)


if __name__ == "__main__":
    unittest.main()
