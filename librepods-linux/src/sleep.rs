// LibrePods - AirPods liberated from Apple's ecosystem
// Copyright (C) 2025 LibrePods contributors
// SPDX-License-Identifier: AGPL-3.0-only

//! Cancellable login1 PrepareForSleep subscription.

use std::sync::{mpsc, Mutex};
use std::time::Duration;

use futures::StreamExt;
use librepods_core::transport::TransportError;
use tokio::sync::watch;

use crate::error::{ErrorKind, PlatformError};

pub struct SleepMonitor {
    events: Mutex<mpsc::Receiver<bool>>,
    cancel: watch::Sender<bool>,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl SleepMonitor {
    pub fn start() -> Result<Self, TransportError> {
        let (sender, receiver) = mpsc::sync_channel(8);
        let (cancel, mut stop) = watch::channel(false);
        let worker = std::thread::Builder::new().name("librepods-sleep".into()).spawn(move || {
            let runtime = match tokio::runtime::Builder::new_current_thread().enable_all().build() { Ok(runtime) => runtime, Err(_) => return };
            runtime.block_on(async {
                loop {
                    let mut inner_stop = stop.clone();
                    tokio::select! { _ = stop.changed() => return, result = listen(&sender,&mut inner_stop) => if let Err(error) = result { log::debug!("Sleep monitor unavailable: {error}"); } }
                    tokio::select! { _ = stop.changed() => return, _ = tokio::time::sleep(Duration::from_secs(5)) => {} }
                }
            });
        }).map_err(|error| PlatformError::io("start sleep monitor",error))?;
        Ok(Self {
            events: Mutex::new(receiver),
            cancel,
            worker: Some(worker),
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
) -> Result<(), TransportError> {
    let connection = tokio::time::timeout(Duration::from_secs(2), zbus::Connection::system())
        .await
        .map_err(|_| PlatformError::new("open sleep monitor bus", ErrorKind::Timeout))?
        .map_err(|error| TransportError::with_source("open sleep monitor bus", error))?;
    let mut stream = zbus::MessageStream::for_match_rule("type='signal',sender='org.freedesktop.login1',path='/org/freedesktop/login1',interface='org.freedesktop.login1.Manager',member='PrepareForSleep'", &connection,Some(8)).await
        .map_err(|error| TransportError::with_source("subscribe sleep monitor",error))?;
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
