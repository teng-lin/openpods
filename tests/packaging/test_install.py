"""Package operations use temporary prefixes and never touch a live user service."""

import importlib.util
import json
from pathlib import Path
import stat
import shutil
import subprocess
import sys
import tempfile
import unittest
from unittest import mock

ROOT = Path(__file__).resolve().parents[2]
spec = importlib.util.spec_from_file_location("installer", ROOT / "scripts/install.py")
installer = importlib.util.module_from_spec(spec)
spec.loader.exec_module(installer)


class Installation(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.root = Path(self.directory.name)
        self.prefix = self.root / "prefix with space%${PODS_TEST}"
        self.unit_dir = self.prefix / "share/systemd/user"
        self.binary = self.root / "built-binary"
        self.binary.write_bytes(b"test binary\n")

    def test_install_is_owned_idempotent_and_uninstall_preserves_settings(self):
        installer.install(self.prefix, self.binary, self.unit_dir)
        installed = self.prefix / "bin/librepods-cli"
        self.assertEqual(installed.read_bytes(), self.binary.read_bytes())
        self.assertEqual(stat.S_IMODE(installed.stat().st_mode), 0o755)
        manifest = installer.manifest_path(self.prefix)
        self.assertEqual(stat.S_IMODE(manifest.stat().st_mode), 0o600)
        unit = (self.unit_dir / installer.UNIT).read_text()
        self.assertIn('ExecStart=:"', unit)
        self.assertIn("space%%${PODS_TEST}", unit)
        self.assertIn("NoNewPrivileges=yes", unit)
        self.assertIn("CapabilityBoundingSet=\n", unit)
        self.assertIn("RuntimeDirectoryMode=0700", unit)
        installer.install(self.prefix, self.binary, self.unit_dir)
        settings = self.prefix / "settings.json"
        settings.write_text("user settings")
        installer.uninstall(self.prefix)
        self.assertFalse(installed.exists())
        self.assertFalse(manifest.exists())
        self.assertEqual(settings.read_text(), "user settings")

    @unittest.skipUnless(shutil.which("systemd-analyze"), "systemd parser unavailable")
    def test_rendered_unit_accepts_literal_space_percent_and_dollar_path(self):
        installer.install(self.prefix, Path(sys.executable), self.unit_dir)
        result = subprocess.run(["systemd-analyze", "--user", "verify",
            str(self.unit_dir / installer.UNIT)], capture_output=True, text=True)
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_modified_and_unowned_files_are_not_overwritten_or_removed(self):
        installer.install(self.prefix, self.binary, self.unit_dir)
        installed = self.prefix / "bin/librepods-cli"
        installed.write_text("user replacement")
        with self.assertRaises(ValueError):
            installer.install(self.prefix, self.binary, self.unit_dir)
        with self.assertRaises(ValueError):
            installer.uninstall(self.prefix)
        self.assertEqual(installed.read_text(), "user replacement")

    def test_symlink_destination_is_rejected(self):
        installed = self.prefix / "bin/librepods-cli"
        installed.parent.mkdir(parents=True)
        installed.symlink_to(self.binary)
        with self.assertRaises(ValueError):
            installer.install(self.prefix, self.binary, self.unit_dir)

    def test_unknown_manifest_is_rejected(self):
        path = installer.manifest_path(self.prefix)
        path.parent.mkdir(parents=True)
        path.write_text(json.dumps({"version": 999, "files": {}}))
        with self.assertRaises(ValueError):
            installer.uninstall(self.prefix)

    def test_failed_install_rolls_back_files_already_written(self):
        installer.install(self.prefix, self.binary, self.unit_dir)
        installed = self.prefix / "bin/librepods-cli"
        original = installed.read_bytes()
        self.binary.write_bytes(b"replacement binary")
        real_write = installer.atomic_write
        failed = False

        def fail_once(path, data, mode):
            nonlocal failed
            if path == self.unit_dir / installer.UNIT and not failed:
                failed = True
                raise OSError("injected read-only destination")
            real_write(path, data, mode)

        with mock.patch.object(installer, "atomic_write", side_effect=fail_once):
            with self.assertRaises(OSError):
                installer.install(self.prefix, self.binary, self.unit_dir)
        self.assertEqual(installed.read_bytes(), original)
        installer.uninstall(self.prefix)

    def test_systemd_argument_cannot_inject_unit_directives(self):
        with self.assertRaises(ValueError):
            installer.install(self.root / "bad\nExecStart=unsafe", self.binary, self.unit_dir)


if __name__ == "__main__":
    unittest.main()
