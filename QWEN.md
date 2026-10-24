# QWEN.md — LibrePods-rs

> **CRITICAL:** At the start of every conversation, before any code changes, read
> `.github/copilot-instructions.md`. It contains the authoritative, machine-parseable
> coding directives for hot path, memory, concurrency, wire format, testing, and
> determinism. All code must comply with those rules regardless of what is summarized
> below.

This file provides guidance to Qwen Code when working with code in this repository.

## Project Overview

`librepods-rs` is a pure-Rust, clean-room implementation of the Apple Accessory
Protocol (AAP), letting AirPods work as first-class citizens on **Linux and
Windows**. It speaks AAP over Bluetooth L2CAP to read battery levels, control
noise cancellation, detect ear gestures, auto play/pause media, and more — no
Apple device required.

It is inspired by the Kotlin/C++ [LibrePods](https://github.com/kavishdevar/librepods)
project and is designed for embeddability, portability, and **strict crate-boundary
separation**.

- **Language:** Rust (edition 2021)
- **License:** AGPL-3.0-only
- **Workspace:** Cargo virtual workspace with four crates (see below)
- **Targets:** Linux (primary) and Windows, over a shared cross-platform core
- **Portability constraint:** `librepods-core` must compile for any target, including
  `aarch64-linux-android`. This is the litmus test for crate purity.

## Architecture

```
┌──────────────────────────────────────────────────────────────────┐
│  librepods-cli          CLI daemon, state machine,               │
│                         clap, env_logger                         │
│                         picks backend via its `platform` module  │
├───────────────────────────────┬──────────────────────────────────┤
│  librepods-linux              │  librepods-windows               │
│  L2CAP socket (nix, libc)     │  L2CAP (WinSock AF_BTH)          │
│  BlueZ D-Bus (zbus, tokio)    │  WinRT Bluetooth (`windows`)     │
│  BLE scan (btleplug)          │  BLE scan (btleplug)             │
│  Media (wpctl/pactl/playerctl)│  Media (SMTC via WinRT)          │
│  ⚠ Linux-only                 │  ⚠ Windows-only                  │
├───────────────────────────────┴──────────────────────────────────┤
│  librepods-core         AAP parser, packet builder,              │
│                         device state, BLE ad decoder,            │
│                         transport traits                         │
│                         ⚠ NO platform I/O, pure Rust             │
└──────────────────────────────────────────────────────────────────┘
```

### Crate Boundaries (Critical Rule)

The single most important rule in this project: **never move platform I/O into
`librepods-core`, and never put domain/protocol logic into a platform crate
(`librepods-linux`, `librepods-windows`) or `librepods-cli`.** A change that
violates this should be rejected.

| Crate | Allowed dependencies | Forbidden | Purpose |
|---|---|---|---|
| `librepods-core` | `serde`, `thiserror`, `log`, `aes` | Any Bluetooth / OS / async-runtime I/O | Protocol engine, device state, BLE parsing, transport traits |
| `librepods-linux` | `nix`, `zbus`, `btleplug`, `tokio`, `libc`, `futures` | Domain logic, packet parsing | Linux Bluetooth + media I/O |
| `librepods-windows` | `windows`, `windows-sys`, `btleplug`, `tokio` | Domain logic, packet parsing | Windows Bluetooth + media I/O |
| `librepods-cli` | `clap`, `env_logger`, `tokio`, `log` | New protocol types | Headless daemon, state machine, backend selection |

The daemon port adds `librepods-daemon` for Tokio orchestration and IPC. The CLI is a thin client. Device state is updated only through the core reducer/session lifecycle; pure policies receive explicit time. See plans/rust-daemon-port.md.

`librepods-core` defines I/O as traits in `src/transport.rs`
(`L2capTransport`, `DeviceMonitor`, `MediaControl`, `BleScanner`); platform crates
provide the implementations. The core crate contains **zero** implementations.

Each platform crate is gated with `#![cfg(target_os = "…")]` and keeps its
dependencies under `[target.'cfg(target_os = "…")'.dependencies]`, so on the
"wrong" OS it compiles to an empty library and a workspace-wide `cargo build`
still succeeds. `librepods-cli` never names a concrete backend type: it selects
one in its `platform` module via `#[cfg(target_os = "…")]` re-exports aliased to
`Transport` / `Monitor` / `Scanner` / `Media`, and otherwise works through the
`dyn` traits (`&dyn L2capTransport`, `Option<&dyn MediaControl>`, `Box<dyn …>`).

### Crate Layout

```
librepods-core/src/
├── lib.rs
├── transport.rs              # I/O traits + TransportError
├── protocol/
│   ├── mod.rs
│   ├── opcodes.rs            # AAP opcode constants (single source of truth)
│   ├── packets.rs            # Packet constants + builder functions
│   ├── parser.rs             # ParsedPacket enum + parse()
│   └── control_command.rs    # ControlCommand builder + ControlCommandId
├── device/
│   ├── mod.rs
│   ├── battery.rs            # BatteryInfo
│   ├── state.rs              # DeviceState, AirPodsModel, NoiseControlMode
│   ├── ble_advert.rs         # BLE advertisement decoder
│   └── listening_mode.rs
└── crossdevice/              # Cross-device sync protocol
    ├── mod.rs
    ├── packets.rs
    └── relay.rs

librepods-linux/src/
├── lib.rs
├── l2cap.rs                  # Raw AF_BLUETOOTH L2CAP socket via nix
├── bluez_monitor.rs          # BlueZ D-Bus connection events via zbus
├── ble.rs                    # BLE scanning via btleplug
└── media.rs                  # wpctl / pactl + playerctl / MPRIS

librepods-windows/src/
├── lib.rs                    # #![cfg(target_os = "windows")]
├── l2cap.rs                  # WinSock AF_BTH L2CAP socket
├── device_monitor.rs         # WinRT Bluetooth connection events
├── ble.rs                    # BLE scanning via btleplug (WinRT backend)
└── media.rs                  # SMTC (WinRT) playback + default-render check

librepods-cli/src/
└── main.rs                   # CLI, handshake state machine, receive loop, `platform` module
```

### Adding a New Platform Backend

`librepods-core` exposes four traits in `src/transport.rs` —
`L2capTransport`, `DeviceMonitor`, `MediaControl`, `BleScanner`. Every OS crate
implements all four; the CLI selects one at compile time. To add a new OS:

1. Create `librepods-<os>/` with `#![cfg(target_os = "<os>")]` at the crate root.
2. Put all dependencies under `[target.'cfg(target_os = "<os>")'.dependencies]`
   in its `Cargo.toml` so the crate stays empty on other targets.
3. Implement the four traits (use `librepods-windows/src/*.rs` as a template).
4. Add the crate to `[workspace] members` in the root `Cargo.toml`.
5. Add a `#[cfg(target_os = "<os>")] mod platform` block in
   `librepods-cli/src/main.rs` re-exporting the impls as `Transport` / `Monitor`
   / `Scanner` / `Media`.
6. Add `[target.'cfg(target_os = "<os>")'.dependencies]` for the new crate in
   `librepods-cli/Cargo.toml`.
7. Add the OS to the CI matrix in `.github/workflows/rust.yml`.

## Building and Running

### Prerequisites (Ubuntu / Debian)

System build dependencies:

```bash
sudo apt update
sudo apt install -y build-essential pkg-config curl \
    libdbus-1-dev libudev-dev
```

- `libdbus-1-dev` — D-Bus headers, required by `zbus` (BlueZ monitor) and `btleplug`
- `libudev-dev` — udev headers, required by `btleplug` for device enumeration
- `pkg-config` — finds `libdbus` and `libudev` at compile time

Bluetooth stack and (optional) audio tools:

```bash
sudo apt install -y bluetooth bluez
sudo systemctl enable --now bluetooth

# Audio & media control (optional, for auto play/pause)
sudo apt install -y wireplumber playerctl   # PipeWire (Ubuntu 22.04+)
# or: sudo apt install -y pulseaudio-utils  # PulseAudio
```

### Build

```bash
cargo build                      # build all crates
cargo build --release -p librepods-cli
```

### Run

Bluetooth L2CAP sockets require `CAP_NET_RAW`. Either set capabilities on the
binary (recommended) or run with `sudo`:

```bash
sudo setcap cap_net_raw,cap_net_admin+eip ./target/release/librepods-cli

# Auto-detect connected AirPods via BlueZ, or pass an address explicitly
RUST_LOG=info ./target/release/librepods-cli
RUST_LOG=info ./target/release/librepods-cli --address AA:BB:CC:DD:EE:FF

# Scan nearby AirPods via BLE advertisements (no pairing required)
./target/release/librepods-cli --scan
```

### Run on Windows (PowerShell)

No extra C libraries are needed — the Windows backend uses built-in WinRT and
WinSock APIs. Requires the MSVC Rust toolchain and AirPods already paired in
Windows Settings → Bluetooth & devices.

```powershell
cargo build --release -p librepods-cli

$env:RUST_LOG="info"
.\target\release\librepods-cli.exe                              # auto-detect
.\target\release\librepods-cli.exe --address AA:BB:CC:DD:EE:FF  # explicit
.\target\release\librepods-cli.exe --scan                       # BLE scan
```

No `CAP_NET_RAW` / `setcap` step is required. If a Bluetooth policy restricts
socket access, run the terminal as Administrator.

### CLI Options

```
-a, --address <ADDRESS>  Bluetooth address (e.g. "AA:BB:CC:DD:EE:FF")
    --retries <RETRIES>  Maximum connection retry attempts [default: 3]
-s, --scan               Scan for nearby AirPods via BLE advertisements
    --no-media           Disable media control (no auto play/pause)
-h, --help               Print help
```

### Development Run

```bash
sudo RUST_LOG=debug cargo run -p librepods-cli -- --address AA:BB:CC:DD:EE:FF
```

## Testing

```bash
cargo test                                                      # all workspace tests (host OS)
cargo test -p librepods-core                                    # core only (must pass on any target)
cargo check -p librepods-core --target aarch64-linux-android    # cross-platform sanity check

# Verify a platform backend compiles without booting that OS:
cargo check -p librepods-windows --target x86_64-pc-windows-msvc
cargo check -p librepods-cli     --target x86_64-pc-windows-msvc
```

- Tests are inline `#[cfg(test)] mod tests` in each module.
- Parser tests use raw byte arrays matching real packet captures.
- Every `ParsedPacket` variant must have at least one parse round-trip test.
- `cargo check` for the Windows target validates types/APIs but does not link;
  the real build + link runs in CI on `windows-latest`.

CI (`.github/workflows/rust.yml`) runs `cargo build --verbose` and
`cargo test --verbose` on pushes and PRs to `master` for **both `ubuntu-latest`
and `windows-latest`** (matrix, `fail-fast: false`). The `pkg-config` /
`libdbus-1-dev` install step is skipped on Windows.

## Development Conventions

### Required File Header

Every Rust source file starts with:

```rust
// LibrePods - AirPods liberated from Apple's ecosystem
// Copyright (C) 2025 LibrePods contributors
// SPDX-License-Identifier: AGPL-3.0-only
```

### Style

- Typed enums for all wire values (`#[repr(u8)]` + `from_byte()` + `as_byte()`).
- Explicit byte-offset comments on packet layouts.
- Small pure functions, unit tested in the same module.
- `log::debug!` for packet hex dumps, `log::info!` for state changes.
- `thiserror` for all error types — no ad-hoc error strings.
- `serde` derive for state/config types.
- **Never** `unwrap()` on packet parsing or wire data. Unknown/future opcodes
  return `ParsedPacket::Unknown(Vec<u8>)`; never panic.

### Auto-Reject Checklist

```
❌ Platform / OS crate in librepods-core (nix, zbus, btleplug, tokio I/O, windows)
❌ Protocol logic in a platform crate (librepods-linux, librepods-windows) or librepods-cli
❌ Missing AGPL-3.0-only header on new Rust source files
❌ New packet constant defined outside protocol/packets.rs
❌ ParsedPacket variant that silently drops data → use ParsedPacket::Unknown
❌ State mutation outside apply_to_state() in librepods-cli/src/main.rs
❌ Hardcoded audio backend assumption (wpctl-only OR pactl-only)
❌ Platform crate missing #![cfg(target_os = "…")] or with a non-target-gated dependency
❌ unwrap() on packet parsing / wire data
```

Priority when making trade-offs:
`Crate purity > Protocol correctness > Packet completeness > Feature count`.
A parser that returns `Unknown` is correct; a parser that panics on bad wire
data is **not**.

## AAP Protocol Reference

The first two bytes distinguish the packet families. All AAP multi-byte values
are **little-endian**; BLE model IDs are **big-endian** (Apple convention).

### Standard Data Packet

```
04 00 04 00 [opcode] 00 [data …]
```

The `HEADER` is `[0x04, 0x00, 0x04, 0x00]`. Byte 4 is the dispatch key; byte 5
is always `0x00` for known opcodes. The parser validates the 4-byte header, then
dispatches on `packet[4]`.

### Handshake / ACK Packets (different first bytes)

- **Handshake (client → AirPods, 16 bytes):** `00 00 04 00 01 00 02 00 00 …`
- **Handshake ACK (AirPods → client):** `01 00 04 00 …`
- **Features ACK (AirPods → client):** `04 00 04 00 2B 00 …`

The parser checks the Handshake ACK / Features ACK prefixes **before** standard
header validation.

### Control Command (sub-type of standard data, 11 bytes fixed)

```
04 00 04 00 09 00 [id] [d1] [d2] [d3] [d4]
```

Build these with `ControlCommand`, never by hand:

```rust
// ✅ CORRECT — use the builder
ControlCommand::create(0x0D, &[0x02, 0x00, 0x00, 0x00])  // noise mode
ControlCommand::enabled(0x28)                              // enable CA
ControlCommand::disabled(0x28)                             // disable CA

// ❌ NEVER — hand-craft 11 bytes
let pkt = [0x04, 0x00, 0x04, 0x00, 0x09, 0x00, 0x0D, ...];
```

### Handshake Sequence (Strict Order)

```
Client  → AirPods : HANDSHAKE                (00 00 04 00 …)
AirPods → Client  : Handshake ACK            (01 00 04 00 …)
Client  → AirPods : SET_SPECIFIC_FEATURES    (04 00 04 00 4D 00 …)
AirPods → Client  : Features ACK             (04 00 04 00 2B 00 …)
Client  → AirPods : REQUEST_NOTIFICATIONS    (04 00 04 00 0F 00 …)
   ── receive loop: Battery / Ear / Noise / Metadata / … ──
```

Skipping or reordering a step causes the AirPods to silently ignore all
subsequent packets.

### Wire Value Conventions

- Battery status bytes: `0x01` = charging, `0x02` = discharging, `0x04` = disconnected
- Ear detection: `0x00` = in ear, non-zero = out
- `NoiseControlMode`: `0x01` Off, `0x02` NoiseCancellation, `0x03` Transparency, `0x04` Adaptive

## Adding a New Packet Type

1. Add the opcode constant → `protocol/opcodes.rs`
2. Add the prefix constant → `protocol/packets.rs`
3. Add a `ParsedPacket` variant → `protocol/parser.rs`
4. Add the match arm in `parse()` → `protocol/parser.rs`
5. Add state handling in `apply_to_state()` → `librepods-cli/src/main.rs`
6. Add a unit test with raw bytes in the same module

Unknown / future opcodes must map to `ParsedPacket::Unknown(Vec<u8>)`. Never panic.

## Media Control Conventions

All playback goes through the `MediaControl` trait; each platform crate
implements it.

- **Linux:** detect the audio backend at runtime — `wpctl` (PipeWire) →
  `pactl` (PulseAudio) → fallback; playback via `playerctl` → D-Bus MPRIS
  fallback. Never assume which backend is present.
- **Windows:** playback via SMTC (`Windows.Media.Control`); routing is automatic,
  so activation/removal are no-ops; `is_airpods_active_sink()` queries the default
  audio render endpoint via `Windows.Media.Devices`. Volume control is not
  implemented yet and returns an error.
- `is_airpods_active_sink()` must be checked before auto play/pause.

## Linux Permissions

```bash
# Raw Bluetooth sockets require CAP_NET_RAW
sudo setcap cap_net_raw,cap_net_admin+eip ./target/release/librepods-cli

# Or run the daemon with sudo during development
sudo RUST_LOG=debug cargo run -p librepods-cli -- --address AA:BB:CC:DD:EE:FF
```

> On Windows no capability is needed — run as a normal user (Administrator only
> if a Bluetooth policy blocks socket access).

## Device Compatibility

| Model | Status |
|---|---|
| AirPods Pro 2 (USB-C) | ✅ Tested (primary dev device) |
| AirPods Pro 2 (Lightning) | ✅ Tested |
| AirPods Pro / Max / 3rd–4th gen / 1st–2nd gen | ⚠️ Expected |

The AAP protocol is consistent across all AirPods models. Noise control and
adaptive transparency only work on models with the hardware for it.

The same AAP protocol runs over both backends — only the L2CAP and media
transport layers differ between Linux and Windows.

## Troubleshooting Quick Reference

**Linux**

- **`Permission denied` on connect** → set `CAP_NET_RAW` capabilities or use `sudo`.
- **`AF_BLUETOOTH not supported`** → `sudo modprobe bluetooth && sudo systemctl restart bluetooth`.
- **`No BLE adapter found` (with `--scan`)** → check `hciconfig -a`, bring adapter up, check `lsusb` / `dmesg`.
- **`No audio backend found`** → install `wireplumber` (PipeWire) or `pulseaudio-utils` (PulseAudio).
- **Connected but no data** → re-pair the device in `bluetoothctl` (remove / scan / pair / trust / connect).

**Windows**

- **`failed to create L2CAP socket … Winsock error …`** → ensure the Bluetooth adapter is enabled; try an Administrator terminal.
- **`L2CAP connect … failed`** → confirm the AirPods are paired, connected and not in use by another device; verify the address.
- **Not auto-detected** → pass `--address` explicitly (auto-detection matches a device name containing `"AirPods"`).
- **No media pause/resume** → ensure a media session is active (SMTC); volume control is not implemented yet.

## VendorID Configuration

Some features (Hearing Aid, Customize Transparency Mode, Multi-device
connectivity) require the host to identify as an Apple device. On Linux, set
`DeviceID = bluetooth:004C:0000:0000` in the `[General]` section of
`/etc/bluetooth/main.conf`, then `sudo systemctl restart bluetooth`.

> **Caution:** This changes how the machine identifies to *all* Bluetooth devices.
> Revert it if other devices misbehave.

## Credits

Built on reverse-engineering research by the
[LibrePods](https://github.com/kavishdevar/librepods) project
([@kavishdevar](https://github.com/kavishdevar)), [@tyalie](https://github.com/tyalie)
(initial AAP protocol documentation), [@rithvikvibhu](https://github.com/rithvikvibhu)
and the Lagrange Point community (hearing aid research), and
[@timgromeyer](https://github.com/timgromeyer) (first LibrePods Linux app).

*AirPods is a trademark of Apple Inc. This project is not affiliated with,
endorsed by, or sponsored by Apple Inc.*
