import importlib.util
from pathlib import Path
import subprocess
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[2]
spec = importlib.util.spec_from_file_location("migration", ROOT / "scripts/migrate.py")
migration = importlib.util.module_from_spec(spec)
spec.loader.exec_module(migration)


class FakeServices:
    def __init__(self):
        self.active = {"librepods.service": True, migration.NEW_UNIT: False}
        self.calls = []
        self.fail_start = False

    def __call__(self, *args):
        self.calls.append(args)
        verb = args[0]
        if verb == "is-active":
            return subprocess.CompletedProcess(args, 0 if self.active[args[1]] else 3,
                                               "active" if self.active[args[1]] else "inactive", "")
        if verb == "is-enabled":
            return subprocess.CompletedProcess(args, 0, "enabled", "")
        if verb == "start" and args[1] == migration.NEW_UNIT and self.fail_start:
            return subprocess.CompletedProcess(args, 1, "", "injected failure")
        if verb in ["start", "stop"]:
            self.active[args[1]] = verb == "start"
        return subprocess.CompletedProcess(args, 0, "", "")


class Migration(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.path = Path(self.directory.name) / "rollback.json"
        self.services = FakeServices()

    def test_capture_does_not_mutate_or_overwrite_rollback_record(self):
        migration.capture(self.path, "librepods.service", self.services)
        self.assertTrue(all(call[0].startswith("is-") for call in self.services.calls))
        with self.assertRaises(FileExistsError):
            migration.capture(self.path, "librepods.service", self.services)
        self.assertEqual(self.path.stat().st_mode & 0o777, 0o600)

    def test_switch_stops_old_owner_first_and_rollback_restores_it(self):
        migration.capture(self.path, "librepods.service", self.services)
        snapshot = migration.load(self.path)
        migration.switch(snapshot, self.services)
        self.assertFalse(self.services.active["librepods.service"])
        self.assertTrue(self.services.active[migration.NEW_UNIT])
        self.assertLess(self.services.calls.index(("stop", "librepods.service")),
                        self.services.calls.index(("start", migration.NEW_UNIT)))
        migration.rollback(snapshot, self.services)
        self.assertTrue(self.services.active["librepods.service"])
        self.assertFalse(self.services.active[migration.NEW_UNIT])

    def test_failed_replacement_restores_previous_owner(self):
        migration.capture(self.path, "librepods.service", self.services)
        self.services.fail_start = True
        with self.assertRaises(ValueError):
            migration.switch(migration.load(self.path), self.services)
        self.assertTrue(self.services.active["librepods.service"])
        self.assertFalse(self.services.active[migration.NEW_UNIT])

    def test_unknown_or_foreign_snapshots_and_invalid_units_are_rejected(self):
        with self.assertRaises(ValueError):
            migration.validate_old_unit("../something.service")
        migration.capture(self.path, "librepods.service", self.services)
        data = self.path.read_text().replace('"version": 1', '"version": 9')
        self.path.write_text(data)
        with self.assertRaises(ValueError):
            migration.load(self.path)


if __name__ == "__main__":
    unittest.main()
