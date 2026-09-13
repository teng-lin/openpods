# Audit — `librepods-rs`

- **Date:** 2026-09-13
- **Scope:** entire workspace (`librepods-core`, `librepods-linux`, `librepods-windows`, `librepods-cli`) + CI + documentation
- **Method:** static read of the whole codebase, checked against the conventions in `.github/copilot-instructions.md` & `QWEN.md`, ran build/test/lint/cross-target, and compared the protocol against the upstream reference (`kavishdevar/librepods` — `AACPManager.kt`, `Packets.kt`, `airpods_packets.h`, `enums.h`, `AAP Definitions.md`) only where a behavior was in doubt.
- **Fix threshold:** confirmed bugs + safe nits. Anything not confirmed was **reported**, not changed.

## Baseline before fixes

| Check | Result |
|---|---|
| `cargo build` | ✅ |
| `cargo test` | ✅ 22 core tests + 1 doctest |
| `cargo clippy --all-targets` | ✅ clean |
| `cargo fmt --check` | ❌ 1 diff (`parser.rs`) |
| `cargo check -p librepods-core --target aarch64-linux-android` | ✅ |

After fixes: 32 core tests + 1 doctest, and `fmt`/`clippy` (host + Windows target)/Android check all green.

---

## A. Confirmed bugs fixed

### A1 — `opcodes::RENAME` had the wrong wire value
- **File:** `librepods-core/src/protocol/opcodes.rs:37`
- **Before:** `pub const RENAME: u8 = 0x1E;` → **After:** `0x1A`.
- **Evidence:** upstream `AACPManager.kt` `Opcodes.RENAME = 0x1A`. The constant was previously **unused** anywhere (`build_rename` hardcodes the correct `0x1A`), so the fix changes no behavior but removes a latent source of drift.
- **Status:** ✅ Fixed.

### A2 — Wrong feature-flag byte in `SET_SPECIFIC_FEATURES`
- **File:** `librepods-core/src/protocol/packets.rs:46`
- **Before:** byte 6 = `0xFF` → **After:** `0xD7`.
- **Evidence:** `AACPManager.kt` `createSetFeatureFlagsPacket()` uses `data = [0xD7, 0, 0, 0, 0, 0, 0, 0]`; C++ `airpods_packets.h` `SET_SPECIFIC_FEATURES = "040004004d00d700000000000000"`. Two independent sources agree → confirmed bug.
- **Risk:** this is a wire-critical packet in the handshake. If your physical device proves otherwise, reverting is a one-byte change. `REQUEST_NOTIFICATIONS` (10 bytes) was checked and matches Kotlin, so it was **not** changed.
- **Status:** ✅ Fixed.

### A3 — Missing `ParsedPacket` test coverage (violates QWEN.md)
- **File:** `librepods-core/src/protocol/parser.rs` (`tests` module)
- **Problem:** QWEN.md requires *every* `ParsedPacket` variant to have at least one round-trip test; previously only 5 of ~13 variants were tested.
- **Added tests for:** `ConversationalAwareness`, `ControlCommand`, `Unknown` (unknown control id + unknown opcode), `Metadata`, `HeadTracking`, `ProximityKeys`, `MagicCloudKeys`, and the two-key fallback case. Also added a `BatteryStatus::OptimizedCharging` test in `battery.rs`.
- **Status:** ✅ Fixed.

### A4 — magic-keys / proximity-keys prefix ambiguity
- **File:** `librepods-core/src/protocol/parser.rs:150`
- **Problem:** `MAGIC_CLOUD_KEYS_HEADER = [04 00 04 00 31 00 02]` matches a `PROXIMITY_KEYS_RSP` with `count == 2`, and was handled **before** opcode dispatch. If a two-key payload did not match the strict IRK+EncKey layout, the old parser returned an error and **swallowed** the packet instead of trying the generic parser.
- **Fix:** `parse_magic_cloud_keys(packet).or_else(|_| parse_proximity_keys(packet))` — try the strict layout first, fall back safely.
- **Status:** ✅ Fixed (covered by `two_key_response_falls_back_to_proximity_keys`).

### A5 — `BatteryStatus` missing `OptimizedCharging` (0x05)
- **File:** `librepods-core/src/device/battery.rs:66`
- **Problem:** status `0x05` (optimized charging) was mapped to "Disconnected" by `unwrap_or(Disconnected)`.
- **Evidence:** upstream `Packets.kt` `BatteryStatus`: `CHARGING=1, NOT_CHARGING=2, DISCONNECTED=4, OPTIMIZED_CHARGING=5`.
- **Status:** ✅ Fixed (added variant + `from_byte` arm + test).

