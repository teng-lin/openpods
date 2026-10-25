// SPDX-License-Identifier: AGPL-3.0-only
//! Explicit BlueZ operations with fresh identity checks and bounded postconditions.

use crate::bluez_monitor::{snapshot_from, BluezSnapshot, DeviceSnapshot};
use librepods_core::transport::{DeviceAction, DeviceActionResult};
use std::{future::Future, time::Duration};
use tokio::time::Instant;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureKind {
    InvalidInput,
    Unsupported,
    Unavailable,
    PermissionDenied,
    Failed,
    Cancelled,
    TimedOut,
    OutcomeUnknown,
}

#[derive(Debug, thiserror::Error)]
#[error("{stage}: {message}")]
pub struct DeviceError {
    pub kind: FailureKind,
    pub stage: &'static str,
    pub message: String,
}

impl DeviceError {
    fn new(kind: FailureKind, stage: &'static str, message: impl Into<String>) -> Self {
        Self {
            kind,
            stage,
            message: message.into(),
        }
    }
    fn dbus(stage: &'static str, error: zbus::Error, issued: bool) -> Self {
        let kind = match &error {
            zbus::Error::MethodError(name, _, _) => match name.as_str() {
                "org.freedesktop.DBus.Error.AccessDenied"
                | "org.freedesktop.DBus.Error.AuthFailed"
                | "org.bluez.Error.NotAuthorized"
                | "org.bluez.Error.NotPermitted" => FailureKind::PermissionDenied,
                "org.bluez.Error.InvalidArguments" | "org.freedesktop.DBus.Error.InvalidArgs" => {
                    FailureKind::InvalidInput
                }
                "org.bluez.Error.NotSupported"
                | "org.freedesktop.DBus.Error.NotSupported"
                | "org.freedesktop.DBus.Error.UnknownMethod" => FailureKind::Unsupported,
                "org.bluez.Error.NotReady" | "org.bluez.Error.DoesNotExist" => {
                    FailureKind::Unavailable
                }
                "org.bluez.Error.Failed" | "org.bluez.Error.InProgress" => FailureKind::Failed,
                _ if issued => FailureKind::OutcomeUnknown,
                _ => FailureKind::Unavailable,
            },
            _ if issued => FailureKind::OutcomeUnknown,
            _ => FailureKind::Unavailable,
        };
        Self::new(kind, stage, error.to_string())
    }
}

async fn bounded<T>(
    future: impl Future<Output = Result<T, DeviceError>>,
    deadline: Instant,
    cancelled: &(dyn Fn() -> bool + Sync),
    issued: bool,
    stage: &'static str,
) -> Result<T, DeviceError> {
    let interrupted = |kind, message| {
        DeviceError::new(
            if issued {
                FailureKind::OutcomeUnknown
            } else {
                kind
            },
            stage,
            message,
        )
    };
    if cancelled() {
        return Err(interrupted(
            FailureKind::Cancelled,
            "request cancelled; inspect status before repeating a dispatched operation",
        ));
    }
    if Instant::now() >= deadline {
        return Err(interrupted(
            FailureKind::TimedOut,
            "original device request deadline elapsed",
        ));
    }
    tokio::select! {
        biased;
        _ = async { loop { if cancelled() { break; } tokio::time::sleep(Duration::from_millis(5)).await; } } => Err(interrupted(FailureKind::Cancelled, "device request interrupted; no operation will be replayed")),
        _ = tokio::time::sleep_until(deadline) => Err(interrupted(FailureKind::TimedOut, "original device request deadline elapsed")),
        result = future => result,
    }
}

async fn fresh(
    connection: &zbus::Connection,
    owner: &str,
    deadline: Instant,
    cancelled: &(dyn Fn() -> bool + Sync),
    issued: bool,
) -> Result<BluezSnapshot, DeviceError> {
    bounded(
        async {
            snapshot_from(connection, owner).await.map_err(|error| {
                DeviceError::new(
                    if issued {
                        FailureKind::OutcomeUnknown
                    } else {
                        match crate::error::kind(&error) {
                            Some(crate::error::ErrorKind::PermissionDenied) => {
                                FailureKind::PermissionDenied
                            }
                            Some(crate::error::ErrorKind::Timeout) => FailureKind::TimedOut,
                            Some(crate::error::ErrorKind::InvalidInput) => {
                                FailureKind::InvalidInput
                            }
                            Some(crate::error::ErrorKind::Unsupported) => FailureKind::Unsupported,
                            Some(crate::error::ErrorKind::Cancelled) => FailureKind::Cancelled,
                            _ => FailureKind::Unavailable,
                        }
                    },
                    "BlueZ readback",
                    error.to_string(),
                )
            })
        },
        deadline,
        cancelled,
        issued,
        "BlueZ readback",
    )
    .await
}

