#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-only
"""Configure a Git-installed Omarchy plugin and enable its installed Rust service."""
import argparse
import importlib.util
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys

ROOT = Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location('omarchy_plugin_install', ROOT / 'scripts/install-omarchy.py')
installer = importlib.util.module_from_spec(spec)
spec.loader.exec_module(installer)


def build_client(root, ble=False):
    cache = Path(os.environ.get('XDG_CACHE_HOME', Path.home() / '.cache')).expanduser()
    if not cache.is_absolute():
        raise ValueError('XDG_CACHE_HOME must be absolute')
    target = cache / 'librepods-rs/omarchy-build'
    if target.resolve().is_relative_to(root.resolve()):
        raise ValueError('build cache must be outside the plugin checkout')
    command = ['cargo', 'build', '--manifest-path', str(root / 'Cargo.toml'), '-p', 'librepods-cli',
               '--release', '--no-default-features', '--locked']
    if ble:
        command += ['--features', 'ble']
    env = os.environ.copy()
    env['CARGO_TARGET_DIR'] = str(target)
    subprocess.run(command, env=env, check=True)
    return target / 'release/openpods-cli'


def configure_checkout(root, binary, config_home):
    expected = config_home / 'omarchy/plugins' / installer.PLUGIN_ID
    if root.resolve() != expected.resolve():
        raise ValueError('run setup from the Omarchy-installed plugin checkout; for a source checkout use scripts/install-omarchy.py')
    binary = binary.expanduser().absolute()
    if not binary.is_file() or not os.access(binary, os.X_OK):
        raise ValueError('the Rust client must be an existing executable')
    subprocess.run(['omarchy', 'plugin', 'validate', str(root)], check=True)
    # Configure in place: the Git checkout and tracked QML files stay untouched.
    return installer.enable_widget(binary, config_home, preserve_placement=True)


def main():
    parser = argparse.ArgumentParser(prog='setup', description=__doc__)
    mode = parser.add_mutually_exclusive_group()
    mode.add_argument('--ctl', type=Path, help='existing OpenPods client; defaults to PATH')
    mode.add_argument('--build', action='store_true', help='build a client into the external user cache')
    parser.add_argument('--ble', action='store_true', help='include BLE support in a --build client; does not enable scanning')
    args = parser.parse_args()
    if args.ble and not args.build:
        parser.error('--ble requires --build')
    config_home = Path.home() / '.config'
    expected = config_home / 'omarchy/plugins' / installer.PLUGIN_ID
    if ROOT.resolve() != expected.resolve():
        parser.error('run this setup from ~/.config/omarchy/plugins/org.librepods.pods; use scripts/install-omarchy.py for a local source checkout')
    ble = args.ble
    if args.build:
        preferences = Path(os.environ.get('XDG_CONFIG_HOME', config_home)) / 'librepods-rs/config.json'
        if preferences.exists():
            stored = json.loads(preferences.read_text())
            if not isinstance(stored, dict):
                raise ValueError('Rust daemon configuration must be a JSON object')
            ble = ble or stored.get('ble_enabled') is True
    binary = build_client(ROOT, ble) if args.build else args.ctl or (shutil.which('openpods-cli') or shutil.which('librepods-cli'))
    if not binary:
        parser.error('OpenPods client was not found; pass --ctl /absolute/path/to/openpods-cli or --build')
    backup = configure_checkout(ROOT, Path(binary), config_home)
    if backup:
        print('Shell configuration backup:', backup)
    print('Configured org.librepods.pods with', Path(binary).absolute())
    print('The Rust service is started and enabled for automatic startup at login.')
    print('After updating QML dependencies, run: omarchy restart shell')


if __name__ == '__main__':
    try:
        main()
    except (OSError, ValueError, RuntimeError, subprocess.CalledProcessError) as error:
        print('Plugin setup:', error, file=sys.stderr)
        raise SystemExit(1)
