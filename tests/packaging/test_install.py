"""Package operations use temporary prefixes and never touch a live user service."""

import importlib.util
import json
import os
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

    def test_legacy_opt_in_is_narrow_and_preserves_state_on_uninstall(self):
        state_home = self.root / "state with space % ${PODS_TEST}"
        installer.install(self.prefix, Path(sys.executable), self.unit_dir, state_home)
        native, legacy = state_home / "librepods-rs", state_home / "librepods"
        for directory in [native, legacy]:
            self.assertEqual(stat.S_IMODE(directory.stat().st_mode), 0o700)
        unit = (self.unit_dir / installer.UNIT).read_text()
        self.assertIn("ProtectHome=read-only", unit)
        self.assertIn('Environment="XDG_STATE_HOME=', unit)
        self.assertIn("state with space %% ${PODS_TEST}/librepods", unit)
        if shutil.which("systemd-analyze"):
            result = subprocess.run(["systemd-analyze", "--user", "verify", str(self.unit_dir / installer.UNIT)], capture_output=True, text=True)
            self.assertEqual(result.returncode, 0, result.stderr)
        status = legacy / "status.json"
        status.write_text("user state")
        installer.uninstall(self.prefix)
        self.assertEqual(status.read_text(), "user state")

    def test_default_install_never_grants_or_creates_legacy_state(self):
        installer.install(self.prefix, self.binary, self.unit_dir)
        unit = (self.unit_dir / installer.UNIT).read_text()
        self.assertNotIn("XDG_STATE_HOME=", unit)
        self.assertNotIn('ReadWritePaths="', unit)
        self.assertFalse((self.root / "state").exists())

    def test_legacy_opt_in_refuses_existing_foreign_status_or_nonprivate_directory(self):
        state_home = self.root / "state"
        legacy = state_home / "librepods"
        legacy.mkdir(parents=True, mode=0o700)
        status = legacy / "status.json"
        status.write_text('{"daemon_instance":"previous-owner"}')
        with self.assertRaisesRegex(ValueError, "legacy status already exists"):
            installer.install(self.prefix, self.binary, self.unit_dir, state_home)
        self.assertEqual(status.read_text(), '{"daemon_instance":"previous-owner"}')
        status.unlink()
        legacy.chmod(0o755)
        with self.assertRaisesRegex(ValueError, "private owned real"):
            installer.install(self.prefix, self.binary, self.unit_dir, state_home)
        legacy.rmdir()
        legacy.symlink_to(self.root, target_is_directory=True)
        with self.assertRaisesRegex(ValueError, "private owned real"):
            installer.install(self.prefix, self.binary, self.unit_dir, state_home)

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
        path.chmod(0o600)
        with self.assertRaises(ValueError):
            installer.uninstall(self.prefix)

    def test_manifest_fifo_cannot_block_uninstall_and_private_json_is_bounded(self):
        manifest = installer.manifest_path(self.prefix)
        manifest.parent.mkdir(parents=True)
        os.mkfifo(manifest, mode=0o600)
        result = subprocess.run([sys.executable, str(ROOT / "scripts/install.py"), "uninstall",
            "--prefix", str(self.prefix)], capture_output=True, text=True, timeout=2)
        self.assertEqual(result.returncode, 1)
        self.assertIn("private owned regular file", result.stderr)
        self.assertTrue(stat.S_ISFIFO(manifest.stat().st_mode))
        manifest.unlink()
        manifest.write_bytes(b" " * 65537)
        manifest.chmod(0o600)
        with self.assertRaisesRegex(ValueError, "exceeds"):
            installer.read_manifest(self.prefix)
        manifest.write_text("[]")
        with self.assertRaisesRegex(ValueError, "version/shape"):
            installer.read_manifest(self.prefix)
        manifest.write_text('{"version":1,"files":{}}')
        manifest.chmod(0o644)
        with self.assertRaisesRegex(ValueError, "private owned"):
            installer.read_manifest(self.prefix)

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

    def test_openpods_commands_and_legacy_alias_are_installed_and_removed(self):
        daemon=self.binary.with_name("openpodsd");daemon.write_bytes(b"daemon binary")
        installer.install(self.prefix,self.binary,self.unit_dir)
        self.assertEqual((self.prefix/"bin/openpods-cli").read_bytes(),self.binary.read_bytes())
        self.assertEqual((self.prefix/"bin/librepods-cli").read_bytes(),self.binary.read_bytes())
        self.assertEqual((self.prefix/"bin/openpodsd").read_bytes(),b"daemon binary")
        installer.uninstall(self.prefix)
        self.assertFalse((self.prefix/"bin/openpods-cli").exists())
        self.assertFalse((self.prefix/"bin/openpodsd").exists())

    def test_rename_preserves_unowned_canonical_commands(self):
        command=self.prefix/"bin/openpods-cli";command.parent.mkdir(parents=True);command.write_text("existing user command")
        with self.assertRaisesRegex(ValueError,"unowned"):
            installer.install(self.prefix,self.binary,self.unit_dir)
        self.assertEqual(command.read_text(),"existing user command")


if __name__ == "__main__":
    unittest.main()
