// SPDX-License-Identifier: AGPL-3.0-only
//! Device actions share the session owner's serialization and cancellation fences.
use super::*;
use librepods_core::transport::DeviceAction;

impl Worker {
    pub(super) fn dispatch_device(&mut self, envelope: Envelope) {
        let action = match envelope.request.operation {
            Operation::DeviceConnect => DeviceAction::Connect,
            Operation::DeviceDisconnect => DeviceAction::Disconnect,
            Operation::DeviceForget { confirmed: true } if envelope.target_address.is_some() => {
                DeviceAction::Forget
            }
            _ => {
                let _ = envelope.reply.send(Response::failed(
                    envelope.request.id,
                    DaemonError::new(
                        ErrorCode::InvalidInput,
                        "Bluetooth forget",
                        "forget requires an explicit address and confirmation",
                    ),
                ));
                return;
            }
        };
        let address = envelope
            .target_address
            .clone()
            .or_else(|| self.config.address.clone())
            .unwrap_or_else(|| self.state.bluetooth_address.clone());
        if let Err(error) = self.telemetry.pause_for_connection_with_timeout(
            envelope.deadline.saturating_duration_since(Instant::now()),
        ) {
            let _ = envelope
                .reply
                .send(Response::failed(envelope.request.id, error));
            return;
        }
        let prior_desired = self.recovery.is_desired();
        let backend = self.backend.clone();
        let stopped = self.stopped.clone();
        let cancelled = || {
            envelope.client_cancelled()
                || stopped.load(Ordering::SeqCst)
                || backend.sleeping()
                || backend.suspend_epoch() != envelope.suspend_epoch
        };
        let result =
            backend.device_action(&address, action, envelope.deadline, &cancelled, &mut || {
                envelope.dispatched.store(true, Ordering::SeqCst);
                self.disconnect();
                self.generation = self.generation.saturating_add(1);
                self.device_epoch
                    .send_modify(|epoch| *epoch = epoch.saturating_add(1));
                self.recovery.disconnect_intentionally();
                // Later explicit device actions keep FIFO order; earlier ordinary work cannot cross intent.
                let mut retained = VecDeque::new();
                while let Some(mut pending) = self.queue.pop_front() {
                    if pending.request.operation.is_device_action() {
                        pending.device_epoch = *self.device_epoch.borrow();
                        retained.push_back(pending);
                    } else {
                        let _ = pending.reply.send(Response::failed(
                            pending.request.id,
                            DaemonError::new(
                                ErrorCode::Cancelled,
                                "Bluetooth intent",
                                "device management cancelled queued work",
                            ),
                        ));
                    }
                }
                self.queue = retained;
                self.sync_recovery();
                self.publish();
            });
        let result = if result.is_ok() && (cancelled() || Instant::now() >= envelope.deadline) {
            Err(DaemonError::new(
                ErrorCode::OutcomeUnknown,
                "Bluetooth device",
                "device intent changed after dispatch; inspect status before repeating",
            ))
        } else {
            result
        };
        let result = match result {
            Ok(result) if !address.is_empty() && !result.address.eq_ignore_ascii_case(&address) => {
                Err(DaemonError::new(
                    ErrorCode::OutcomeUnknown,
                    "Bluetooth identity",
                    "unexpected device identity after dispatch; no private keys were removed",
                ))
            }
            result => result,
        };
        let response = match result {
            Ok(result) => {
                self.bluez_connected = result.bluetooth_connected;
                self.state.set_device_identity(&result.address, "");
                if action == DeviceAction::Connect {
                    self.recovery.reconnect_explicitly(self.now());
                }
                self.status.last_error = None;
                self.next_probe_ms = self.now();
                if action == DeviceAction::Forget && self.paths.is_some() {
                    if let Err(mut error) = self.telemetry.forget(&result.address) {
                        error.code = ErrorCode::PersistenceFailed;
                        error.stage = "Bluetooth pairing removed; key cleanup".into();
                        error.retryable = false;
                        error.message = format!(
                            "Bluetooth pairing removed; private key cleanup failed: {}",
                            error.message
                        );
                        self.status.last_error = Some(error.clone());
                        self.sync_recovery();
                        self.publish();
                        let _ = envelope
                            .reply
                            .send(Response::failed(envelope.request.id, error));
                        return;
                    }
                }
                if action == DeviceAction::Forget {
                    self.state.forget_proximity_keys();
                }
                self.sync_recovery();
                self.publish();
                Response::ok(
                    envelope.request.id,
                    serde_json::json!({"device":result,"aacp_ready":false,"aacp_reconnect_scheduled":action == DeviceAction::Connect,"connection_desired":self.recovery.is_desired()}),
                )
            }
            Err(error) => {
                if prior_desired
                    && envelope.dispatched.load(Ordering::SeqCst)
                    && matches!(
                        error.code,
                        ErrorCode::PermissionDenied
                            | ErrorCode::Unsupported
                            | ErrorCode::InvalidInput
                            | ErrorCode::ConnectFailed
                            | ErrorCode::DeviceUnavailable
                    )
                {
                    self.recovery.reconnect_explicitly(self.now());
                }
                self.status.last_error = Some(error.clone());
                self.sync_recovery();
                self.publish();
                Response::failed(envelope.request.id, error)
            }
        };
        let _ = envelope.reply.send(response);
    }
}

