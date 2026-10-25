# LibrePods RS

A Rust Apple Accessory Protocol (AACP) implementation with a persistent Linux
service, local CLI, optional native GPUI desktop, and reusable protocol core. The primary development device
is AirPods Pro 2 USB-C on Omarchy. See the [release evidence](docs/release-evidence.md)
for acceptance status; unfinished hardware and soak gates remain unfinished.

The default build contains no BLE scanner or cryptography backend. Connected
battery, listening-mode controls, ear reports and reconnect work over AACP.
Automatic playback, routing, profile recovery, volume ducking and notifications
are opt-in host policies. Ordinary operation does not require root, file
capabilities, Bluetooth identity changes or an audio-server restart.

## Build and start

Install the toolchain pinned in [rust-toolchain.toml](rust-toolchain.toml), a C
linker, and the development dependencies appropriate to your distribution.
BLE builds additionally use the system D-Bus development library.

```bash
cargo build -p librepods-cli --release --no-default-features --locked
./target/release/librepods-cli serve --address AA:BB:CC:DD:EE:FF
```

Use an already paired device. The service can wait while it is disconnected;
`device connect` can explicitly ask BlueZ to connect it. Without `--address`, the service
selects a single connected AirPods device and rejects ambiguity. It owns the
AACP connection until stopped. Run clients in another terminal:

```bash
./target/release/librepods-cli status --json
./target/release/librepods-cli --status
./target/release/librepods-cli capabilities --json
./target/release/librepods-cli doctor --json
./target/release/librepods-cli --noise-control anc
./target/release/librepods-cli --noise-control transparency
./target/release/librepods-cli --noise-control adaptive
./target/release/librepods-cli --conversation-awareness on
./target/release/librepods-cli --one-bud-anc off
./target/release/librepods-cli adaptive-level 50
```

Adaptive level accepts 0–100 and requires a fresh Adaptive-mode report. Off and
other settings depend on the model and firmware. Unknown readings remain unknown;
retained battery readings become stale after disconnect. Confirmed commands
observe the reported target state after dispatch. A lost confirmation yields
`outcome_unknown`; inspect status before retrying. Requests are serialized and
expired or disconnected clients cannot leave an unsent mutation queued.

An explicit client `--address` asserts the daemon's selected device; it does not
silently retarget a running service. `--direct` is an isolated diagnostic session
that holds the same ownership lock and refuses a competing daemon/direct owner.
Rename and long-press commands have protocol support but remain experimental
until their actual firmware confirmations are accepted.

## Omarchy bar panel

An optional QML panel adapted from omarchy-pods integrates battery and listening
controls into the Omarchy bar, using the native Rust daemon. The root manifest
and executable `setup` support standard Git plugin installation. See
[installation and behavior](docs/omarchy-panel.md). It preserves the upstream MIT
attribution and uses original icons.

## Native desktop

A standalone GPUI window provides battery percentages, noise modes, Adaptive
strength, conversation awareness, single-earbud ANC, pause policy, and Bluetooth
connection actions. Build it separately; BLE remains optional:

```sh
cargo run --manifest-path librepods-gui/Cargo.toml --features desktop --locked -- --demo
# Omit --demo to use your running daemon.
```

See [GUI setup, behavior and verification](docs/gui.md). The default daemon/CLI
build remains free of GPUI and Qt.

## Bluetooth device management

```bash
librepods-cli --address AA:BB:CC:DD:EE:FF device connect
librepods-cli device disconnect
librepods-cli --address AA:BB:CC:DD:EE:FF device forget --yes
librepods-cli reconnect
```

These commands require the persistent service. Bluetooth connect confirms the
BlueZ link and schedules AACP recovery; `status.connected` separately reports
control readiness. Disconnect stops Bluetooth audio and sets
`connection_desired: false`, suppressing automatic control reconnection through
watchdog and suspend/resume events. Explicit `device connect` or `reconnect`
re-enables control recovery. This intent lasts for the daemon's lifetime.

Forget requires both an explicit address and `--yes`, disconnects first when
necessary, removes the pairing and deletes that device's private proximity keys.
Re-pair through the desktop before connecting again. The existing `reconnect`
command reopens only the vendor control channel and does not connect Bluetooth.
See [device-management semantics and tests](docs/device-management.md) for
uncertain outcomes, partial cleanup failures and target selection.

