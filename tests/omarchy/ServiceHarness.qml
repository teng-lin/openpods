// SPDX-License-Identifier: AGPL-3.0-only
import QtQuick
import Quickshell
import "../../ui/omarchy" as Pods

ShellRoot {
  property int stage: 0
  property int ticks: 0
  function fail(message) { console.error("SERVICE_TEST_FAILED",message); Qt.quit() }
  Pods.Service { id: service; settings: ({ctlPath:Quickshell.env("LPRS_TEST_CLI")}) }
  Timer {
    interval: 50; running: true; repeat: true
    onTriggered: {
      ticks++
      if (ticks>160) { fail("timed out at stage " + stage + ": " + service.lastError); return }
      if (stage===0 && service.daemonReachable) {
        if (service.noiseMode!==1 || service.caseBattery.level!==60 || !service.caseBattery.stale) { fail("bad initial status"); return }
        service.setNoiseMode(2)
        if (service.noiseMode!==1) { fail("optimistic mode update"); return }
        stage=1
      } else if(stage===1 && service.busy) {
        service.setNoiseMode(0)
        stage=2
      } else if(stage===2 && !service.busy && service.noiseMode===2) {
        service.setNoiseMode(0)
        stage=3
      } else if(stage===3 && !service.busy && service.actionStatus.indexOf("OutcomeUnknown")>=0) {
        if(service.noiseMode!==2) { fail("failed action changed reported mode"); return }
        stage=4
      } else if(stage===4 && !service.daemonReachable) {
        if(!service.caseBattery.stale || service.caseBattery.level!==60) { fail("lost stale history"); return }
        stage=5
      } else if(stage===5 && service.daemonReachable) {
        if(service.noiseMode!==2) { fail("bad reconnect status"); return }
        console.log("SERVICE_TEST_PASSED");Qt.quit()
      }
    }
  }
}
