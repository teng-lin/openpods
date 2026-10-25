// SPDX-License-Identifier: MIT
// Adapted from omarchy-pods, Copyright (c) 2026 GM.
// No QML imports on purpose, so every function here runs in a plain JS harness.

var NOISE_OFF = 0
var NOISE_ANC = 1
var NOISE_TRANSPARENCY = 2
var NOISE_ADAPTIVE = 3

var LID_OPEN = 0
var LID_CLOSED = 1
var LID_UNKNOWN = 2

var EAR_PAUSE_ONE_OUT = 0
var EAR_PAUSE_BOTH_OUT = 1
var EAR_DISABLED = 2

// Highest schema_version this panel knows how to read.
var SUPPORTED_SCHEMA = 1

// Level the daemon reports when a pod or the case has not been heard from.
var LEVEL_UNKNOWN = -1

// noise_mode before the daemon has identified the device.
var NOISE_UNKNOWN = -1

// How many ear-detection behaviours exist, so callers can cycle without a bare 3.
var EAR_BEHAVIOR_COUNT = 3

// nf-md-check U+F012C, the one candidate glyph that rendered on the box.
var GLYPH_CHECK = "\uDB80\uDD2C"

// Longest error the panel will show inside a row, and the cut that leaves room for the ellipsis.
var MAX_ERROR_CHARS = 140
var ELIDED_ERROR_CHARS = 137

function defaultPod() {
  return { level: LEVEL_UNKNOWN, charging: false, inEar: false, stale: false, reportedAt: 0 }
}

// Full shape on every path, so the panel never reads undefined off a parse failure.
function defaultStatus() {
  return {
    ok: false,
    lastError: "",
    schemaVersion: 0,
    schemaTooNew: false,
    connected: false,
    deviceName: "",
    modelName: "",
    isProSeries: false,
    isHeadset: false,
    supportsNoiseOff: false,
    supportsNoiseControl: false,
    supportsAdaptive: false,
    supportsConversationalAwareness: false,
    supportsOneBudANC: false,
    noiseMode: NOISE_UNKNOWN,
    adaptiveNoiseLevel: 0,
    oneBudANC: false,
    conversationalAwareness: false,
    earDetectionBehavior: EAR_PAUSE_ONE_OUT,
    lidState: LID_UNKNOWN,
    left: defaultPod(),
    right: defaultPod(),
    caseBattery: { level: LEVEL_UNKNOWN, charging: false },
    headset: { level: LEVEL_UNKNOWN, charging: false }
  }
}

// Native snapshot schema 1: {daemon_instance, revision, session_generation, connected, device:{capabilities,battery,observations,...},...}.
function fresh(snapshot, field) {
  var m = snapshot.device.observations && snapshot.device.observations[field]
  return !!m && m.stale === false && m.generation === snapshot.session_generation
}

function reading(snapshot, field, raw) {
  var pod = defaultPod()
  var observations = snapshot.device.observations || {}
  if (!raw || !observations[field] || !Number.isInteger(raw.level) || raw.level < 0 || raw.level > 100
      || ["Charging", "Discharging", "OptimizedCharging"].indexOf(raw.status) < 0) return pod
  pod.level = raw.level
  pod.stale = !fresh(snapshot, field)
  pod.charging = !pod.stale && raw.status === "Charging"
  pod.reportedAt = (snapshot.observation_unix_ms || {})[field] || 0
  return pod
}

function control(snapshot, id) {
  if (!snapshot.connected || !fresh(snapshot, "control." + id)) return null
  var raw = (snapshot.device.reported_controls || {})[parseInt(id, 16)]
  return Array.isArray(raw) && raw.length === 4 ? raw[0] : null
}

