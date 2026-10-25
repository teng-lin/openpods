#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-only
"""Exercise the actual QML service with a fake CLI, without Bluetooth or a desktop."""
from pathlib import Path
import json
import os
import shutil
import subprocess
import tempfile

ROOT=Path(__file__).resolve().parents[2]
snapshot={'schema_version':1,'daemon_instance':'test','revision':1,'session_generation':1,'connected':True,'ear_policy':'off','device':{'name':'Synthetic','address':'00:11:22:33:44:55','model':'AirPodsPro2Usbc','capabilities':{'noise_control':True,'noise_off':True},'listening_mode':'NoiseCancellation','battery':{'case_':{'level':60,'status':'Discharging'}},'observations':{'metadata':{'generation':1,'stale':False},'noise_control':{'generation':1,'stale':False},'battery.case':{'generation':1,'stale':True}}}}
with tempfile.TemporaryDirectory(prefix='lprs-qml-') as tmp:
    root=Path(tmp);(root/'Pods').mkdir()
    for name in ['Model.js','Service.qml','ClientConfig.js']:shutil.copy2(ROOT/'ui/omarchy'/name,root/'Pods'/name)
    (root/'shell.qml').write_text((ROOT/'tests/omarchy/ServiceHarness.qml').read_text().replace('../../ui/omarchy','Pods'))
    (root/'state.json').write_text(json.dumps(snapshot))
    cli=root/'client';cli.write_text('''#!/usr/bin/env python3
import json,sys,time,os
from pathlib import Path
root=Path(__file__).parent
if '--status' in sys.argv:
    first=not (root/'restarted').exists()
    if first:(root/'restarted').touch()
    start=time.monotonic();last=''
    while True:
        raw=(root/'state.json').read_text()
        if raw!=last:print(raw,flush=True);last=raw
        if first and time.monotonic()-start>1.5:sys.exit(0)
        time.sleep(.03)
elif 'status' in sys.argv:print((root/'state.json').read_text())
elif '--noise-control' in sys.argv:
    with (root/'actions').open('a') as f:f.write(sys.argv[-1]+'\\n')
    time.sleep(.3)
    if sys.argv[-1]=='off':print('OutcomeUnknown: confirmation lost',file=sys.stderr);sys.exit(1)
    s=json.loads((root/'state.json').read_text());s['device']['listening_mode']='Transparency';s['revision']+=1
    temp=root/'next.json';temp.write_text(json.dumps(s));os.replace(temp,root/'state.json')
else:sys.exit(2)
''');cli.chmod(0o755)
    env=os.environ.copy();env.update(QT_QPA_PLATFORM='offscreen',QT_QPA_PLATFORMTHEME='',QT_QUICK_CONTROLS_STYLE='Basic',LPRS_TEST_CLI=str(cli));env.pop('WAYLAND_DISPLAY',None);env.pop('DISPLAY',None)
    result=subprocess.run(['quickshell','-p',str(root)],env=env,stdout=subprocess.PIPE,stderr=subprocess.STDOUT,text=True,timeout=12)
    print(result.stdout)
    assert result.returncode==0 and 'SERVICE_TEST_PASSED' in result.stdout and 'SERVICE_TEST_FAILED' not in result.stdout
    assert (root/'actions').read_text().splitlines()==['transparency','off'],'queued or uncertain actions must never replay'
    (root/'Pods/ClientConfig.js').write_text('var path = '+json.dumps(str(cli))+'\n')
    (root/'shell.qml').write_text('''import QtQuick
import Quickshell
import "Pods" as Pods
ShellRoot {
  property int stage: 0
  property int ticks: 0
  Pods.Service { id: service; settings: ({ctlPath:"/missing/librepods-test-client"}) }
  Timer {
    interval: 50; repeat: true; running: true
    onTriggered: {
      ticks++
      if(ticks>100) { console.error("SERVICE_TEST_FAILED", "missing-client recovery"); Qt.quit(); return }
      if(stage===0 && !service.daemonReachable && service.lastError.indexOf("Cannot start")>=0) {
        service.settings=({});stage=1
      } else if(stage===1 && service.daemonReachable && service.noiseMode===2) {
        console.log("CLIENT_FALLBACK_PASSED");Qt.quit()
      }
    }
  }
}
''')
    result=subprocess.run(['quickshell','-p',str(root)],env=env,stdout=subprocess.PIPE,stderr=subprocess.STDOUT,text=True,timeout=8)
    print(result.stdout)
    assert result.returncode==0 and 'CLIENT_FALLBACK_PASSED' in result.stdout and 'SERVICE_TEST_FAILED' not in result.stdout

print('QML service: streaming, pending state, typed command, errors, stale disconnect and reconnect passed')
