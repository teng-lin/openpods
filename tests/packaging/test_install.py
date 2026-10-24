"""Package operations use temporary prefixes and never touch a live user service."""

import importlib.util
import json
from pathlib import Path
import stat
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[2]
spec = importlib.util.spec_from_file_location("installer", ROOT / "scripts/install.py")
installer = importlib.util.module_from_spec(spec)
spec.loader.exec_module(installer)


class Installation(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.root = Path(self.directory.name)
        self.prefix = self.root / "prefix with space%"
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
        self.assertIn('ExecStart="', unit)
        self.assertIn("space%%", unit)
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


if __name__ == "__main__":
    unittest.main()