## Audio and desktop policies

Audio observation and repairs are separate from ANC and battery communication:

```bash
./target/release/librepods-cli audio status --json
./target/release/librepods-cli audio output SINK_NAME
./target/release/librepods-cli audio input SOURCE_NAME
./target/release/librepods-cli audio profile PROFILE_NAME
./target/release/librepods-cli audio move-stream STREAM_ID SINK_NAME
./target/release/librepods-cli audio unmute
./target/release/librepods-cli ear off
```

Audio discovery uses structured `pactl` JSON through PipeWire's PulseAudio
interface or PulseAudio. MPRIS playback and notifications use the user bus.
Missing optional desktop facilities degrade audio automation while AACP remains
usable. An external microphone remains valid with AirPods playback. A missing
microphone in a playback profile does not trigger an implicit headset switch.
Choose a capture-capable profile explicitly when that playback tradeoff is wanted.

`ear one` and `ear both` persist host pause preferences. Resume requires exact,
confirmed ownership of a player's pause; manual playback or owner loss cancels
that ownership. Further opt-ins are configured in the private, schema-versioned
`$XDG_CONFIG_HOME/librepods-rs/config.json` (default `~/.config`):

```json
{"schema_version":1,"ca_ducking":false,"audio_recovery":false,"auto_route":false,"low_battery_notifications":false}
```

Large audio graphs are summarized explicitly in ordinary status. Detailed audio
requests have a bounded response and report `response_too_large` when necessary.
Native requests are capped at 16 KiB and responses at 128 KiB.

## Installation and Omarchy compatibility

Follow [deployment and rollback](docs/deployment.md) for the hardened systemd user
unit, private paths, migration and restoration. Test a temporary switch before
changing startup enablement. Keep exactly one connection owner enabled.

The existing panel can use the compatibility client:

```bash
librepods-cli compat status
librepods-cli compat noise:anc
librepods-cli compat ear:both
```

Optional legacy publication requires `legacy_compatibility: true`. It preserves
foreign status files and reports a publication error rather than replacing them.
The native socket is `$XDG_RUNTIME_DIR/librepods-rs/control.sock`; native status
is `$XDG_STATE_HOME/librepods-rs/status.json` (default `~/.local/state`).

## Optional BLE

```bash
cargo build -p librepods-cli --release --features ble --locked
librepods-cli keys fetch
librepods-cli ble on
librepods-cli ble off
librepods-cli keys forget
```

A BLE-capable build still defaults to scanning off. Fetching keys is explicit and
requires a ready persistent session; keys are stored privately outside public
status and never printed. Identity is verified before merging advertisements.
Scanning pauses for connection preparation, recovery and suspend. Connected
scanning is disabled pending a separate controller coexistence acceptance run.
Case/lid telemetry may remain unavailable indefinitely without affecting control.

## Architecture and verification

| Crate | Responsibility |
| --- | --- |
| `librepods-core` | Pure parsers, encoders, reducer, capabilities and deterministic policies |
| `librepods-linux` | BlueZ, cancellable L2CAP, optional BLE, sleep and desktop adapters |
| `librepods-daemon` | Single session owner, command queue, IPC, persistence and policy orchestration |
| `librepods-cli` | Thin local client and Linux service entry point |
| `librepods-windows` | Platform library adapters; cross-compiled contracts, explicit unsupported operations |

The persistent service and new CLI runtime currently support Linux. Windows
library compilation does not establish Windows runtime or hardware parity.
Other model mappings describe verified protocol capabilities, not equivalent
hardware acceptance. Head tracking, relay/multipoint, hearing aid/custom
transparency and high-resolution microphone transport remain experimental and
are not automatically activated by the service.

See [testing](docs/testing.md), [development evidence](docs/automated-evidence.md),
and the [port plan](plans/rust-daemon-port.md). Ordinary tests use isolated buses,
synthetic peers and temporary paths. Hardware-changing runs are separate and
restore captured settings. Short measurements never count as the required
24-hour no-BLE soak.

The source is licensed under [AGPL-3.0-only](Cargo.toml). Protocol and behavioral references
include [LibrePods](https://github.com/kavishdevar/librepods) and the provenance
records accompanying vendored fixtures and compatibility tests.
