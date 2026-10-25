# SPDX-License-Identifier: AGPL-3.0-only
import importlib.util
import contextlib
import io
import sys
import json
import os
from pathlib import Path
import tempfile
import unittest
from unittest import mock

ROOT = Path(__file__).resolve().parents[2]
spec = importlib.util.spec_from_file_location('plugin_setup', ROOT / 'scripts/setup-omarchy.py')
setup = importlib.util.module_from_spec(spec)
spec.loader.exec_module(setup)


class PluginPackageTests(unittest.TestCase):
    def test_root_manifest_matches_ui_and_has_a_real_nested_entry_point(self):
        package = json.loads((ROOT / 'manifest.json').read_text())
        ui = json.loads((ROOT / 'ui/omarchy/manifest.json').read_text())
        self.assertEqual(package['entryPoints']['barWidget'], 'ui/omarchy/Panel.qml')
        self.assertTrue((ROOT / package['entryPoints']['barWidget']).is_file())
        self.assertEqual(package['license'], 'AGPL-3.0-only AND MIT')
        package['entryPoints'] = ui['entryPoints']
        package['license'] = ui['license']
        self.assertEqual(package, ui)
        self.assertTrue(os.access(ROOT / 'setup', os.X_OK))

    def test_git_checkout_configuration_does_not_replace_source_or_git(self):
        with tempfile.TemporaryDirectory() as tmp:
            config = Path(tmp) / '.config'
            checkout = config / 'omarchy/plugins' / setup.installer.PLUGIN_ID
            (checkout / '.git').mkdir(parents=True)
            (checkout / '.git/HEAD').write_text('keep-git')
            (checkout / 'Panel.qml').write_text('keep-source')
            binary = Path(tmp) / 'client with spaces'
            binary.write_text('#!/bin/sh\nexit 0\n'); binary.chmod(0o755)
            with mock.patch.object(setup.subprocess, 'run') as run, mock.patch.object(setup.installer, 'enable_widget') as enable:
                setup.configure_checkout(checkout, binary, config)
                run.assert_called_once_with(['omarchy', 'plugin', 'validate', str(checkout)], check=True)
                enable.assert_called_once_with(binary, config, preserve_placement=True)
            self.assertEqual((checkout / '.git/HEAD').read_text(), 'keep-git')
            self.assertEqual((checkout / 'Panel.qml').read_text(), 'keep-source')
            self.assertFalse((checkout / setup.installer.OWNED).exists())

    def test_setup_refuses_wrong_location_or_missing_client(self):
        with tempfile.TemporaryDirectory() as tmp, mock.patch.object(setup.subprocess, 'run') as run:
            root = Path(tmp)
            with self.assertRaises(ValueError):
                setup.configure_checkout(root, root / 'client', root / 'config')
            checkout = root / 'config/omarchy/plugins' / setup.installer.PLUGIN_ID
            checkout.mkdir(parents=True)
            with self.assertRaises(ValueError):
                setup.configure_checkout(checkout, root / 'missing', root / 'config')
            run.assert_not_called()

    def test_build_is_locked_optional_ble_and_outside_checkout(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp) / 'plugin'; root.mkdir()
            cache = Path(tmp) / 'cache'
            with mock.patch.dict(os.environ, {'XDG_CACHE_HOME': str(cache)}), mock.patch.object(setup.subprocess, 'run') as run:
                binary = setup.build_client(root)
                command = run.call_args.args[0]
                self.assertIn('--locked', command)
                self.assertNotIn('--features', command)
                self.assertEqual(run.call_args.kwargs['env']['CARGO_TARGET_DIR'], str(cache / 'librepods-rs/omarchy-build'))
                self.assertEqual(binary, cache / 'librepods-rs/omarchy-build/release/openpods-cli')
                setup.build_client(root, ble=True)
                self.assertEqual(run.call_args.args[0][-2:], ['--features', 'ble'])
            with mock.patch.dict(os.environ, {'XDG_CACHE_HOME': str(root / 'cache')}):
                with self.assertRaises(ValueError): setup.build_client(root)

    def test_build_follows_existing_ble_preference_without_enabling_it(self):
        with tempfile.TemporaryDirectory() as tmp:
            home=Path(tmp);config=home/'.config'
            checkout=config/'omarchy/plugins'/setup.installer.PLUGIN_ID
            checkout.mkdir(parents=True)
            preferences=config/'librepods-rs/config.json';preferences.parent.mkdir()
            preferences.write_text('{"ble_enabled":true}')
            binary=home/'client'
            with mock.patch.object(setup,'ROOT',checkout), mock.patch.object(Path,'home',return_value=home), mock.patch.dict(os.environ,{'XDG_CONFIG_HOME':str(config)}), mock.patch.object(sys,'argv',['setup','--build']), mock.patch.object(setup,'build_client',return_value=binary) as build, mock.patch.object(setup,'configure_checkout',return_value=None), contextlib.redirect_stdout(io.StringIO()):
                setup.main()
                build.assert_called_once_with(checkout,True)
            self.assertEqual(preferences.read_text(),'{"ble_enabled":true}')
