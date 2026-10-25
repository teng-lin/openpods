// SPDX-License-Identifier: AGPL-3.0-only
mod app;
fn main() {
    app::entry(std::env::args_os().collect());
}
