// SPDX-License-Identifier: AGPL-3.0-only
#![cfg(all(target_os = "linux", feature = "desktop"))]
use std::process::Command;
#[test]
fn help_and_invalid_arguments_work_without_graphics_or_runtime() {
    let binary = env!("CARGO_BIN_EXE_openpods");
    let output = Command::new(binary)
        .args(["--help"])
        .env_remove("XDG_RUNTIME_DIR")
        .env_remove("DISPLAY")
        .env_remove("WAYLAND_DISPLAY")
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("--demo"));
    let output = Command::new(binary).arg("--invalid").output().unwrap();
    assert_eq!(output.status.code(), Some(2));
}
#[test]
fn live_launch_without_a_runtime_reports_an_actionable_error_before_graphics() {
    let output = Command::new(env!("CARGO_BIN_EXE_openpods"))
        .env_remove("XDG_RUNTIME_DIR")
        .env_remove("DISPLAY")
        .env_remove("WAYLAND_DISPLAY")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr).contains("XDG_RUNTIME_DIR is required"));
}
