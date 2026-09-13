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

These are gaps and risks that were **not** applied, with the evidence gathered for each. Priorities are `impact × effort`; nothing here is a crash or a wire-format regression — all unknown inputs already degrade to `ParsedPacket::Unknown` / `CrossDeviceAction::Unknown` without panicking.

> **Implementation status (2026-09-13):** C1, C2, C5, C6, C7, C9 and C10 addressed; C3 documented in code; C4 left unchanged by design; C8 deferred. See the Status column.

### C-triage

| # | Item | Impact | Effort | Priority | Status |
|---|---|---|---|---|---|
| C9 | Entire `crossdevice` subsystem unused by the app; upstream reference absent | Dead subsystem, unverifiable constants | M (decide keep/remove) | **P1 (decision)** | ✅ Experimental notice added |
| C1 | `ControlCommandId` missing 9 upstream ids | Events map to `Unknown`, less typed logging | S (additive) | P2 | ✅ Added |
| C2 | `opcodes.rs` missing ~9 upstream opcodes | These events map to `Unknown` | S (additive) | P2 | ✅ Constants added |
| C10 | `HEAD_TRACKING_START/STOP` never sent | Head-tracking feature may never activate | S–M (device test) | P2 | ✅ Wired (needs device test) |
| C3 | `REQUEST_BATTERY` / `REQUEST_ANC` / `DATA_HEADER` unverified | Possible wrong cross-device behavior | M (needs source) | P2 | 📝 Documented |
| C4 | Receive loop assumes `1 recv() = 1 packet` | Fragile if a backend streams/coalesces | M | P3 | ⏸ No change (by design) |
| C7 | Cluster of unused `pub const`s | Hygiene / misleading API surface | S | P3 | ✅ Annotated |
| C5 | `which_exists()` shells out to `which` | Extra runtime dependency | S | P3 | ✅ Fixed |
| C6 | Windows errors lose their `source` | Weaker diagnostics | S | P3 | ✅ Fixed |
| C8 | Comment refs to non-existent `docs/*` | Slightly misleading | S | P4 | ⏸ Deferred |

### C1 — `ControlCommandId` is missing 9 upstream identifiers
- **File:** `librepods-core/src/protocol/control_command.rs`
- **Missing (upstream `AACPManager.kt` `ControlCommandIdentifiers`):** `0x06 OWNS_CONNECTION`, `0x0A EAR_DETECTION_CONFIG`, `0x20 AUTOMATIC_CONNECTION_CONFIG`, `0x35 SLEEP_DETECTION_CONFIG`, `0x36 ALLOW_AUTO_CONNECT`, `0x37 PPE_TOGGLE_CONFIG`, `0x38 PPE_CAP_LEVEL_CONFIG`, `0x39 STEM_CONFIG`, `0x3B DYNAMIC_END_OF_CHARGE`.
- **Behavior today:** `parse_control_command()` logs `Unknown control command identifier` and returns `ParsedPacket::Unknown` — safe, but the event is dropped rather than surfaced as a typed `ControlCommand`.
- **Recommendation:** additive enum + `from_byte` arms; low risk. Optionally add `apply_to_state` handling for the ones with a known bool/state (`0x06`, `0x0A`, `0x20`).

### C2 — `opcodes.rs` is missing ~9 upstream opcodes
- **File:** `librepods-core/src/protocol/opcodes.rs`
- **Missing (upstream `AACPManager.kt` `Opcodes`):** `0x0E AUDIO_SOURCE`, `0x10 SMART_ROUTING`, `0x11 SMART_ROUTING_RESP`, `0x14 SEND_CONNECTED_MAC`, `0x19 STEM_PRESS`, `0x2E CONNECTED_DEVICES`, `0x53 HEADPHONE_ACCOMMODATION`, `0x63 CUSTOM_EQ` (plus the redundant `0x0C`).
- **Behavior today:** fall through to `ParsedPacket::Unknown` and are only `log::debug!`-ed.
- **Recommendation:** add the constants first (single source of truth, zero risk); add parser variants only with raw-byte round-trip tests, per the QWEN.md rule. `0x19 STEM_PRESS` is the highest-value one (user stem gestures).

