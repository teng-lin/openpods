# Native GPUI desktop

`librepods-gui` is a standalone Linux control window built with the published
GPUI 0.2.2 API. It uses the existing user's daemon socket. It does not open
Bluetooth, take the session lock, scan BLE, change file capabilities, launch a
daemon, or change audio routes. The daemon remains the sole AACP owner.

## Build and run

The renderer lives in a separate workspace; the CLI and default workspace build do not include
GPUI. A Vulkan-capable graphics driver and a Wayland or X11 session are required.
On Arch, the native build dependencies include `base-devel`, `pkgconf`,
`fontconfig`, `freetype2`, `libxkbcommon`, `libxkbcommon-x11`, `wayland`,
`libxcb`, `libx11`, `vulkan-headers`, `vulkan-icd-loader`, `openssl` and `zlib`.
Install the Vulkan driver appropriate for your GPU. The pinned Rust toolchain
is shared with the rest of this workspace.

```sh
cargo build --manifest-path librepods-gui/Cargo.toml --features desktop --release --locked
./librepods-gui/target/release/librepods-gui --demo
```

Demo mode uses only synthetic data and simulated actions; it never constructs
an IPC client. Close the demo and launch without `--demo` to use your daemon:

```sh
# In another terminal, if the service is not already running:
librepods-cli serve --address AA:BB:CC:DD:EE:FF
./librepods-gui/target/release/librepods-gui
```

Use one daemon. An existing systemd user service is sufficient. The GUI retries
a missing socket automatically. Closing the window leaves the daemon running.

## Controls

- Left/right/case battery percentages, or a headset battery for AirPods Max.
  Missing readings display `—`; actual zero is `0%`. Retained or expired
  readings are labelled “Last reported” with their age, including after daemon
  loss. An unavailable AACP case report preserves a prior valid case reading as
  stale, with its original source and receipt time. Fresh valid reports replace
  that history; real 0% remains valid. This history lasts for the running daemon,
  and does not manufacture a value when none was received.
- Off, ANC, Transparency and Adaptive, gated by model and fresh metadata.
- Adaptive strength in ten-point steps, only with a fresh Adaptive-mode report.
- Conversation awareness and single-earbud ANC with observed On/Off states.
- Host pause policy: never, when one earbud is removed, or when both are removed.
- Bluetooth connect/disconnect and forget. Forget needs a second confirmation
  for the selected address and removes pairing/private keys through the daemon.

Older daemons without the `device_management` capability can still supply
battery/settings; the new connection actions remain disabled. Restart with an
updated daemon when you are ready to enable them. This is independent of BLE.
Audio routing, device discovery/pairing, experimental rename/long-press controls,
BLE setup, tray integration and Quickshell bar placement are outside this first
window. Existing CLI and desktop tools still cover those workflows.

Tab/Shift-Tab navigate enabled controls, Enter/Space activate, and Escape cancels
a forget confirmation or closes the window. Small windows scroll vertically;
large tiled windows keep the controls within a readable width. Colors are local
to this application; no Omarchy configuration or Qt theme is needed.

## State, failures and recovery

A bounded background worker reads native status/capabilities approximately once
per second, using one-second read deadlines. Rendering never blocks on IPC.
Controls expire after 3.5 seconds without a successful status, measured from
worker receipt rather than delayed UI consumption. Device observations must
also belong to the current session and be marked fresh by the daemon.

The shared `librepods-api` crate holds status/config/command DTOs and bounded
Unix transport. The daemon reexports its former API paths, and the GUI has no
daemon or platform crate dependency. This prevents the daemon’s Tokio D-Bus
feature from selecting a runtime that GPUI’s portal worker threads do not host.
The desktop has its own Cargo workspace and lockfile to prevent feature
unification with daemon builds, including broad workspace checks.
`scripts/check-gui-deps.py` guards the supported standalone build graph.

Each mutation captures the selected address, daemon instance and generation.
The worker rereads status and validates these before dispatch; the daemon also
asserts the selected address. This preflight is not an atomic session fence:
normal daemon validation and response uncertainty still apply to a restart
between preflight and dispatch. Mutations are serialized and never replayed
automatically. The UI keeps reported state while a request is pending. A lost
reply displays “Outcome unknown”; review the current state before retrying.
Connection failures clear after recovery, while action errors remain visible
until dismissed or another explicit action begins. Read-only refresh resumes
after every command, including failures.

## Optional launcher

After reviewing the built application, these commands install only the GUI and
its launcher under your account; they do not install/start a daemon:

```sh
install -Dm755 librepods-gui/target/release/librepods-gui ~/.local/bin/librepods-gui
install -Dm644 packaging/org.librepods.GUI.desktop ~/.local/share/applications/org.librepods.GUI.desktop
install -Dm644 packaging/org.librepods.GUI.svg ~/.local/share/icons/hicolor/scalable/apps/org.librepods.GUI.svg
```

## Verification

```sh
cargo test --manifest-path librepods-gui/Cargo.toml --locked
cargo test --manifest-path librepods-gui/Cargo.toml --features ui-test --locked
cargo clippy --manifest-path librepods-gui/Cargo.toml --all-targets --features ui-test --locked -- -D warnings
scripts/gui-coverage.sh
```

The default tests cover the model and real temporary Unix-socket peers without
hardware. The opt-in GPUI tests render headlessly and simulate pointer and
keyboard input. The production line coverage gate applies to `model.rs`,
`client.rs`, and `demo.rs`; renderer coverage is not included in that percentage.
A separate desktop CI job compiles the native renderer and runs GPUI tests.
