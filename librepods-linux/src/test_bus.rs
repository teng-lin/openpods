// LibrePods - AirPods liberated from Apple's ecosystem
// Copyright (C) 2025 LibrePods contributors
// SPDX-License-Identifier: AGPL-3.0-only

use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};

pub struct TestBus {
    process: Child,
    pub address: String,
}

impl TestBus {
    pub fn start() -> Self {
        let mut process = Command::new("dbus-daemon")
            .args(["--session", "--nofork", "--print-address=1"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("dbus-daemon is required for isolated integration tests");
        let mut address = String::new();
        BufReader::new(process.stdout.take().expect("bus stdout"))
            .read_line(&mut address)
            .expect("bus address");
        assert!(!address.trim().is_empty(), "private bus did not start");
        Self {
            process,
            address: address.trim().to_owned(),
        }
    }
}

impl Drop for TestBus {
    fn drop(&mut self) {
        let _ = self.process.kill();
        let _ = self.process.wait();
    }
}
