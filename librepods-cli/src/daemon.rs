// SPDX-License-Identifier: AGPL-3.0-only
mod app;
use clap::Parser;

#[derive(Parser)]
#[command(name = "openpodsd", version, about = "OpenPods background service")]
struct Options {
    #[arg(long)]
    address: Option<String>,
    /// Include optional BLE telemetry when this build supports it.
    #[arg(long)]
    ble: bool,
}
fn main() {
    let options = Options::parse();
    let mut args = vec![std::ffi::OsString::from("openpodsd"), "serve".into()];
    if let Some(address) = options.address {
        args.extend(["--address".into(), address.into()]);
    }
    if options.ble {
        args.push("--ble".into());
    }
    app::entry(args);
}