function parseStatus(raw) {
  var status = defaultStatus()
  var parsed
  try {
    if (String(raw).length > 131072) throw new Error("oversized")
    parsed = JSON.parse(raw)
  } catch (e) { status.lastError = "Could not read native daemon status"; return status }
  if (!parsed || typeof parsed !== "object" || parsed.schema_version !== 1
      || !parsed.device || typeof parsed.device !== "object"
      || typeof parsed.daemon_instance !== "string" || !Number.isSafeInteger(parsed.revision)
      || !Number.isSafeInteger(parsed.session_generation) || parsed.revision < 0 || parsed.session_generation < 0) {
    status.schemaTooNew = !!parsed && parsed.schema_version > SUPPORTED_SCHEMA
    status.lastError = status.schemaTooNew ? "Unsupported daemon status schema" : "Invalid native daemon status"
    return status
  }
  var d = parsed.device
  var c = d.capabilities || {}
  var metadata = parsed.connected === true && fresh(parsed, "metadata")
  status.ok = true
  status.schemaVersion = parsed.schema_version
  status.connected = parsed.connected === true
  status.deviceName = String(d.name || "AirPods")
  status.modelName = modelLabel(d.model)
  status.isProSeries = String(d.model || "").indexOf("AirPodsPro") === 0
  status.isHeadset = c.headset === true
  status.supportsNoiseControl = metadata && c.noise_control === true
  status.supportsNoiseOff = metadata && c.noise_off === true
  status.supportsAdaptive = metadata && c.adaptive === true
  var ca = control(parsed, "28"), one = control(parsed, "1b"), level = control(parsed, "2e")
  status.supportsConversationalAwareness = metadata && c.conversational_awareness === true && (ca === 1 || ca === 2)
  status.supportsOneBudANC = metadata && c.one_bud_anc === true && (one === 1 || one === 2)
  status.conversationalAwareness = ca === 1
  status.oneBudANC = one === 1
  status.noiseMode = fresh(parsed, "noise_control") ? ["Off", "NoiseCancellation", "Transparency", "Adaptive"].indexOf(d.listening_mode) : NOISE_UNKNOWN
  status.adaptiveKnown = Number.isInteger(level) && level >= 0 && level <= 100
  status.adaptiveNoiseLevel = status.adaptiveKnown ? level : 0
  // EarPolicy uses snake_case on the wire.
  status.earDetectionBehavior = {one_removed: EAR_PAUSE_ONE_OUT, both_removed: EAR_PAUSE_BOTH_OUT, off: EAR_DISABLED}[parsed.ear_policy]
  if (status.earDetectionBehavior === undefined) status.earDetectionBehavior = -1
  status.lidState = fresh(parsed, "lid") && [0, 1].indexOf(d.lid_state) >= 0 ? d.lid_state : LID_UNKNOWN
  var b = d.battery || {}
  status.left = reading(parsed, "battery.left", b.left)
  status.right = reading(parsed, "battery.right", b.right)
  status.caseBattery = reading(parsed, "battery.case", b.case_)
  status.headset = reading(parsed, "battery.headset", b.headset)
  if (fresh(parsed, "wear") && (d.wear_primary_side === "Left" || d.wear_primary_side === "Right")) {
    status.left.inEar = (d.wear_primary_side === "Left" ? d.primary_in_ear : d.secondary_in_ear) === true
    status.right.inEar = (d.wear_primary_side === "Right" ? d.primary_in_ear : d.secondary_in_ear) === true
  }
  status.address = String(d.address || "")
  status.instance = parsed.daemon_instance
  status.revision = parsed.revision
  status.generation = parsed.session_generation
  return status
}

function modelLabel(model) {
  var labels = {AirPodsPro2Usbc:"AirPods Pro 2 · USB-C",AirPodsPro2Lightning:"AirPods Pro 2 · Lightning",
    AirPodsMaxUsbc:"AirPods Max · USB-C",AirPodsMaxLightning:"AirPods Max · Lightning",AirPods4Anc:"AirPods 4 ANC"}
  return labels[model] || String(model || "AirPods").replace(/([a-z])([A-Z0-9])/g, "$1 $2")
}