### C3 — Cross-device constants cannot be verified against upstream
- **File:** `librepods-core/src/crossdevice/packets.rs`
- **Finding:** `REQUEST_BATTERY` (`00 02 00 01`), `REQUEST_ANC` (`00 02 00 02`) and `DATA_HEADER` (`00 04 00 01`) are **not** in `airpods_packets.h`, which only defines `Phone::NOTIFICATION`, `CONNECTED`, `DISCONNECTED`, `STATUS_REQUEST`, `DISCONNECT_REQUEST`. A full walk of the upstream tree found **no** `CrossDevice.kt` / `main.cpp` (the files cited by this module's comments), so the provenance of these three constants is untraceable in the current upstream branch.
- **Impact:** unverified; could be wrong, but currently harmless because nothing consumes them (see C9).
- **Recommendation:** either locate the original reference (older branch/tag/PR) or mark the module experimental.

### C4 — Receive loop assumes one datagram per `recv()`
- **File:** `librepods-cli/src/main.rs:353,371,392` (`let mut buf = [0u8; 1024];` + one `recv` per iteration)
- **Finding:** the loop treats one `recv()` as exactly one AAP packet. This is currently **correct** because both backends use `SOCK_SEQPACKET` (`nix::unistd::read` on Linux, `ws_recv` on Windows), which preserves message boundaries — one `read` returns one datagram. The 1024-byte buffer is comfortably larger than any known packet (largest is `HEADPHONE_ACCOMMODATION` at 140 bytes; head-tracking ≈70).
- **Risk:** if a future backend returns a partial or coalesced read, `parse()` would see a truncated/garbage packet and the loop would `log::warn!` and drop it — degrading silently rather than buffering.
- **Recommendation:** no change required for the current two backends; add a length-prefixed/streaming framing layer only if a byte-stream backend is ever added.

### C5 — `which_exists()` shells out to `which`
- **File:** `librepods-linux/src/media.rs` (`Command::new("which")`)
- **Impact:** the daemon depends on the `which` binary being present; on minimal images it may be missing, silently disabling backend detection.
- **Recommendation:** resolve the executable against `$PATH` in pure Rust (or use an existing crate already in the tree).

### C6 — Windows transport errors discard the OS error source
- **File:** `librepods-windows/src/l2cap.rs`
- **Finding:** `socket()`/`connect()`/`send()`/`recv()` failures use `TransportError::new(format!("… Winsock error {err}"))`, which is `TransportError::Other(String)` — the raw Winsock error is embedded in the string and not exposed as `#[source]`.
- **Recommendation:** use `TransportError::with_source` (or a dedicated `Winsock(i32)` variant) so callers can match on the code. Diagnostics only.

### C7 — Cluster of unused `pub const`s
- **Files:** `librepods-core/src/protocol/packets.rs`, `opcodes.rs`, `crossdevice/packets.rs`
- **Evidence (repo-wide grep, definitions with no other reference):** `AIRPODS_DISCONNECTED`, `EAR_DETECTION_PREFIX`, `REQUEST_MAGIC_CLOUD_KEYS`, `HEAD_TRACKING_START`, `HEAD_TRACKING_STOP`, `crossdevice::NOTIFICATION`, and the opcode constants `opcodes::REQUEST_NOTIFICATIONS` / `opcodes::RENAME` / `opcodes::SET_FEATURE_FLAGS` (the *packet* constant is used, the *opcode* one is not).
- **Why clippy is silent:** they are `pub` items of a library, so `dead_code` does not fire.
- **Recommendation:** keep as documented API surface, or gate with `#[allow(dead_code)]`/`#[doc(hidden)]`. Note `HEAD_TRACKING_START/STOP` being unused ties into C10.

### C8 — Comments reference files that do not exist in this repo
- **Finding:** module docs cite `docs/control_commands.md`, `AAP Definitions.md`, `CrossDevice.kt`, `main.cpp`, `BasicControlCommand.hpp`, etc. None are vendored here; they live in the upstream reference.
- **Recommendation:** prefix such references with the upstream repo URL so readers can follow them.

### C9 — The entire `crossdevice` subsystem is unused by the application
- **Files:** `librepods-core/src/crossdevice/{mod,packets,relay}.rs`
- **Evidence:** the only references to `handle_packet`/`CrossDeviceAction`/`crossdevice::packets::*` are inside the module's own tests; `librepods-cli` and both platform crates never import `crossdevice`. It is also the home of the unverifiable constants in C3.
- **Impact:** ~165 lines + tests that are compiled and maintained but never exercised in production. No runtime risk (it cannot be reached), but it inflates the "implemented features" surface and its upstream provenance is missing (see C3).
- **Recommendation:** make an explicit product decision — either (a) wire it into the CLI/platform crates as a real cross-device/relay feature, or (b) mark the module clearly as experimental/unsupported (or remove it) so it is not mistaken for working code. This is the highest-impact item in this section because it is about *direction*, not a missing enum arm.

### C10 — Head-tracking START/STOP are never sent
- **Files:** `librepods-core/src/protocol/packets.rs` (`HEAD_TRACKING_START`, `HEAD_TRACKING_STOP`), `librepods-cli/src/main.rs`
- **Finding:** the CLI parses inbound head-tracking sensor data and `is_head_tracking_data()` is used to suppress its logging, but the outgoing START/STOP packets are never sent by any code path (see C7).
- **Impact:** the README advertises "receive raw spatial audio head tracking sensor data"; whether the AirPods emit it without an explicit START is unverified. If START is required, the feature is inert.
- **Recommendation:** confirm on a real device; if required, send `HEAD_TRACKING_START` after the handshake (and `HEAD_TRACKING_STOP` on disconnect), reusing the existing constants.

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
