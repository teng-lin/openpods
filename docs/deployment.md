# User service installation and rollback

Build the no-BLE release first:

```bash
cargo build -p librepods-cli --release --no-default-features --locked
python3 scripts/install.py install
systemctl --user daemon-reload
```

The installer copies only the binary, the graphical-session user unit and a
private ownership manifest. It does not start/enable a service, change pairings,
grant capabilities or edit Bluetooth/audio/panel configuration. Existing files
without a matching ownership manifest and modified installed files are preserved.

The unit runs `librepods-cli serve` with private runtime/state/config directories,
read-only home/system access, no ambient capabilities, no new privileges and only
local/Bluetooth/netlink socket families. Test the real service restrictions on
the host; a command that works in a terminal does not prove sandboxed operation.
The native namespace is `librepods-rs`, separate from the previous daemon.

For a machine without an existing owner:

```bash
systemctl --user start librepods-rs.service
librepods-cli doctor
librepods-cli status --json
```

Enable automatic startup only after verification:

```bash
systemctl --user enable librepods-rs.service
```

## Switching from the old daemon

Run these explicit steps only when ready to change which service owns the AirPods:

```bash
python3 scripts/migrate.py capture --snapshot /path/to/private/rollback.json
python3 scripts/migrate.py switch --snapshot /path/to/private/rollback.json
librepods-cli doctor
librepods-cli status --json
```

Capture is read-only and refuses to overwrite an existing rollback record. Switch
stops the old owner before starting Rust. If startup fails it attempts to restore
the originally active service. Enablement is recorded but not changed. A custom
old unit can be named with `--old-unit` during capture. Never run both owners
against the same AirPods or legacy status file.

Keep the panel on the native or explicitly enabled compatibility client/state
path. Save its previous selection before manually changing it. The migration
script deliberately does not rewrite Quickshell/Omarchy configuration.

Rollback:

```bash
python3 scripts/migrate.py rollback --snapshot /path/to/private/rollback.json
```

This restores captured service activity. Restore any manually changed panel path
as well. It does not remove old binaries or reset configuration, routes or pairing.

Uninstall only after stopping/disabling the Rust service:

```bash
systemctl --user disable --now librepods-rs.service
python3 scripts/install.py uninstall
systemctl --user daemon-reload
```

Uninstall removes only unchanged owned program/unit files. User state/config and
modified files survive. `--prefix`, `--binary` and `--unit-dir` support staging
and packaging tests; those tests never operate on a live user service.