fn select(
    snapshot: &BluezSnapshot,
    address: &str,
    action: DeviceAction,
) -> Result<DeviceSnapshot, DeviceError> {
    if !address.is_empty()
        && (address.split(':').count() != 6
            || address
                .split(':')
                .any(|part| part.len() != 2 || u8::from_str_radix(part, 16).is_err()))
    {
        return Err(DeviceError::new(
            FailureKind::InvalidInput,
            "BlueZ selection",
            "invalid Bluetooth address",
        ));
    }
    let mut matching = snapshot.devices.iter().filter(|device| {
        device.airpods
            && (address.is_empty() || device.address.eq_ignore_ascii_case(address))
            && (device.paired || action == DeviceAction::Forget)
    });
    let selected = matching.next().ok_or_else(|| {
        DeviceError::new(
            FailureKind::Unavailable,
            "BlueZ selection",
            "selected paired AirPods are unavailable; pair them through the desktop first",
        )
    })?;
    if matching.next().is_some() {
        return Err(DeviceError::new(
            FailureKind::InvalidInput,
            "BlueZ selection",
            "multiple matching AirPods or adapters; specify an unambiguous address",
        ));
    }
    if selected.address.split(':').count() != 6
        || selected
            .address
            .split(':')
            .any(|part| part.len() != 2 || u8::from_str_radix(part, 16).is_err())
    {
        return Err(DeviceError::new(
            FailureKind::InvalidInput,
            "BlueZ selection",
            "BlueZ reported an invalid device address",
        ));
    }
    let adapter = snapshot
        .adapters
        .iter()
        .find(|adapter| adapter.path == selected.adapter)
        .ok_or_else(|| {
            DeviceError::new(
                FailureKind::Unavailable,
                "BlueZ selection",
                "selected device adapter is unavailable",
            )
        })?;
    if action == DeviceAction::Connect && !adapter.powered {
        return Err(DeviceError::new(
            FailureKind::Unavailable,
            "BlueZ selection",
            "selected device adapter is powered off",
        ));
    }
    Ok(selected.clone())
}

async fn bluez_owner(
    connection: &zbus::Connection,
    deadline: Instant,
    cancelled: &(dyn Fn() -> bool + Sync),
    issued: bool,
) -> Result<zbus::names::OwnedUniqueName, DeviceError> {
    bounded(
        async {
            let proxy = zbus::fdo::DBusProxy::new(connection)
                .await
                .map_err(|error| DeviceError::dbus("BlueZ owner", error, issued))?;
            proxy
                .get_name_owner(
                    zbus::names::BusName::try_from("org.bluez").expect("constant bus name"),
                )
                .await
                .map_err(|error| {
                    DeviceError::new(
                        if issued {
                            FailureKind::OutcomeUnknown
                        } else {
                            FailureKind::Unavailable
                        },
                        "BlueZ owner",
                        error.to_string(),
                    )
                })
        },
        deadline,
        cancelled,
        issued,
        "BlueZ owner",
    )
    .await
}

pub async fn execute(
    address: &str,
    action: DeviceAction,
    deadline: std::time::Instant,
    cancelled: &(dyn Fn() -> bool + Sync),
    prepare: &mut dyn FnMut(),
) -> Result<DeviceActionResult, DeviceError> {
    let deadline = Instant::from_std(deadline);
    let connection = bounded(
        async {
            zbus::Connection::system()
                .await
                .map_err(|error| DeviceError::dbus("BlueZ bus", error, false))
        },
        deadline,
        cancelled,
        false,
        "BlueZ bus",
    )
    .await?;
    execute_on(&connection, address, action, deadline, cancelled, prepare).await
}

