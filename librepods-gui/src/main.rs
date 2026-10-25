// SPDX-License-Identifier: AGPL-3.0-only
#[cfg(target_os = "linux")]
mod view;
#[cfg(target_os = "linux")]
fn main() {
    let mut demo = false;
    for arg in std::env::args().skip(1) {
        match arg.as_str() {
            "--demo" => demo = true,
            "--help" | "-h" => {
                println!("OpenPods GUI\n\nUsage: openpods [--demo]\n\nConnects to the current user's librepods-rs daemon.\n--demo uses synthetic data and never contacts Bluetooth or the daemon.");
                return;
            }
            _ => {
                eprintln!("Unknown argument: {arg}");
                std::process::exit(2);
            }
        }
    }
    if let Err(e) = view::run(demo) {
        eprintln!("OpenPods GUI: {e}");
        std::process::exit(1);
    }
}
#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("The GPUI desktop currently supports Linux.");
    std::process::exit(1);
}
