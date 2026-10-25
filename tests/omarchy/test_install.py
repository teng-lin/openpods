# SPDX-License-Identifier: AGPL-3.0-only
import importlib.util
import json
from pathlib import Path
import tempfile
import unittest
from unittest import mock

ROOT=Path(__file__).resolve().parents[2]
spec=importlib.util.spec_from_file_location('omarchy_install',ROOT/'scripts/install-omarchy.py')
installer=importlib.util.module_from_spec(spec);spec.loader.exec_module(installer)

class InstallTests(unittest.TestCase):
    def setUp(self):
        self.temp=tempfile.TemporaryDirectory();self.addCleanup(self.temp.cleanup)
        self.root=Path(self.temp.name);self.dest=self.root/'plugins'/installer.PLUGIN_ID
    def test_install_update_and_license(self):
        installer.install_plugin(ROOT/'ui/omarchy',self.dest)
        self.assertIn('Copyright (c) 2026 GM',(self.dest/'LICENSE').read_text())
        installer.install_plugin(ROOT/'ui/omarchy',self.dest)
        self.assertEqual(json.loads((self.dest/'manifest.json').read_text())['id'],installer.PLUGIN_ID)
    def test_foreign_or_modified_plugin_is_preserved(self):
        self.dest.mkdir(parents=True);(self.dest/'Panel.qml').write_text('local')
        with self.assertRaises(ValueError):installer.install_plugin(ROOT/'ui/omarchy',self.dest)
        self.assertEqual((self.dest/'Panel.qml').read_text(),'local')
        (self.dest/'Panel.qml').unlink();self.dest.rmdir()
        installer.install_plugin(ROOT/'ui/omarchy',self.dest);(self.dest/'Panel.qml').write_text('modified')
        with self.assertRaises(ValueError):installer.install_plugin(ROOT/'ui/omarchy',self.dest)
        self.assertEqual((self.dest/'Panel.qml').read_text(),'modified')
    def test_symlink_and_extra_files_are_not_replaced(self):
        self.dest.parent.mkdir();self.dest.symlink_to(self.root,target_is_directory=True)
        with self.assertRaises(ValueError):installer.install_plugin(ROOT/'ui/omarchy',self.dest)
        self.dest.unlink();installer.install_plugin(ROOT/'ui/omarchy',self.dest);(self.dest/'custom').write_text('keep')
        with self.assertRaises(ValueError):installer.install_plugin(ROOT/'ui/omarchy',self.dest)
    def test_config_preserves_other_widgets_and_saves_backup(self):
        config=self.root/'shell.json'
        original={'idle':{'lock':300},'bar':{'layout':{'left':[{'id':'omarchy.menu'}],'right':[{'id':installer.PLUGIN_ID},{'id':'omarchy.bluetooth'}]}}}
        config.write_text(json.dumps(original));backup=installer.configure_client(config,Path('/example/librepods-cli'))
        self.assertEqual(json.loads(backup.read_text()),original)
        current=json.loads(config.read_text());self.assertEqual(current['idle'],original['idle']);self.assertEqual(current['bar']['layout']['left'],original['bar']['layout']['left'])
        self.assertEqual(current['bar']['layout']['right'][0]['ctlPath'],'/example/librepods-cli')
        self.assertIsNone(installer.configure_client(config,Path('/example/librepods-cli')))
    def test_missing_entry_and_symlink_config_are_rejected(self):
        config=self.root/'shell.json';config.write_text('{}')
        with self.assertRaises(ValueError):installer.configure_client(config,Path('/example/client'))
        link=self.root/'link';link.symlink_to(config)
        with self.assertRaises(ValueError):installer.configure_client(link,Path('/example/client'))

    def test_enable_waits_for_asynchronous_registry_discovery(self):
        with mock.patch.object(installer.subprocess,'check_output',side_effect=['[]',json.dumps([{'id':installer.PLUGIN_ID}])]) as run, mock.patch.object(installer.time,'sleep'):
            installer.wait_for_plugin()
            self.assertEqual(run.call_count,2)
        with self.assertRaises(RuntimeError):installer.wait_for_plugin(timeout=0)

    def test_installed_client_path_survives_plugin_reload_and_owned_file_upgrade(self):
        binary=Path('/example/client with spaces')
        installer.install_plugin(ROOT/'ui/omarchy',self.dest,binary)
        self.assertIn(json.dumps(str(binary)),(self.dest/'ClientConfig.js').read_text())
        marker=self.dest/installer.OWNED;record=json.loads(marker.read_text())
        del record['ClientConfig.js'];(self.dest/'ClientConfig.js').unlink();marker.write_text(json.dumps(record))
        installer.install_plugin(ROOT/'ui/omarchy',self.dest,binary)
        self.assertIn(json.dumps(str(binary)),(self.dest/'ClientConfig.js').read_text())