/// The supplied connection permits tests against a private fake BlueZ bus.
pub async fn execute_on(
    connection: &zbus::Connection,
    address: &str,
    action: DeviceAction,
    deadline: Instant,
    cancelled: &(dyn Fn() -> bool + Sync),
    prepare: &mut dyn FnMut(),
) -> Result<DeviceActionResult, DeviceError> {
    let owner = bluez_owner(connection, deadline, cancelled, false).await?;
    let before = fresh(connection, owner.as_str(), deadline, cancelled, false).await?;
    let device = select(&before, address, action)?;
    let already = match action {
        DeviceAction::Connect => device.connected,
        DeviceAction::Disconnect => !device.connected,
        DeviceAction::Forget => false,
    };
    // Validate before the owner changes its intent or releases its control link.
    bounded(
        async { Ok(()) },
        deadline,
        cancelled,
        false,
        "BlueZ dispatch",
    )
    .await?;
    if bluez_owner(connection, deadline, cancelled, false).await? != owner {
        return Err(DeviceError::new(
            FailureKind::Unavailable,
            "BlueZ owner",
            "BlueZ restarted before dispatch",
        ));
    }
    prepare();
    bounded(async { Ok(()) }, deadline, cancelled, true, "BlueZ intent").await?;
    if already {
        return Ok(DeviceActionResult {
            address: device.address,
            bluetooth_connected: device.connected,
            paired: device.paired,
            changed: false,
        });
    }
    if action == DeviceAction::Forget && device.connected {
        bounded(
            async {
                match connection
                    .call_method(
                        Some(owner.as_str()),
                        device.path.as_str(),
                        Some("org.bluez.Device1"),
                        "Disconnect",
                        &(),
                    )
                    .await
                {
                    Ok(_) => Ok(()),
                    Err(zbus::Error::MethodError(name, _, _))
                        if name.as_str() == "org.bluez.Error.NotConnected" =>
                    {
                        Ok(())
                    }
                    Err(error) => Err(DeviceError::dbus(
                        "BlueZ disconnect before forget",
                        error,
                        true,
                    )),
                }
            },
            deadline,
            cancelled,
            true,
            "BlueZ disconnect before forget",
        )
        .await?;
        if confirm(
            connection,
            owner.as_str(),
            &device,
            DeviceAction::Disconnect,
            deadline,
            cancelled,
        )
        .await?
        .is_none()
        {
            return Ok(DeviceActionResult {
                address: device.address,
                bluetooth_connected: false,
                paired: false,
                changed: true,
            });
        }
    }
    let method = match action {
        DeviceAction::Connect => "Connect",
        DeviceAction::Disconnect => "Disconnect",
        DeviceAction::Forget => "RemoveDevice",
    };
    let call = async {
        let reply = if action == DeviceAction::Forget {
            let path =
                zbus::zvariant::ObjectPath::try_from(device.path.as_str()).map_err(|error| {
                    DeviceError::new(
                        FailureKind::InvalidInput,
                        "BlueZ dispatch",
                        error.to_string(),
                    )
                })?;
            connection
                .call_method(
                    Some(owner.as_str()),
                    device.adapter.as_str(),
                    Some("org.bluez.Adapter1"),
                    method,
                    &(path,),
                )
                .await
        } else {
            connection
                .call_method(
                    Some(owner.as_str()),
                    device.path.as_str(),
                    Some("org.bluez.Device1"),
                    method,
                    &(),
                )
                .await
        };
        match reply {
            Ok(_) => Ok(()),
            Err(zbus::Error::MethodError(name, _, _))
                if (action == DeviceAction::Connect
                    && name.as_str() == "org.bluez.Error.AlreadyConnected")
                    || (action == DeviceAction::Disconnect
                        && name.as_str() == "org.bluez.Error.NotConnected") =>
            {
                Ok(())
            }
            Err(error) => Err(DeviceError::dbus("BlueZ device operation", error, true)),
        }
    };
    bounded(call, deadline, cancelled, true, "BlueZ device operation")
        .await
        .map_err(|mut error| {
            if action == DeviceAction::Forget && device.connected {
                error.message = format!(
                    "Bluetooth disconnection completed; pairing removal result: {}",
                    error.message
                );
            }
            error
        })?;
    let current = confirm(
        connection,
        owner.as_str(),
        &device,
        action,
        deadline,
        cancelled,
    )
    .await?;
    Ok(DeviceActionResult {
        address: device.address,
        bluetooth_connected: action == DeviceAction::Connect,
        paired: action != DeviceAction::Forget && current.is_some_and(|value| value.paired),
        changed: true,
    })
}

async fn confirm(
    connection: &zbus::Connection,
    owner: &str,
    device: &DeviceSnapshot,
    action: DeviceAction,
    deadline: Instant,
    cancelled: &(dyn Fn() -> bool + Sync),
) -> Result<Option<DeviceSnapshot>, DeviceError> {
    loop {
        let after = fresh(connection, owner, deadline, cancelled, true).await?;
        if bluez_owner(connection, deadline, cancelled, true)
            .await?
            .as_str()
            != owner
        {
            return Err(DeviceError::new(
                FailureKind::OutcomeUnknown,
                "BlueZ owner",
                "BlueZ restarted after dispatch; inspect status before repeating",
            ));
        }
        let current = after.devices.iter().find(|value| value.path == device.path);
        let confirmed = match action {
            DeviceAction::Connect => current.is_some_and(|value| {
                value.address.eq_ignore_ascii_case(&device.address)
                    && value.connected
                    && value.paired
            }),
            DeviceAction::Disconnect => current.is_none_or(|value| {
                value.address.eq_ignore_ascii_case(&device.address) && !value.connected
            }),
            DeviceAction::Forget => current.is_none(),
        };
        if confirmed {
            return Ok(current.cloned());
        }
        bounded(
            async {
                tokio::time::sleep(Duration::from_millis(20)).await;
                Ok(())
            },
            deadline,
            cancelled,
            true,
            "BlueZ postcondition",
        )
        .await?;
    }
}

#[cfg(test)]
#[path = "device_tests.rs"]
mod tests;
