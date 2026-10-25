#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-only
"""Validate a fresh Git plugin and its setup using an isolated fake Omarchy session."""
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile

ROOT = Path(__file__).resolve().parents[2]
validator = shutil.which('omarchy-plugin-validate')
if not validator:
    raise SystemExit('This integration check requires Omarchy plugin validation tools')
with tempfile.TemporaryDirectory(prefix='librepods-git-plugin-') as temp:
    home = Path(temp)
    plugin = home / '.config/omarchy/plugins/org.librepods.pods'
    plugin.parent.mkdir(parents=True)
    subprocess.run(['git', 'clone', '--quiet', '--no-hardlinks', str(ROOT), str(plugin)], check=True)
    subprocess.run([validator, str(plugin)], check=True)
    shell = home / '.config/omarchy/shell.json'
    shell.write_text(json.dumps({'version': 1, 'idle': {'lock': 300}, 'bar': {'layout': {'left': [{'id': 'omarchy.menu'}], 'right': [{'id': 'org.librepods.pods'}, {'id': 'omarchy.bluetooth'}]}}}))
    binary = home / 'client with spaces'
    binary.write_text('#!/bin/sh\nexit 0\n'); binary.chmod(0o755)
    tools = home / 'bin'; tools.mkdir()
    fake = tools / 'omarchy'
    fake.write_text('''#!/usr/bin/env python3
import json,os,subprocess,sys
from pathlib import Path
args=sys.argv[1:];home=Path(os.environ['HOME'])
with (home/'calls.jsonl').open('a') as out:out.write(json.dumps(args)+'\\n')
if args[:2]==['plugin','validate']:sys.exit(subprocess.run([os.environ['REAL_VALIDATOR'],args[2]]).returncode)
if args==['shell','shell','rescanPlugins']:sys.exit(0)
if args==['shell','shell','listPlugins']:print('[{"id":"org.librepods.pods"}]');sys.exit(0)
if args[:3]==['plugin','enable','org.librepods.pods']:sys.exit(0)
if args[:4]==['bar','set','org.librepods.pods','ctlPath']:
    config=home/'.config/omarchy/shell.json';data=json.loads(config.read_text())
    data['bar']['layout']['right'][0]['ctlPath']=args[4];config.write_text(json.dumps(data));sys.exit(0)
print('Unexpected setup operation: '+repr(args),file=sys.stderr);sys.exit(2)
''')
    fake.chmod(0o755)
    runtime = home / 'runtime'; runtime.mkdir(mode=0o700)
    env = os.environ.copy()
    env.update(HOME=str(home), XDG_CONFIG_HOME=str(home / '.config'), XDG_RUNTIME_DIR=str(runtime),
               DBUS_SESSION_BUS_ADDRESS='unix:path=/nonexistent', REAL_VALIDATOR=validator,
               PATH=str(tools) + os.pathsep + env['PATH'])
    subprocess.run([str(plugin / 'setup'), '--ctl', str(binary)], env=env, check=True)
    data = json.loads(shell.read_text())
    assert data['idle'] == {'lock': 300}
    assert data['bar']['layout']['right'][0]['ctlPath'] == str(binary)
    assert data['bar']['layout']['right'][1] == {'id': 'omarchy.bluetooth'}
    assert (plugin / '.git').is_dir()
    assert not subprocess.check_output(['git', '-C', str(plugin), 'status', '--porcelain', '--untracked-files=all'], env=env, text=True), 'setup dirtied the Git checkout'
    subprocess.run([validator, str(plugin)], check=True)
    calls = [json.loads(line) for line in (home / 'calls.jsonl').read_text().splitlines()]
    assert len(calls) == 5
    assert [call for call in calls if call[:2] == ['plugin','enable']] == [['plugin','enable','org.librepods.pods']]
    print('Fresh Git plugin: validation, in-place setup, preserved settings and clean checkout passed')
