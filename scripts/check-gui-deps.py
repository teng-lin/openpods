#!/usr/bin/env python3
"""Guard the native GUI's process boundary and GPUI portal runtime selection."""
import subprocess


def tree(*args):
    return subprocess.check_output(['cargo', 'tree', '--locked', *args], text=True)


def main():
    desktop = tree('--manifest-path', 'librepods-gui/Cargo.toml', '--features', 'desktop')
    for forbidden in ['librepods-daemon v', 'librepods-linux v', 'librepods-windows v',
                      'btleplug v', 'bluez-async v']:
        if forbidden in desktop:
            raise SystemExit(f'GUI gained a platform/session dependency: {forbidden}')
    minimal = tree('-p', 'librepods-cli', '--no-default-features')
    if 'gpui v' in minimal:
        raise SystemExit('GUI renderer leaked into the CLI')
    features = tree('--manifest-path', 'librepods-gui/Cargo.toml', '--features', 'desktop', '-e', 'features', '-i', 'zbus')
    if 'zbus feature "tokio"' in features:
        raise SystemExit('GPUI portal zbus would require Tokio on GPUI worker threads')
    print('GUI has no Bluetooth backend; CLI has no GPUI; portal runtime is async-io.')


if __name__ == '__main__':
    main()
