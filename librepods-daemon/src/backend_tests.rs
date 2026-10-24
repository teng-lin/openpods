// SPDX-License-Identifier: AGPL-3.0-only
//! Exercise the production backend against a private login1 bus, never the host bus.

use super::{Backend, LinuxBackend};
use librepods_linux::sleep::SleepMonitor;
use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64};
use std::sync::Mutex;
use std::time::Duration;

struct PrivateBus(Child);
impl Drop for PrivateBus {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

async fn emit(service: &zbus::Connection, sleeping: bool) {
    service
        .emit_signal(
            None::<&str>,
            "/org/freedesktop/login1",
            "org.freedesktop.login1.Manager",
            "PrepareForSleep",
            &sleeping,
        )
        .await
        .expect("private sleep signal");
}

async fn observed(backend: &LinuxBackend, epoch: u64, sleeping: bool) {
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            // Inspect the raw monitor to prove both edges arrived BEFORE the
            // production Backend gets its first chance to poll current state.
            let seen = backend
                .sleep
                .lock()
                .unwrap()
                .as_ref()
                .map(|monitor| (monitor.suspend_epoch(), monitor.is_sleeping()));
            if seen == Some((epoch, sleeping)) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    })
    .await
    .expect("raw monitor observed both ordered edges");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn private_login1_edges_stay_latched_until_the_owner_acknowledges_the_observed_epoch() {
    let mut process = Command::new("dbus-daemon")
        .args(["--session", "--nofork", "--print-address=1"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("private dbus-daemon required");
    let mut address = String::new();
    BufReader::new(process.stdout.take().unwrap())
        .read_line(&mut address)
        .unwrap();
    let _bus = PrivateBus(process);
    assert!(!address.trim().is_empty());
    let service = zbus::connection::Builder::address(address.trim())
        .unwrap()
        .name("org.freedesktop.login1")
        .unwrap()
        .build()
        .await
        .unwrap();
    let monitor = SleepMonitor::start_at_bus(address.trim().into()).unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        while !monitor.is_ready() {
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    })
    .await
    .expect("private monitor subscribed");
    let backend = LinuxBackend {
        sleep: Mutex::new(Some(monitor)),
        monitor: None,
        connecting: Mutex::new(None),
        cancelled: AtomicBool::new(false),
        acknowledged_suspend: AtomicU64::new(0),
    };
    emit(&service, true).await;
    emit(&service, false).await;
    observed(&backend, 1, false).await;
    for _ in 0..3 {
        assert!(
            backend.sleeping(),
            "a complete unpolled suspend/resume cycle disappeared"
        );
        assert_eq!(
            backend.suspend_epoch(),
            1,
            "reading must never consume the latch"
        );
    }
    backend.acknowledge_suspend(1);
    assert!(!backend.sleeping());
    emit(&service, true).await;
    observed(&backend, 2, true).await;
    backend.acknowledge_suspend(1);
    assert!(
        backend.sleeping(),
        "old acknowledgement cleared a new epoch"
    );
    backend.acknowledge_suspend(2);
    assert!(
        backend.sleeping(),
        "acknowledgement cannot mask actual suspended state"
    );
    emit(&service, true).await; // Duplicate signal is not another suspend edge.
    emit(&service, false).await;
    observed(&backend, 2, false).await;
    assert_eq!(backend.suspend_epoch(), 2);
    assert!(!backend.sleeping());
    backend.acknowledge_suspend(u64::MAX); // A future acknowledgement cannot clear a future edge.
    emit(&service, true).await;
    emit(&service, false).await;
    observed(&backend, 3, false).await;
    assert!(backend.sleeping());
    backend.acknowledge_suspend(3);
    assert!(!backend.sleeping());
}
