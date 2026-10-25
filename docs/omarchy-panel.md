# Omarchy bar panel

`ui/omarchy` adapts the MIT-licensed QML panel from
[omarchy-pods](https://github.com/thisisgm/omarchy-pods) to the Rust daemon.
It runs inside the existing Omarchy Quickshell process, follows the shell theme,
and provides per-component battery, listening modes, Adaptive strength,
conversation awareness, single-earbud ANC and pause policy. The source attribution
and exact upstream revision are in `ui/omarchy/UPSTREAM.md`; its MIT license is
bundled. Icons are original geometric drawings.

## Git plugin installation

The repository root is an Omarchy plugin package: `manifest.json` points at
`ui/omarchy/Panel.qml`, and the executable `setup` configures that checkout in
place. The QML is MIT-licensed; the Rust workspace remains AGPL-3.0-only.

Once this packaging is merged and published to the default GitHub branch:

```sh
omarchy plugin add https://github.com/brianpht/librepods-rs --enable
~/.config/omarchy/plugins/org.librepods.pods/setup --ctl /absolute/path/to/librepods-cli
```

Use an already running, up-to-date Rust daemon. Setup uses the client on PATH if
`--ctl` is omitted. To build a client instead:

```sh
~/.config/omarchy/plugins/org.librepods.pods/setup --build
```

Builds go into `$XDG_CACHE_HOME/librepods-rs/omarchy-build` (default
`~/.cache/librepods-rs/omarchy-build`), keeping artifacts and symlinks out of the
plugin checkout. Builds use the pinned toolchain and lockfile. Existing BLE
configuration is respected when choosing client build features; `--build --ble`
can explicitly include BLE support without enabling scanning.

Setup changes bar configuration through the same installer helpers, and preserves
the Git checkout and tracked QML files. It does not install/start another daemon
or enable daemon startup. Follow [deployment](deployment.md) for those steps.
After `omarchy plugin update org.librepods.pods`, rerun setup if the client needs
rebuilding or its path changed. Restart the shell when QML dependency caching
requires it.

An existing copied installation uses the same plugin ID, so Omarchy will refuse a
second installation. Keep using the copied plugin, or disable it, retain a backup
outside the plugin directory, remove that copy through Omarchy's plugin manager,
and then add the Git plugin. Do not run the copy installer over a Git checkout.

## Local source installation

Use an up-to-date, already running Rust daemon and its matching CLI. The native
snapshot must contain `device.capabilities`; older snapshots fail closed for
feature controls. GPUI remains available as a separate desktop application.

```sh
python3 scripts/install-omarchy.py --ctl /absolute/path/to/librepods-cli --enable
```

This installs `org.librepods.pods` under the user plugin directory and places its
widget before the stock Bluetooth widget. It backs up `shell.json` when changing
the client path and preserves other settings/widgets. The installer waits for
Omarchy's asynchronous discovery, then applies the client path through the bar
API. The installed `ClientConfig.js` also records a fallback path so plugin reloads
do not depend on the shell process's PATH or initial settings-injection timing.
Local changes or unowned plugin directories are preserved, with a clear error.

When updating QML dependencies, Quickshell may retain a cached Service component
through a plugin rescan. Apply a full shell restart after the update if needed:

```sh
omarchy restart shell
```

The installer does not build/start a daemon, enable another Bluetooth owner,
modify pairings, select codecs, or change audio routes. The stock Bluetooth and
audio panels stay available for those operations. Using this panel does not
require legacy status publication, Qt Kvantum, elevated privileges, or the C++
daemon. BLE remains an independent daemon option.

Daemon startup is configured separately; see [deployment](deployment.md).

Without `--enable`, only the plugin files are installed. `--plugin-dir` supports
staging; automatic enablement targets the normal user plugin location. Widget
settings include `hideWhenDisconnected` and an optional `ctlPath` override.

## Behavior and errors

One persistent `librepods-cli --status --json` client streams native snapshots.
A lightweight status probe every 15 seconds checks responsiveness while the
stream is open; opening/refreshing the panel also requests status. A failed
connection retries with bounded backoff. Missing executables produce an explicit
client-path error. The panel never runs `serve`, `--direct`, or a BLE scanner.

Controls use only current-session metadata, capabilities and observed settings.
Commands are argument arrays with an explicit selected-address assertion. One
command runs at a time; extra input while busy is dropped, and no mutation is
replayed automatically after a lost reply or reconnect. Reported settings stay
unchanged while a request is pending. Errors remain visible until another
explicit command begins; successful writes do not leave a success banner.

Known battery values remain visible with “Last reported” and their age when
stale or after daemon loss. Unknown readings remain `--`. Absolute in-ear labels
require known left/right orientation. Case history only exists after an actual
valid report, and lasts for the running daemon.

The panel supports the upstream mouse and keyboard navigation, including
listening-mode shortcuts, j/k row selection, arrow adjustment and activation.

```sh
omarchy shell librepods open
omarchy shell librepods close
omarchy shell librepods status
```

## Tests and removal

```sh
node tests/omarchy/model.mjs
python3 -m unittest discover -s tests/omarchy -p 'test_*.py' -v
python3 tests/omarchy/service.py
omarchy plugin validate ui/omarchy
```

The model and installer checks run in CI. The service harness requires Quickshell
and runs offscreen with a synthetic CLI: it checks streaming, pending state,
errors, reconnect and no replay, without Bluetooth or shell configuration changes.

Disable the widget independently of the daemon:

```sh
omarchy plugin disable org.librepods.pods
```

The plugin directory may then be removed through Omarchy's plugin manager. The
Rust daemon, configuration, private keys and optional GPUI application remain
separate. Do not run the original omarchy-pods setup alongside the Rust owner.
