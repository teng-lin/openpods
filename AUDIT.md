# Audit — `librepods-rs`

- **Ngày:** 2026-09-13
- **Phạm vi:** toàn bộ workspace (`librepods-core`, `librepods-linux`, `librepods-windows`, `librepods-cli`) + CI + tài liệu
- **Phương pháp:** đọc tĩnh toàn bộ mã nguồn, đối chiếu quy ước trong `.github/copilot-instructions.md` & `QWEN.md`, chạy build/test/lint/cross-target, và đối chiếu protocol với tham chiếu upstream (`kavishdevar/librepods` — `AACPManager.kt`, `Packets.kt`, `airpods_packets.h`, `enums.h`, `AAP Definitions.md**) chỉ khi có nghi vấn.
- **Ngưỡng sửa:** lỗi chắc chắn + nit an toàn. Việc không đủ chắc chắn được **ghi nhận**, không đổi code.

## Baseline trước khi sửa

| Kiểm tra | Kết quả |
|---|---|
| `cargo build` | ✅ |
| `cargo test` | ✅ 22 test core + 1 doctest |
| `cargo clippy --all-targets` | ✅ sạch |
| `cargo fmt --check` | ❌ 1 diff (`parser.rs`) |
| `cargo check -p librepods-core --target aarch64-linux-android` | ✅ |

Sau khi sửa: 32 test core + 1 doctest, `fmt`/`clippy` (host + Windows target)/Android check đều xanh.

---

## A. Lỗi chắc chắn đã sửa

### A1 — `opcodes::RENAME` sai giá trị wire
- **File:** `librepods-core/src/protocol/opcodes.rs:37`
- **Trước:** `pub const RENAME: u8 = 0x1E;` → **Sau:** `0x1A`.
- **Bằng chứng:** upstream `AACPManager.kt` `Opcodes.RENAME = 0x1A`. Hằng số này trước đây **không được dùng** ở đâu (`build_rename` hardcode đúng `0x1A`), nên sửa không đổi hành vi nhưng loại bỏ nguồn sai lệch tiềm ẩn.
- **Trạng thái:** ✅ Đã sửa.

### A2 — Byte feature-flag của `SET_SPECIFIC_FEATURES` sai
- **File:** `librepods-core/src/protocol/packets.rs:46`
- **Trước:** byte 6 = `0xFF` → **Sau:** `0xD7`.
- **Bằng chứng:** `AACPManager.kt` `createSetFeatureFlagsPacket()` dùng `data = [0xD7, 0, 0, 0, 0, 0, 0, 0]`; C++ `airpods_packets.h` `SET_SPECIFIC_FEATURES = "040004004d00d700000000000000"`. Hai nguồn độc lập trùng khớp → lỗi chắc chắn.
- **Rủi ro:** đây là gói wire-critical trong handshake. Nếu thiết bị thực tế của bạn kiểm chứng ngược lại, chỉ cần hoàn nguyên 1 byte. `REQUEST_NOTIFICATIONS` (10 byte) đã đối chiếu khớp Kotlin nên **không** đổi.
- **Trạng thái:** ✅ Đã sửa.

### A3 — Phủ kiểm thử `ParsedPacket` thiếu (vi phạm QWEN.md)
- **File:** `librepods-core/src/protocol/parser.rs` (module `tests`)
- **Vấn đề:** QWEN.md yêu cầu *mỗi* `ParsedPacket` variant có ≥1 test round-trip; trước đây chỉ 5/~13 variant được test.
- **Đã thêm test cho:** `ConversationalAwareness`, `ControlCommand`, `Unknown` (control id lạ + opcode lạ), `Metadata`, `HeadTracking`, `ProximityKeys`, `MagicCloudKeys`, và ca fallback 2-khóa. Thêm test `BatteryStatus::OptimizedCharging` ở `battery.rs`.
- **Trạng thái:** ✅ Đã sửa.

### A4 — Nhập nhằng prefix magic-keys / proximity-keys
- **File:** `librepods-core/src/protocol/parser.rs:150`
- **Vấn đề:** `MAGIC_CLOUD_KEYS_HEADER = [04 00 04 00 31 00 02]` khớp với một `PROXIMITY_KEYS_RSP` có `count == 2` và được xử lý **trước** dispatch opcode. Nếu payload 2 khóa không đúng layout IRK+EncKey, parser cũ trả lỗi và **nuốt** packet thay vì thử parser tổng quát.
- **Sửa:** `parse_magic_cloud_keys(packet).or_else(|_| parse_proximity_keys(packet))` — thử layout chặt trước, fallback an toàn.
- **Trạng thái:** ✅ Đã sửa (có test `two_key_response_falls_back_to_proximity_keys`).

### A5 — `BatteryStatus` thiếu `OptimizedCharging` (0x05)
- **File:** `librepods-core/src/device/battery.rs:66`
- **Vấn đề:** status `0x05` (optimized charging) bị `unwrap_or(Disconnected)` map nhầm thành "Disconnected".
- **Bằng chứng:** upstream `Packets.kt` `BatteryStatus`: `CHARGING=1, NOT_CHARGING=2, DISCONNECTED=4, OPTIMIZED_CHARGING=5`.
- **Trạng thái:** ✅ Đã sửa (thêm variant + `from_byte` + test).

### A6 — Panic khi mutex poisoning trong media (Linux)
- **File:** `librepods-linux/src/media.rs:99`
- **Vấn đề:** 3 chỗ `.lock().unwrap()` có thể panic nếu mutex bị poisoned, làm chết daemon.
- **Sửa:** thêm helper `sink_id_cache()` dùng `unwrap_or_else(|p| p.into_inner())` (khôi phục thay vì panic); thay cả 3 chỗ dùng.
- **Trạng thái:** ✅ Đã sửa.

### A7 — CI thiếu phụ thuộc & gate chất lượng
- **File:** `.github/workflows/rust.yml`
- **Vấn đề:** (1) thiếu `libudev-dev` mà `btleplug` cần trên Linux (README/QWEN.md đều liệt kê); (2) không chạy `fmt`/`clippy`; (3) không có check `aarch64-linux-android` (litmus test của dự án).
- **Sửa:** thêm `libudev-dev`; thêm bước `cargo fmt --all -- --check`, `cargo clippy --all-targets -- -D warnings`, và cross-check Android (chỉ Linux). Clippy chạy trên cả 2 OS để phủ lần lượt `librepods-linux` và `librepods-windows`.
- **Trạng thái:** ✅ Đã sửa.

### A8 — `ble_advert.rs`: dead code + kiểu trả về gây nhầm
- **File:** `librepods-core/src/device/ble_advert.rs:267`
- **Vấn đề:** `let _ = &mut advert; // suppress unused mut warning` chỉ để che `mut` thừa; `decrypt_battery` trả `Option<()>` nhưng **không bao giờ** trả `None` (doc nói ngược lại).
- **Sửa:** bỏ `mut` + hack; `decrypt_battery` trả `()` và sửa doc.
- **Trạng thái:** ✅ Đã sửa (hàm không có caller trong repo).

### A9 — `DATA_HEADER` trùng wire value với `NOTIFICATION`
- **File:** `librepods-core/src/crossdevice/packets.rs:48`
- **Vấn đề:** `DATA_HEADER` và `NOTIFICATION` cùng `00 04 00 01`. Upstream `airpods_packets.h` chỉ định nghĩa `Phone::NOTIFICATION` dưới giá trị đó, không có header relay riêng.
- **Sửa:** ghi chú rõ quan hệ alias trong doc-comment (không đổi giá trị vì chưa xác định được header relay khác).
- **Trạng thái:** ✅ Đã sửa (doc).

---

## B. Đã rà soát — KHÔNG cần đổi

### B1 — `ConversationalAwareness { status }` không ghi vào state (`main.rs:485`)
- Xét theo upstream: gói `0x4B` là **mức** CA (ví dụ `01/02` bắt đầu nói, `03` ngừng nói, `08/09` bình thường), **không** phải cờ bật/tắt. Cờ `DeviceState.conversational_awareness` được set đúng từ control command `0x28` (`main.rs:496`), khớp upstream. Vì vậy việc chỉ log mức `0x4B` và **không** ghi đè bool là **đúng**.
- **Trạng thái:** chỉ ghi nhận, không đổi.

### B2 — `is_head_tracking_data` không khớp `HEAD_TRACKING_START/STOP`
- **File:** `librepods-core/src/protocol/packets.rs`
- `is_head_tracking_data` yêu cầu `byte[10] ∈ {0x44,0x45}` (khớp upstream `Packets.kt`), còn START/STOP có `byte[10]=0x10/0x11`. Đây là **thiết kế đúng**: START/STOP là gói **gửi đi**, không đi qua `is_head_tracking_data`. Ngưỡng parser `≥70` byte khớp upstream (`packet.size < 70` → reject).
- **Trạng thái:** chỉ ghi nhận (đã sửa doc `HeadTracking` cho khớp: trả cả packet, không phải payload).

### B3 — `MAGIC_CLOUD_KEYS_HEADER` là đặc thù của bản port
- Upstream không có khái niệm "magic cloud keys" riêng; chỉ có parser proximity-keys tổng quát. Việc tách nhánh 2 khóa IRK+EncKey là **thêm mới của bản Rust** (vẫn hợp lệ). Đã tăng độ bền bằng fallback (A4), giữ nguyên thiết kế.
- **Trạng thái:** chỉ ghi nhận.

### B4 — Round-trip & layout khác đã đối chiếu khớp upstream
- Handshake 16 byte, `REQUEST_NOTIFICATIONS` (10 byte, `0x0F`), battery (component `02/04/08`, spacer/end `0x01`), ear detection (`0x06`), control command (`0x09`, id `0x0D/0x28/0x2E/0x1A`), opcode map chung (`0x04/06/09/0F/17/1D/30/31/4B/4D`) — **khớp**.
- `NoiseControlMode` (`01/02/03/04`) khớp cả `AAP Definitions.md` lẫn `Packets.kt`.
- Model-number map (`model_from_number`) khớp `enums.h` `parseModelNumber()`.
- **Trạng thái:** ✅ tương thích.

---

## C. Gợi ý / backlog (chưa đổi code)

| # | Khu vực | Nhận xét |
|---|---|---|
| C1 | `ControlCommandId` | Thiếu nhiều id upstream: `0x06 OWNS_CONNECTION`, `0x0A EAR_DETECTION_CONFIG`, `0x20 AUTOMATIC_CONNECTION_CONFIG`, `0x35 SLEEP_DETECTION_CONFIG`, `0x36 ALLOW_AUTO_CONNECT`, `0x37 PPE_TOGGLE_CONFIG`, `0x38 PPE_CAP_LEVEL_CONFIG`, `0x39 STEM_CONFIG`, `0x3B DYNAMIC_END_OF_CHARGE`. Hiện rơi vào `Unknown` (an toàn, không panic). |
| C2 | `opcodes.rs` | Thiếu opcode upstream: `0x0E AUDIO_SOURCE`, `0x10 SMART_ROUTING`, `0x11 SMART_ROUTING_RESP`, `0x14 SEND_CONNECTED_MAC`, `0x19 STEM_PRESS`, `0x2E CONNECTED_DEVICES`, `0x53 HEADPHONE_ACCOMMODATION`, `0x63 CUSTOM_EQ`. → `Unknown`. |
| C3 | `crossdevice/packets.rs` | `REQUEST_BATTERY` (`00 02 00 01`), `REQUEST_ANC` (`00 02 00 02`) **không có** trong `airpods_packets.h`; `DATA_HEADER` cũng vậy → chưa xác minh được nguồn, cần tìm mã cross-device tham chiếu. |
| C4 | `librepods-cli/src/main.rs` | Vòng recv giả định `1 recv() = 1 packet`. Trên Linux `SOCK_SEQPACKET` giữ ranh giới message nên tạm ổn, nhưng nếu backend trả nhiều/nửa gói thì parse sai. Nên có bộ đệm/framing nếu hỗ trợ thêm backend. |
| C5 | `librepods-linux/src/media.rs` | `which_exists()` gọi shell `which`; có thể thay bằng dò `$PATH` thuần Rust để giảm phụ thuộc ngoài. |
| C6 | `librepods-windows/*` | `TransportError::new` cho lỗi Winsock mất `source` (chỉ chuỗi); có thể dùng `with_source`. Volume control chưa hỗ trợ (đã ghi trong README). |
| C7 | Toàn crate | Nhiều `pub const` chưa dùng (`AIRPODS_DISCONNECTED`, `EAR_DETECTION_PREFIX`, `REQUEST_MAGIC_CLOUD_KEYS`, `opcodes::SET_FEATURE_FLAGS`) — là API surface, clippy im lặng. Cân nhắc `#[allow(dead_code)]`/tài liệu hoá. |
| C8 | Comment | Tham chiếu `docs/control_commands.md`, `AAP Definitions.md` trỏ tới upstream, không có trong repo — nên nêu rõ đường dẫn upstream. |

---

## D. Checklist tuân thủ (đạt)

- ✅ **Ranh giới crate:** `librepods-core` chỉ dùng `serde`, `thiserror`, `log`, `aes`; không có I/O nền tảng. Hai crate nền tảng có `#![cfg(target_os = "…")]` + dependency target-gated. CLI chọn backend qua `mod platform` bằng `#[cfg]`.
- ✅ **Litmus test:** `cargo check -p librepods-core --target aarch64-linux-android` đạt.
- ✅ **Header AGPL-3.0:** 26/26 file `.rs` có header 3 dòng.
- ✅ **Không `unwrap()` trên wire data** ngoài test (các `unwrap` còn lại: test, và mutex — đã xử lý ở A6).
- ✅ **Đột biến state** chỉ trong `apply_to_state()` (`librepods-cli/src/main.rs`).
- ✅ Enum wire đủ `#[repr(u8)]` + `from_byte()`/`as_byte()`; packet builder đi qua `ControlCommand`.
- ✅ Không có `panic!`/`todo!`/`unimplemented!` trong mã sản phẩm (chỉ trong test).

## E. Xác minh sau khi sửa

| Lệnh | Kết quả |
|---|---|
| `cargo test` (workspace) | ✅ 32 core + 1 doctest |
| `cargo clippy --all-targets -- -D warnings` | ✅ sạch |
| `cargo clippy -p librepods-windows -p librepods-cli --target x86_64-pc-windows-msvc` | ✅ sạch |
| `cargo fmt --all -- --check` | ✅ sạch |
| `cargo check -p librepods-core --target aarch64-linux-android` | ✅ |

## F. Nguồn tham chiếu đã dùng để xác minh

- `AACPManager.kt` — `Opcodes`, `createSetFeatureFlagsPacket`, `parseProximityKeysResponse`, `ControlCommandIdentifiers`.
- `Packets.kt` — `BatteryStatus`, `BatteryComponent`, prefix pattern head-tracking.
- `airpods_packets.h` — `Connection::*`, `Phone::*`.
- `enums.h` — `parseModelNumber()`.
- `docs/AAP Definitions.md` — layout battery/control-command/CA.

*(Các file trên thuộc repo tham chiếu `kavishdevar/librepods`, không nằm trong repo này.)*