function commandFor(status, action, value) {
  if (!status.ok || !status.connected || !/^([0-9a-f]{2}:){5}[0-9a-f]{2}$/i.test(status.address)) return []
  var args = ["--address", status.address, "--json"]
  if (action === "noise" && availableModes(status.supportsNoiseControl,status.supportsNoiseOff,status.supportsAdaptive).indexOf(value) >= 0)
    return args.concat(["--noise-control",["off","anc","transparency","adaptive"][value]])
  if (action === "ca" && status.supportsConversationalAwareness && typeof value === "boolean") return args.concat(["--conversation-awareness",value ? "on":"off"])
  if (action === "onebud" && status.supportsOneBudANC && typeof value === "boolean") return args.concat(["--one-bud-anc",value ? "on":"off"])
  if (action === "adaptive" && status.supportsAdaptive && status.adaptiveKnown && status.noiseMode === NOISE_ADAPTIVE
      && Number.isInteger(value) && value >= 0 && value <= 100) return args.concat(["adaptive-level",String(value)])
  if (action === "ear" && status.earDetectionBehavior >= 0 && [0,1,2].indexOf(value) >= 0) return args.concat(["ear",["one","both","off"][value]])
  return []
}

// The one list both the cycle and the panel rows are built from, in the order the panel draws them.
function availableModes(hasNoiseControl, hasOff, hasAdaptive) {
  // AirPods 1, 2, 3 and the plain AirPods 4 have no listening modes at all.
  if (!hasNoiseControl) return []
  var modes = []
  if (hasOff) modes.push(NOISE_OFF)
  modes.push(NOISE_TRANSPARENCY)
  if (hasAdaptive) modes.push(NOISE_ADAPTIVE)
  modes.push(NOISE_ANC)
  return modes
}

function noiseModeName(mode) {
  if (mode === NOISE_OFF) return "Off"
  if (mode === NOISE_ANC) return "Noise Cancellation"
  if (mode === NOISE_TRANSPARENCY) return "Transparency"
  if (mode === NOISE_ADAPTIVE) return "Adaptive"
  return "Unknown"
}

function earDetectionName(behavior) {
  if (behavior === EAR_PAUSE_ONE_OUT) return "Pause when one is out"
  if (behavior === EAR_PAUSE_BOTH_OUT) return "Pause when both are out"
  if (behavior === EAR_DISABLED) return "Never pause"
  return "Unknown"
}

function levelText(level) {
  return level === LEVEL_UNKNOWN ? "--" : String(level) + "%"
}

// 0 to 1 for the meter; an unknown level draws an empty track rather than a full one.
function levelFraction(level) {
  if (level === LEVEL_UNKNOWN) return 0
  return Math.max(0, Math.min(100, level)) / 100
}

function lidText(lidState) {
  if (lidState === LID_OPEN) return "Open"
  if (lidState === LID_CLOSED) return "Closed"
  return ""
}

function podMeta(pod, now) {
  if (pod.stale) {
    now = now === undefined ? Date.now() : now
    if (!(pod.reportedAt > 0) || pod.reportedAt > now) return "Last reported"
    var minutes = Math.floor((now-pod.reportedAt)/60000)
    return "Last reported · " + (minutes < 1 ? "just now" : minutes < 60 ? minutes + "m ago" : minutes < 1440 ? Math.floor(minutes/60) + "h ago" : Math.floor(minutes/1440) + "d ago")
  }
  if (pod.charging) return "Charging"
  if (pod.inEar) return "In ear"
  return ""
}

// Collapse the native CLI's stderr into one line the panel can show inside a row.
function elideError(text) {
  var value = String(text || "").replace(/\s+/g, " ").trim()
  return value.length > MAX_ERROR_CHARS ? value.substring(0, ELIDED_ERROR_CHARS) + "…" : value
}
