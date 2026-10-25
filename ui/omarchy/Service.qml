// SPDX-License-Identifier: MIT
import QtQuick
import Quickshell
import Quickshell.Io
import "Model.js" as Model
import "ClientConfig.js" as Client

Item {
  id: root
  property var settings: ({})
  property var reported: Model.defaultStatus()
  property bool daemonReachable: false
  property bool schemaUnsupported: false
  property string lastError: ""
  property string actionStatus: ""
  property int retryMs: 1000
  property bool watchStarted: false
  readonly property string ctlPath: String(settings.ctlPath || Client.path || "librepods-cli")
  readonly property bool busy: commandProcess.running
  readonly property bool connected: daemonReachable && reported.connected
  readonly property bool hasAirPods: connected
  readonly property string deviceName: reported.deviceName
  readonly property string modelName: reported.modelName
  readonly property bool isProSeries: reported.isProSeries
  readonly property bool isHeadset: reported.isHeadset
  readonly property bool supportsNoiseOff: connected && reported.supportsNoiseOff
  readonly property bool supportsNoiseControl: connected && reported.supportsNoiseControl
  readonly property bool supportsAdaptive: connected && reported.supportsAdaptive
  readonly property bool supportsConversationalAwareness: connected && reported.supportsConversationalAwareness
  readonly property bool supportsOneBudANC: connected && reported.supportsOneBudANC
  readonly property int noiseMode: connected ? reported.noiseMode : Model.NOISE_UNKNOWN
  readonly property bool adaptiveKnown: reported.adaptiveKnown === true
  readonly property int adaptiveNoiseLevel: reported.adaptiveNoiseLevel
  readonly property bool oneBudANC: reported.oneBudANC
  readonly property bool conversationalAwareness: reported.conversationalAwareness
  readonly property int earDetectionBehavior: reported.earDetectionBehavior
  readonly property int lidState: reported.lidState
  readonly property var leftPod: reported.left
  readonly property var rightPod: reported.right
  readonly property var caseBattery: reported.caseBattery
  readonly property var headsetBattery: reported.headset
  readonly property bool hasBattery: isHeadset ? headsetBattery.level >= 0
    : leftPod.level >= 0 || rightPod.level >= 0 || caseBattery.level >= 0

  function refresh() {
    if (!watchProcess.running) {
      retry.stop()
      watchStarted = false
      watchProcess.command = [ctlPath,"--status","--json"]
      watchProcess.running = true
      startCheck.restart()
    }
    else if (!healthProcess.running) healthProcess.running = true
  }
  function applyLine(raw) {
    var next = Model.parseStatus(raw)
    if (!next.ok) {
      stateGone()
      schemaUnsupported = next.schemaTooNew
      lastError = next.lastError
      return
    }
    if (reported.instance === next.instance && next.revision < reported.revision) {
      return
    }
    reported = next
    daemonReachable = true
    schemaUnsupported = false
    lastError = ""
    retryMs = 1000
  }
  function stateGone() {
    daemonReachable = false
    var stale = Object.assign({}, reported)
    for (var field of ["left", "right", "caseBattery", "headset"]) {
      stale[field] = Object.assign({}, reported[field], {stale:true,charging:false,inEar:false})
    }
    reported = stale
  }
  function send(action, value) {
    if (!daemonReachable || busy) return
    var args = Model.commandFor(reported, action, value)
    if (!args.length) return
    actionStatus = ""
    commandProcess.command = [ctlPath].concat(args)
    commandProcess.running = true
    commandStartCheck.restart()
  }
  function availableModes() { return Model.availableModes(supportsNoiseControl,supportsNoiseOff,supportsAdaptive) }
  function setNoiseMode(mode) { send("noise",mode) }
  function cycleNoiseMode() {
    var modes=availableModes(), index=modes.indexOf(noiseMode)
    if (modes.length) setNoiseMode(modes[(index+1)%modes.length])
  }
  function setAdaptiveNoiseLevel(level) { send("adaptive",Math.max(0,Math.min(100,Math.round(level)))) }
  function setConversationalAwareness(value) { send("ca",value) }
  function setOneBudANC(value) { send("onebud",value) }
  function setEarDetectionBehavior(value) { send("ear",value) }
  function cycleEarDetection() { setEarDetectionBehavior((earDetectionBehavior+1)%Model.EAR_BEHAVIOR_COUNT) }

  Timer { id: retry; interval: root.retryMs; onTriggered: root.refresh() }
  Timer {
    id: startCheck; interval: 1000
    onTriggered: if (!root.watchStarted) {
      watchProcess.running = false
      root.stateGone()
      root.lastError = "Cannot start the configured Rust client: " + root.ctlPath
      retry.restart()
    }
  }
  Timer {
    id: commandStartCheck; interval: 1000
    onTriggered: {
      commandProcess.running = false
      root.actionStatus = "Cannot start the configured Rust client: " + root.ctlPath
    }
  }
  Component.onCompleted: refresh()
  onCtlPathChanged: { watchProcess.running = false; stateGone(); retry.restart() }
  Process {
    id: watchProcess
    onStarted: { root.watchStarted = true; startCheck.stop() }
    stdout: SplitParser { onRead: function(line) { root.applyLine(line) } }
    stderr: StdioCollector { id: watchErrors; waitForEnd: true }
    onExited: function(exitCode) {
      root.stateGone()
      if (!root.schemaUnsupported) root.lastError=Model.elideError(watchErrors.text || "Rust daemon unavailable; reconnecting…")
      retry.restart()
      root.retryMs=Math.min(30000,root.retryMs*2)
    }
  }
  Timer {
    interval: 15000; repeat: true; running: watchProcess.running
    onTriggered: if (!healthProcess.running) healthProcess.running = true
  }
  Process {
    id: healthProcess
    property string instanceAtStart: ""
    onStarted: instanceAtStart = String(root.reported.instance || "")
    command: [root.ctlPath,"status","--json"]
    stdout: StdioCollector { id: healthStatus; waitForEnd: true }
    stderr: StdioCollector { id: healthErrors; waitForEnd: true }
    onExited: function(exitCode) {
      if (instanceAtStart !== String(root.reported.instance || "")) return
      if (exitCode === 0) root.applyLine(healthStatus.text)
      else { root.stateGone(); root.lastError=Model.elideError(healthErrors.text || "Daemon status unavailable") }
    }
  }
  Process {
    id: commandProcess
    onStarted: commandStartCheck.stop()
    stderr: StdioCollector { id: commandErrors; waitForEnd: true }
    onExited: function(exitCode) {
      if (exitCode !== 0) root.actionStatus=Model.elideError(commandErrors.text || "Rust daemon rejected the command")
    }
  }
}
