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
