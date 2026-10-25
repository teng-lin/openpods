#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-only
"""Install the Omarchy client and start its installed service when enabled."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import time

ROOT = Path(__file__).resolve().parents[1]
PLUGIN_ID = 'org.librepods.pods'
OWNED = '.librepods-owned.json'
FILES = ['manifest.json', 'Panel.qml', 'Service.qml', 'Model.js', 'AirPodsIcon.qml', 'ClientConfig.js', 'LICENSE', 'UPSTREAM.md']


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def install_plugin(source, destination, binary=None):
    if destination.is_symlink():
        raise ValueError('plugin destination is a symlink')
    if destination.exists():
        marker = destination / OWNED
        if marker.is_symlink() or not marker.is_file():
            raise ValueError('existing plugin has no ownership record')
        record = json.loads(marker.read_text())
        if not isinstance(record, dict) or not record or not set(record).issubset(FILES) or any((destination / name).is_symlink()
                or not (destination / name).is_file() or digest(destination / name) != record[name] for name in record):
            raise ValueError('installed plugin was modified; preserving local changes')
        if set(p.name for p in destination.iterdir()) != set(list(record) + [OWNED]):
            raise ValueError('installed plugin contains extra files; preserving local changes')
    destination.parent.mkdir(parents=True, exist_ok=True)
    # Stage outside the plugin search directory so the shell never discovers a partial copy.
    with tempfile.TemporaryDirectory(prefix='.librepods-plugin-', dir=destination.parent.parent) as temp:
        stage = Path(temp) / 'new'
        stage.mkdir()
        for name in FILES:
            if (source / name).is_symlink():
                raise ValueError('source plugin contains a symlink')
            shutil.copy2(source / name, stage / name)
        if binary is not None:
            (stage / 'ClientConfig.js').write_text('// SPDX-License-Identifier: MIT\nvar path = ' + json.dumps(str(binary)) + '\n')
        (stage / OWNED).write_text(json.dumps({name: digest(stage / name) for name in FILES}, indent=2) + '\n')
        backup = Path(temp) / 'previous'
        if destination.exists():
            destination.rename(backup)
        try:
            stage.rename(destination)
        except OSError:
            if backup.exists():
                backup.rename(destination)
            raise
    return destination


def configure_client(config, binary):
    if config.is_symlink():
        raise ValueError('shell config is a symlink')
    content = config.read_bytes()
    data = json.loads(content)
    found = False
    for section in data.get('bar', {}).get('layout', {}).values():
        if not isinstance(section, list):
            continue
        for item in section:
            if isinstance(item, dict) and item.get('id') == PLUGIN_ID:
                item['ctlPath'] = str(binary)
                found = True
    if not found:
        raise ValueError('plugin is not in the bar layout; enable it first')
    if json.loads(content) == data:
        return None
    fd, backup = tempfile.mkstemp(prefix='shell.json.librepods-backup-', dir=config.parent)
    with os.fdopen(fd, 'wb') as output:
        output.write(content)
    fd, temporary = tempfile.mkstemp(prefix='.shell-librepods-', dir=config.parent)
    try:
        with os.fdopen(fd, 'w') as output:
            json.dump(data, output, indent=2)
            output.write('\n')
        os.replace(temporary, config)
    finally:
        if os.path.exists(temporary):
            os.unlink(temporary)
    return Path(backup)


def wait_for_plugin(timeout=5):
    deadline=time.monotonic()+timeout
    while time.monotonic()<deadline:
        raw=subprocess.check_output(['omarchy','shell','shell','listPlugins'],text=True)
        if any(item.get('id')==PLUGIN_ID for item in json.loads(raw)):
            return
        time.sleep(0.1)
    raise RuntimeError('Omarchy did not discover the installed plugin before the deadline')


def enable_widget(binary, config_home, preserve_placement=False):
    subprocess.run(['systemctl', '--user', 'daemon-reload'], check=True)
    subprocess.run(['systemctl', '--user', 'enable', '--now', 'librepods-rs.service'], check=True)
    subprocess.run(['omarchy', 'shell', 'shell', 'rescanPlugins'], check=True)
    wait_for_plugin()
    command = ['omarchy', 'plugin', 'enable', PLUGIN_ID]
    if not preserve_placement:
        command += ['--section', 'right', '--before', 'omarchy.bluetooth']
    subprocess.run(command, check=True)
    backup = configure_client(config_home / 'omarchy/shell.json', binary)
    subprocess.run(['omarchy','bar','set',PLUGIN_ID,'ctlPath',str(binary)],check=True)
    return backup


def main():
    config_home = Path(os.environ.get('XDG_CONFIG_HOME', Path.home() / '.config'))
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--plugin-dir', type=Path, default=config_home / 'omarchy/plugins' / PLUGIN_ID)
    parser.add_argument('--ctl', default=(shutil.which('openpods-cli') or shutil.which('librepods-cli')))
    parser.add_argument('--enable', action='store_true', help='place the widget before Bluetooth and configure its client')
    args = parser.parse_args()
    binary = Path(args.ctl).expanduser().absolute() if args.ctl else None
    if not binary or not binary.is_file() or not os.access(binary, os.X_OK):
        parser.error('--ctl must name an existing executable OpenPods client')
    subprocess.run(['omarchy', 'plugin', 'validate', str(ROOT / 'ui/omarchy')], check=True)
    print('Installed', install_plugin(ROOT / 'ui/omarchy', args.plugin_dir.expanduser().absolute(), binary))
    if args.enable:
        backup = enable_widget(binary, config_home)
        if backup:
            print('Shell configuration backup:', backup)
        print('Enabled native Rust AirPods widget and automatic daemon startup.')


if __name__ == '__main__':
    main()
