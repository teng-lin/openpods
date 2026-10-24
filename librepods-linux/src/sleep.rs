// LibrePods - AirPods liberated from Apple's ecosystem
// Copyright (C) 2025 LibrePods contributors
// SPDX-License-Identifier: AGPL-3.0-only

//! Cancellable login1 PrepareForSleep subscription.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

use futures::StreamExt;
use librepods_core::transport::TransportError;
use tokio::sync::watch;

use crate::error::{ErrorKind, PlatformError};

pub struct SleepMonitor {
    events: Mutex<mpsc::Receiver<bool>>,
    cancel: watch::Sender<bool>,
    worker: Option<std::thread::JoinHandle<()>>,
    ready: Arc<AtomicBool>,
}

impl SleepMonitor {
    pub fn start() -> Result<Self, TransportError> {
        Self::start_with_bus(None)
    }

    pub fn start_at_bus(address: String) -> Result<Self, TransportError> {
        Self::start_with_bus(Some(address))
    }

    pub fn is_ready(&self) -> bool {
        self.ready.load(Ordering::Acquire)
    }

    fn start_with_bus(address: Option<String>) -> Result<Self, TransportError> {
        let (sender, receiver) = mpsc::sync_channel(8);
        let (cancel, mut stop) = watch::channel(false);
        let ready = Arc::new(AtomicBool::new(false));
        let worker_ready = ready.clone();
        let worker = std::thread::Builder::new().name("librepods-sleep".into()).spawn(move || {
            let runtime = match tokio::runtime::Builder::new_current_thread().enable_all().build() { Ok(runtime) => runtime, Err(_) => return };
            runtime.block_on(async {
                loop {
                    let mut inner_stop = stop.clone();
                    tokio::select! { _ = stop.changed() => return, result = listen(&sender,&mut inner_stop,&address,&worker_ready) => if let Err(error) = result { log::debug!("Sleep monitor unavailable: {error}"); } }
                    worker_ready.store(false,Ordering::Release);
                    tokio::select! { _ = stop.changed() => return, _ = tokio::time::sleep(Duration::from_secs(5)) => {} }
                }
            });
        }).map_err(|error| PlatformError::io("start sleep monitor",error))?;
        Ok(Self {
            events: Mutex::new(receiver),
            cancel,
            worker: Some(worker),
            ready,
        })
    }

    pub fn next_event_timeout(&self, timeout: Duration) -> Result<Option<bool>, TransportError> {
        match self
            .events
            .lock()
            .map_err(|_| PlatformError::new("sleep event receiver lock poisoned", ErrorKind::Io))?
            .recv_timeout(timeout)
        {
            Ok(sleeping) => Ok(Some(sleeping)),
            Err(mpsc::RecvTimeoutError::Timeout) => Ok(None),
            Err(mpsc::RecvTimeoutError::Disconnected) => Err(TransportError::ChannelClosed),
        }
    }

    pub fn shutdown(&mut self) {
        let _ = self.cancel.send(true);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
        self.ready.store(false, Ordering::Release);
    }
}

impl Drop for SleepMonitor {
    fn drop(&mut self) {
        self.shutdown();
    }
}

async fn listen(
    sender: &mpsc::SyncSender<bool>,
    stop: &mut watch::Receiver<bool>,
    address: &Option<String>,
    ready: &AtomicBool,
) -> Result<(), TransportError> {
    let connect = async {
        match address {
            Some(address) => {
                zbus::connection::Builder::address(address.as_str())?
                    .build()
                    .await
            }
            None => zbus::Connection::system().await,
        }
    };
    let connection = tokio::time::timeout(Duration::from_secs(2), connect)
        .await
        .map_err(|_| PlatformError::new("open sleep monitor bus", ErrorKind::Timeout))?
        .map_err(|error| TransportError::with_source("open sleep monitor bus", error))?;
    let mut stream = zbus::MessageStream::for_match_rule("type='signal',sender='org.freedesktop.login1',path='/org/freedesktop/login1',interface='org.freedesktop.login1.Manager',member='PrepareForSleep'", &connection,Some(8)).await
        .map_err(|error| TransportError::with_source("subscribe sleep monitor",error))?;
    ready.store(true, Ordering::Release);
    loop {
        let message = tokio::select! { _ = stop.changed() => return Ok(()), message = stream.next() => message.ok_or(TransportError::ChannelClosed)? };
        let message =
            message.map_err(|error| TransportError::with_source("read sleep event", error))?;
        if let Ok(sleeping) = message.body().deserialize::<bool>() {
            match sender.try_send(sleeping) {
                Ok(()) => {}
                Err(mpsc::TrySendError::Disconnected(_)) => return Ok(()),
                Err(mpsc::TrySendError::Full(_)) => {
                    return Err(PlatformError::new(
                        "sleep event queue full",
                        ErrorKind::Unavailable,
                    )
                    .into())
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn private_bus_sleep_events_and_shutdown_are_bounded() {
        let bus = crate::test_bus::TestBus::start();
        let service = zbus::connection::Builder::address(bus.address.as_str())
            .expect("bus builder")
            .name("org.freedesktop.login1")
            .expect("name")
            .build()
            .await
            .expect("login1 connection");
        let mut monitor = SleepMonitor::start_at_bus(bus.address.clone()).expect("monitor");
        tokio::time::timeout(Duration::from_secs(2), async {
            while !monitor.is_ready() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("subscription ready");
        for sleeping in [true, false] {
            service
                .emit_signal(
                    None::<&str>,
                    "/org/freedesktop/login1",
                    "org.freedesktop.login1.Manager",
                    "PrepareForSleep",
                    &sleeping,
                )
                .await
                .expect("sleep signal");
            let observed = tokio::time::timeout(Duration::from_secs(1), async {
                loop {
                    if let Some(value) = monitor
                        .next_event_timeout(Duration::ZERO)
                        .expect("event receiver")
                    {
                        return value;
                    }
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            })
            .await
            .expect("event delivered");
            assert_eq!(observed, sleeping);
        }
        monitor.shutdown();
        assert!(!monitor.is_ready());
    }
}