#[cfg(test)]
mod tests {
    use super::super::dispatch_tests::{owner, Spy};
    use super::*;
    use librepods_core::transport::DeviceActionResult;
    use std::sync::atomic::AtomicUsize;
    const ADDRESS: &str = "00:11:22:33:44:55";
    struct DeviceBackend(Mutex<Vec<DeviceAction>>);
    impl Backend for DeviceBackend {
        fn discover(&self, _: Option<&str>) -> Result<crate::backend::Endpoint, DaemonError> {
            panic!("no discovery in dispatch")
        }
        fn connect(
            &self,
            _: &crate::backend::Endpoint,
            _: Duration,
        ) -> Result<Arc<dyn SessionTransport>, DaemonError> {
            panic!("no AACP connection in dispatch")
        }
        fn device_action(
            &self,
            address: &str,
            action: DeviceAction,
            _: Instant,
            _: &(dyn Fn() -> bool + Sync),
            prepare: &mut dyn FnMut(),
        ) -> Result<DeviceActionResult, DaemonError> {
            prepare();
            self.0.lock().unwrap().push(action);
            Ok(DeviceActionResult {
                address: address.into(),
                bluetooth_connected: action == DeviceAction::Connect,
                paired: action != DeviceAction::Forget,
                changed: true,
            })
        }
    }
    fn request(id: u64, operation: Operation) -> (Envelope, oneshot::Receiver<Response>) {
        let (reply, rx) = oneshot::channel();
        (
            Envelope {
                device_epoch: 0,
                suspend_epoch: 0,
                peer: None,
                target_address: Some(ADDRESS.into()),
                request: Request {
                    version: 1,
                    id,
                    timeout_ms: 1000,
                    operation,
                },
                deadline: Instant::now() + Duration::from_secs(1),
                reply,
                dispatched: Arc::new(AtomicBool::new(false)),
            },
            rx,
        )
    }
    #[test]
    fn owned_queue_retains_later_device_intents_and_cancels_ordinary_work() {
        let spy = Arc::new(Spy(AtomicUsize::new(0)));
        let mut actor = owner(spy.clone());
        actor.state.set_device_identity(ADDRESS, "Synthetic");
        actor.recovery.ready(0);
        let backend = Arc::new(DeviceBackend(Mutex::new(Vec::new())));
        actor.backend = backend.clone();
        let (ordinary, mut cancelled) = request(
            1,
            Operation::EarPolicy(crate::config::EarPolicy::OneRemoved),
        );
        actor.enqueue(ordinary);
        let (disconnect, mut first) = request(2, Operation::DeviceDisconnect);
        actor.enqueue(disconnect);
        let (connect, mut second) = request(3, Operation::DeviceConnect);
        actor.enqueue(connect);
        assert_eq!(
            actor.queue.iter().map(|e| e.request.id).collect::<Vec<_>>(),
            [2, 3, 1]
        );
        actor.dispatch();
        assert!(first.try_recv().unwrap().error.is_none());
        assert_eq!(
            cancelled.try_recv().unwrap().error.unwrap().code,
            ErrorCode::Cancelled
        );
        assert_eq!(actor.queue.len(), 1);
        assert_eq!(
            actor.queue.front().unwrap().device_epoch,
            *actor.device_epoch.borrow()
        );
        actor.dispatch();
        assert!(second.try_recv().unwrap().error.is_none());
        assert_eq!(
            *backend.0.lock().unwrap(),
            [DeviceAction::Disconnect, DeviceAction::Connect]
        );
        assert!(actor.recovery.is_desired());
        assert_eq!(spy.0.load(Ordering::SeqCst), 0);
        // An old admission forced into the queue still cannot dispatch after the barrier.
        let (old, mut rejected) = request(
            4,
            Operation::EarPolicy(crate::config::EarPolicy::BothRemoved),
        );
        actor.queue.push_back(old);
        actor.dispatch();
        assert_eq!(
            rejected.try_recv().unwrap().error.unwrap().code,
            ErrorCode::Cancelled
        );
    }
    #[test]
    fn owner_rechecks_destructive_confirmation_without_touching_a_backend() {
        let mut actor = owner(Arc::new(Spy(AtomicUsize::new(0))));
        let generation = actor.generation;
        for op in [
            Operation::DeviceForget { confirmed: false },
            Operation::DeviceForget { confirmed: true },
            Operation::DeviceConnect,
        ] {
            let (mut envelope, mut response) = request(1, op);
            envelope.target_address = None;
            if matches!(envelope.request.operation, Operation::DeviceConnect) {
                envelope.request.operation = Operation::Status;
            }
            actor.dispatch_device(envelope);
            assert_eq!(
                response.try_recv().unwrap().error.unwrap().code,
                ErrorCode::InvalidInput
            );
        }
        assert_eq!(actor.generation, generation);
        assert_eq!(*actor.device_epoch.borrow(), 0);
    }
}