### A6 — Panic on mutex poisoning in media (Linux)
- **File:** `librepods-linux/src/media.rs:99`
- **Problem:** three `.lock().unwrap()` calls could panic if the mutex were poisoned, taking down the daemon.
- **Fix:** added a `sink_id_cache()` helper using `unwrap_or_else(|p| p.into_inner())` (recover instead of panic); switched all three call sites to it.
- **Status:** ✅ Fixed.

### A7 — CI missing a dependency and quality gates
- **File:** `.github/workflows/rust.yml`
- **Problem:** (1) missing `libudev-dev`, which `btleplug` needs on Linux (README/QWEN.md both list it); (2) no `fmt`/`clippy`; (3) no `aarch64-linux-android` check (the project's litmus test).
- **Fix:** added `libudev-dev`; added `cargo fmt --all -- --check`, `cargo clippy --all-targets -- -D warnings`, and the Android cross-check (Linux only). Clippy runs on both OSes so it covers `librepods-linux` and `librepods-windows` respectively.
- **Status:** ✅ Fixed.

### A8 — `ble_advert.rs`: dead code + misleading return type
- **File:** `librepods-core/src/device/ble_advert.rs:267`
- **Problem:** `let _ = &mut advert; // suppress unused mut warning` existed only to hide an unnecessary `mut`; `decrypt_battery` returned `Option<()>` but **never** returned `None` (the doc claimed the opposite).
- **Fix:** removed the `mut` + hack; `decrypt_battery` now returns `()` and the doc was corrected.
- **Status:** ✅ Fixed (the function has no caller in the repo).

### A9 — `DATA_HEADER` shares its wire value with `NOTIFICATION`
- **File:** `librepods-core/src/crossdevice/packets.rs:48`
- **Problem:** `DATA_HEADER` and `NOTIFICATION` are both `00 04 00 01`. Upstream `airpods_packets.h` defines only `Phone::NOTIFICATION` under that value and no separate relay header.
- **Fix:** documented the aliasing relationship in the doc comment (value left unchanged since no distinct relay header could be identified).
- **Status:** ✅ Fixed (doc).

---

## B. Reviewed — no change needed

### B1 — `ConversationalAwareness { status }` is not written to state (`main.rs:485`)
- Per upstream, packet `0x4B` carries the CA **level** (e.g. `01/02` started speaking, `03` stopped speaking, `08/09` normal), **not** an on/off flag. `DeviceState.conversational_awareness` is correctly set from control command `0x28` (`main.rs:496`), matching upstream. Therefore only logging the `0x4B` level and **not** overwriting the bool is **correct**.
- **Status:** reported only, unchanged.

### B2 — `is_head_tracking_data` does not match `HEAD_TRACKING_START/STOP`
- **File:** `librepods-core/src/protocol/packets.rs`
- `is_head_tracking_data` requires `byte[10] ∈ {0x44,0x45}` (matches upstream `Packets.kt`), while START/STOP have `byte[10]=0x10/0x11`. This is **by design**: START/STOP are **outgoing** packets and never pass through `is_head_tracking_data`. The parser's `≥70`-byte threshold also matches upstream (`packet.size < 70` → reject).
- **Status:** reported only (the `HeadTracking` doc was fixed to match: it returns the whole packet, not the payload).

### B3 — `MAGIC_CLOUD_KEYS_HEADER` is specific to this port
- Upstream has no separate "magic cloud keys" concept; it only has the generic proximity-keys parser. Splitting out the two-key IRK+EncKey branch is a **Rust-port addition** (still valid). Robustness was increased via the fallback (A4); the design is otherwise unchanged.
- **Status:** reported only.

### B4 — Other round-trips & layouts verified against upstream
- Handshake (16 bytes), `REQUEST_NOTIFICATIONS` (10 bytes, `0x0F`), battery (components `02/04/08`, spacer/end `0x01`), ear detection (`0x06`), control commands (`0x09`, ids `0x0D/0x28/0x2E/0x1A`), and the general opcode map (`0x04/06/09/0F/17/1D/30/31/4B/4D`) — all **match**.
- `NoiseControlMode` (`01/02/03/04`) matches both `AAP Definitions.md` and `Packets.kt`.
- Model-number map (`model_from_number`) matches `enums.h` `parseModelNumber()`.
- **Status:** ✅ compatible.

---

## C. Suggestions / backlog (no code change)

| # | Area | Note |
|---|---|---|
| C1 | `ControlCommandId` | Missing several upstream ids: `0x06 OWNS_CONNECTION`, `0x0A EAR_DETECTION_CONFIG`, `0x20 AUTOMATIC_CONNECTION_CONFIG`, `0x35 SLEEP_DETECTION_CONFIG`, `0x36 ALLOW_AUTO_CONNECT`, `0x37 PPE_TOGGLE_CONFIG`, `0x38 PPE_CAP_LEVEL_CONFIG`, `0x39 STEM_CONFIG`, `0x3B DYNAMIC_END_OF_CHARGE`. They currently fall through to `Unknown` (safe, no panic). |
| C2 | `opcodes.rs` | Missing upstream opcodes: `0x0E AUDIO_SOURCE`, `0x10 SMART_ROUTING`, `0x11 SMART_ROUTING_RESP`, `0x14 SEND_CONNECTED_MAC`, `0x19 STEM_PRESS`, `0x2E CONNECTED_DEVICES`, `0x53 HEADPHONE_ACCOMMODATION`, `0x63 CUSTOM_EQ`. → `Unknown`. |
| C3 | `crossdevice/packets.rs` | `REQUEST_BATTERY` (`00 02 00 01`) and `REQUEST_ANC` (`00 02 00 02`) are **not** in `airpods_packets.h`; neither is `DATA_HEADER` → source unverified; the cross-device reference code still needs to be located. |
| C4 | `librepods-cli/src/main.rs` | The receive loop assumes `1 recv() = 1 packet`. On Linux `SOCK_SEQPACKET` preserves message boundaries so it is currently fine, but if a backend ever returns a coalesced or partial packet the parse breaks. Add buffering/framing if more backends are supported. |
| C5 | `librepods-linux/src/media.rs` | `which_exists()` shells out to `which`; could be replaced with a pure-Rust `$PATH` lookup to drop the external dependency. |
| C6 | `librepods-windows/*` | `TransportError::new` for Winsock errors drops the `source` (string only); consider `with_source`. Volume control is not implemented (already noted in the README). |
| C7 | Whole crate | Several unused `pub const`s (`AIRPODS_DISCONNECTED`, `EAR_DETECTION_PREFIX`, `REQUEST_MAGIC_CLOUD_KEYS`, `opcodes::SET_FEATURE_FLAGS`) — they are API surface, so clippy is silent. Consider documenting them or marking `#[allow(dead_code)]`. |
| C8 | Comments | References to `docs/control_commands.md` and `AAP Definitions.md` point at upstream and do not exist in this repo — worth stating the upstream path explicitly. |

---

## D. Compliance checklist (passing)

- ✅ **Crate boundaries:** `librepods-core` uses only `serde`, `thiserror`, `log`, `aes`; no platform I/O. Both platform crates have `#![cfg(target_os = "…")]` + target-gated dependencies. The CLI selects a backend via `mod platform` with `#[cfg]`.
- ✅ **Litmus test:** `cargo check -p librepods-core --target aarch64-linux-android` passes.
- ✅ **AGPL-3.0 header:** 26/26 `.rs` files carry the 3-line header.
- ✅ **No `unwrap()` on wire data** outside tests (remaining `unwrap`s are in tests, plus the mutex — handled in A6).
- ✅ **State mutation** happens only inside `apply_to_state()` (`librepods-cli/src/main.rs`).
- ✅ Wire enums all have `#[repr(u8)]` + `from_byte()`/`as_byte()`; packet builders go through `ControlCommand`.
- ✅ No `panic!`/`todo!`/`unimplemented!` in production code (tests only).

## E. Verification after fixes

| Command | Result |
|---|---|
| `cargo test` (workspace) | ✅ 32 core + 1 doctest |
| `cargo clippy --all-targets -- -D warnings` | ✅ clean |
| `cargo clippy -p librepods-windows -p librepods-cli --target x86_64-pc-windows-msvc` | ✅ clean |
| `cargo fmt --all -- --check` | ✅ clean |
| `cargo check -p librepods-core --target aarch64-linux-android` | ✅ |

## F. Reference sources used for verification

- `AACPManager.kt` — `Opcodes`, `createSetFeatureFlagsPacket`, `parseProximityKeysResponse`, `ControlCommandIdentifiers`.
- `Packets.kt` — `BatteryStatus`, `BatteryComponent`, head-tracking prefix pattern.
- `airpods_packets.h` — `Connection::*`, `Phone::*`.
- `enums.h` — `parseModelNumber()`.
- `docs/AAP Definitions.md` — battery/control-command/CA layouts.

*(The files above belong to the reference repo `kavishdevar/librepods` and are not part of this repo.)*
