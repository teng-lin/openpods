// LibrePods - AirPods liberated from Apple's ecosystem
// Copyright (C) 2025 LibrePods contributors
// SPDX-License-Identifier: AGPL-3.0-only

//! Thin command client and persistent daemon entry point.

use clap::{ArgGroup, Parser, Subcommand, ValueEnum};
use librepods_core::device::state::NoiseControlMode;
use librepods_core::protocol::settings::SettingChange;
#[cfg(target_os = "linux")]
use librepods_daemon::ipc::{Request, Response, PROTOCOL_VERSION};
use librepods_daemon::{config::EarPolicy, ipc::Operation, DaemonError, ErrorCode};
#[cfg(target_os = "linux")]
use std::sync::Arc;
#[cfg(target_os = "linux")]
use std::time::Duration;

#[cfg(target_os = "linux")]
#[path = "status.rs"]
mod status;

#[derive(Parser)]
#[command(
    name = "openpods-cli",
    about = "OpenPods AirPods control service and local client"
)]
#[command(group(ArgGroup::new("change").args(["noise_control","conversation_awareness","one_bud_anc"])))]
struct Cli {
    #[arg(short, long, global = true)]
    address: Option<String>,
    /// Display live reported status.
    #[arg(long, conflicts_with = "scan")]
    status: bool,
    /// Emit machine-readable JSON.
    #[arg(long, global = true)]
    json: bool,
    /// Use an isolated direct session; refuses to compete with a running service.
    #[arg(long, global = true)]
    direct: bool,
    /// Accepted for compatibility; audio automation is off by default.
    #[arg(long, global = true)]
    no_media: bool,
    #[arg(long, value_enum, conflicts_with = "scan")]
    noise_control: Option<NoiseMode>,
    #[arg(long, value_enum, conflicts_with = "scan")]
    conversation_awareness: Option<Toggle>,
    #[arg(long, value_enum, conflicts_with = "scan")]
    one_bud_anc: Option<Toggle>,
    #[arg(short, long, conflicts_with = "status")]
    scan: bool,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Clone, Copy, ValueEnum)]
enum NoiseMode {
    #[value(alias = "on")]
    Anc,
    Off,
    Transparency,
    Adaptive,
}
#[derive(Clone, Copy, ValueEnum)]
enum Toggle {
    On,
    Off,
}
#[derive(Clone, Copy, ValueEnum)]
enum EarMode {
    Off,
    One,
    Both,
}

#[derive(Subcommand)]
enum Command {
    /// Run the persistent connection owner.
    Serve {
        #[arg(long)]
        ble: bool,
    },
    /// Print one snapshot.
    Status,
    /// Show service and optional-feature diagnostics.
    Doctor,
    Capabilities,
    Reconnect,
    /// Manage the Bluetooth link and pairing through the persistent service.
    Device {
        #[command(subcommand)]
        action: DeviceCommand,
    },
    /// Enable or disable optional proximity telemetry without restarting.
    Ble {
        #[arg(value_enum)]
        mode: Toggle,
    },
    /// Retrieve or forget privately stored proximity keys. Never prints keys.
    Keys {
        #[command(subcommand)]
        action: KeyCommand,
    },
    /// Change host-side automatic pause policy.
    Ear {
        #[arg(value_enum)]
        mode: EarMode,
    },
    AdaptiveLevel {
        level: u8,
    },
    Rename {
        name: String,
    },
    LongPressModes {
        mask: u8,
    },
    /// Explicit output, input and profile management.
    Audio {
        #[command(subcommand)]
        action: AudioCommand,
    },
    /// Translate an Omarchy control verb without a separate Bluetooth owner.
    Compat {
        verb: String,
    },
}
#[derive(Subcommand)]
enum AudioCommand {
    Status,
    Output { name: String },
    Input { name: String },
    Profile { name: String },
    MoveStream { stream: String, output: String },
    Unmute,
}

#[derive(Subcommand)]
enum DeviceCommand {
    /// Connect already-paired AirPods; AACP readiness is reported separately.
    Connect,
    /// Disconnect Bluetooth audio and suppress automatic AACP recovery.
    Disconnect,
    /// Remove the selected pairing and private proximity keys. Requires --address.
    Forget {
        #[arg(long)]
        yes: bool,
    },
}

#[derive(Subcommand)]
enum KeyCommand {
    Fetch,
    Forget,
}

impl Cli {
    fn operation(&self) -> Result<Operation, DaemonError> {
        if let Some(mode) = self.noise_control {
            return Ok(Operation::Change(SettingChange::ListeningMode(
                match mode {
                    NoiseMode::Anc => NoiseControlMode::NoiseCancellation,
                    NoiseMode::Off => NoiseControlMode::Off,
                    NoiseMode::Transparency => NoiseControlMode::Transparency,
                    NoiseMode::Adaptive => NoiseControlMode::Adaptive,
                },
            )));
        }
        if let Some(value) = self.conversation_awareness {
            return Ok(Operation::Change(SettingChange::ConversationalAwareness(
                matches!(value, Toggle::On),
            )));
        }
        if let Some(value) = self.one_bud_anc {
            return Ok(Operation::Change(SettingChange::SingleEarbudAnc(matches!(
                value,
                Toggle::On
            ))));
        }
        Ok(match &self.command {
            Some(Command::Status) | None => {
                if self.status || self.command.is_none() {
                    Operation::Watch
                } else {
                    Operation::Status
                }
            }
            Some(Command::Doctor) => Operation::Doctor,
            Some(Command::Capabilities) => Operation::Capabilities,
            Some(Command::Reconnect) => Operation::Reconnect,
            Some(Command::Device { action }) => match action {
                DeviceCommand::Connect => Operation::DeviceConnect,
                DeviceCommand::Disconnect => Operation::DeviceDisconnect,
                DeviceCommand::Forget { yes } => {
                    if !yes || self.address.is_none() {
                        return Err(DaemonError::new(ErrorCode::InvalidInput, "CLI", "forget removes the Bluetooth pairing; use device forget --address ADDRESS --yes to confirm"));
                    }
                    Operation::DeviceForget { confirmed: true }
                }
            },
            Some(Command::Ble { mode }) => Operation::BleEnabled(matches!(mode, Toggle::On)),
            Some(Command::Keys { action }) => match action {
                KeyCommand::Fetch => Operation::FetchKeys,
                KeyCommand::Forget => Operation::ForgetKeys,
            },
            Some(Command::Ear { mode }) => Operation::EarPolicy(match mode {
                EarMode::Off => EarPolicy::Off,
                EarMode::One => EarPolicy::OneRemoved,
                EarMode::Both => EarPolicy::BothRemoved,
            }),
            Some(Command::AdaptiveLevel { level }) => {
                Operation::Change(SettingChange::AdaptiveNoiseLevel(*level))
            }
            Some(Command::Rename { name }) => {
                Operation::Change(SettingChange::Rename(name.clone()))
            }
            Some(Command::LongPressModes { mask }) => {
                Operation::Change(SettingChange::LongPressModes(*mask))
            }
            Some(Command::Audio { action }) => match action {
                AudioCommand::Status => Operation::AudioStatus,
                AudioCommand::Output { name } => Operation::SetOutput(name.clone()),
                AudioCommand::Input { name } => Operation::SetInput(name.clone()),
                AudioCommand::Profile { name } => Operation::SetProfile(name.clone()),
                AudioCommand::MoveStream { stream, output } => Operation::MoveStream {
                    stream: stream.clone(),
                    output: output.clone(),
                },
                AudioCommand::Unmute => Operation::Unmute,
            },
            Some(Command::Compat { verb }) => compat_operation(verb)?,
            Some(Command::Serve { .. }) => {
                return Err(DaemonError::new(
                    ErrorCode::InvalidInput,
                    "CLI",
                    "serve is not a client operation",
                ))
            }
        })
    }
}

fn compat_operation(verb: &str) -> Result<Operation, DaemonError> {
    let change = match verb {
        "connect" => return Ok(Operation::DeviceConnect),
        "disconnect" => return Ok(Operation::DeviceDisconnect),
        "status" => return Ok(Operation::Status),
        "noise:off" => SettingChange::ListeningMode(NoiseControlMode::Off),
        "noise:anc" => SettingChange::ListeningMode(NoiseControlMode::NoiseCancellation),
        "noise:transparency" => SettingChange::ListeningMode(NoiseControlMode::Transparency),
        "noise:adaptive" => SettingChange::ListeningMode(NoiseControlMode::Adaptive),
        "ca:on" => SettingChange::ConversationalAwareness(true),
        "ca:off" => SettingChange::ConversationalAwareness(false),
        "onebud:on" => SettingChange::SingleEarbudAnc(true),
        "onebud:off" => SettingChange::SingleEarbudAnc(false),
        "ear:one" => return Ok(Operation::EarPolicy(EarPolicy::OneRemoved)),
        "ear:both" => return Ok(Operation::EarPolicy(EarPolicy::BothRemoved)),
        "ear:off" => return Ok(Operation::EarPolicy(EarPolicy::Off)),
        _ if verb.starts_with("adaptive:") => {
            let value = verb
                .strip_prefix("adaptive:")
                .filter(|value| !value.is_empty() && value.bytes().all(|b| b.is_ascii_digit()))
                .and_then(|value| value.parse::<u8>().ok())
                .filter(|value| *value <= 100)
                .ok_or_else(|| {
                    DaemonError::new(
                        ErrorCode::InvalidInput,
                        "compat",
                        "adaptive level must be an integer from 0 to 100",
                    )
                })?;
            SettingChange::AdaptiveNoiseLevel(value)
        }
        _ => {
            return Err(DaemonError::new(
                ErrorCode::Unsupported,
                "compat",
                "unsupported legacy verb",
            ))
        }
    };
    Ok(Operation::Change(change))
}

#[tokio::main]
pub(crate) async fn entry(args: Vec<std::ffi::OsString>) {
    let cli = Cli::parse_from(args);
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).init();
    if let Err(error) = run(&cli).await {
        if cli.json {
            eprintln!(
                "{}",
                serde_json::to_string(&error).unwrap_or_else(|_| "{\"code\":\"internal\"}".into())
            );
        } else {
            eprintln!("{error}");
        }
        std::process::exit(1);
    }
}

#[cfg(target_os = "linux")]
async fn run(cli: &Cli) -> Result<(), DaemonError> {
    use librepods_daemon::{backend::LinuxBackend, runtime, storage::Paths};
    let paths = Paths::from_env()?;
    let mut config = paths.load_config()?;
    if let Some(address) = &cli.address {
        config.address = Some(address.clone());
    }
    if cli.no_media {
        config.ear_policy = EarPolicy::Off;
        config.ca_ducking = false;
        config.audio_recovery = false;
        config.auto_route = false;
    }
    config.validate()?;
    if let Some(Command::Serve { ble }) = &cli.command {
        if cli.direct {
            return Err(DaemonError::new(
                ErrorCode::InvalidInput,
                "serve",
                "--direct cannot be used with serve",
            ));
        }
        config.ble_enabled = *ble || config.ble_enabled;
        config.validate()?;
        return runtime::serve(paths, config, Arc::new(LinuxBackend::default())).await;
    }
    if cli.scan {
        return scan(&paths).await;
    }
    let operation = cli.operation()?;
    if cli.direct && operation.is_device_action() {
        return Err(DaemonError::new(
            ErrorCode::Unsupported,
            "CLI",
            "Bluetooth device management requires the persistent service",
        ));
    }
    let watch = matches!(operation, Operation::Watch);
    let operation = match &cli.address {
        Some(address) => Operation::ForDevice {
            address: address.clone(),
            operation: Box::new(operation),
        },
        None => operation,
    };
    let request = Request {
        version: PROTOCOL_VERSION,
        id: 1,
        timeout_ms: config.command_timeout_ms,
        operation,
    };
    if cli.direct {
        if paths.socket().exists() || paths.legacy_socket.exists() {
            return Err(DaemonError::new(
                ErrorCode::ServiceUnavailable,
                "direct",
                "stop the running connection owner before opening a direct session",
            ));
        }
        // Hold the same lock as the service before any Bluetooth preparation.
        // A pathname check alone races another direct client or service startup.
        let _owner =
            librepods_daemon::storage::OwnerGuard::acquire(&paths, &runtime::instance_id())?;
        let session = librepods_daemon::session::Session::spawn(
            Arc::new(LinuxBackend::default()),
            config,
            runtime::instance_id(),
        );
        let mut snapshots = session.handle.status.clone();
        let ready = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let snapshot = snapshots.borrow_and_update().clone();
                if snapshot.connected
                    && snapshot.device.model != librepods_core::device::state::AirPodsModel::Unknown
                {
                    return Ok(());
                }
                if let Some(error) = snapshot.last_error {
                    if !error.retryable {
                        return Err(error);
                    }
                }
                snapshots.changed().await.map_err(|_| {
                    DaemonError::new(
                        ErrorCode::ServiceUnavailable,
                        "direct",
                        "session owner stopped",
                    )
                })?;
            }
        })
        .await
        .map_err(|_| {
            DaemonError::new(
                ErrorCode::DeviceUnavailable,
                "direct",
                "AirPods did not become ready within ten seconds",
            )
        })?;
        ready?;
        if watch {
            let mut display = status::Display::new();
            loop {
                let snapshot = snapshots.borrow_and_update().clone();
                show_snapshot(cli, &snapshot, &mut display)?;
                tokio::select! {_=tokio::signal::ctrl_c()=>break,result=snapshots.changed()=>{if result.is_err(){break;}}}
            }
        } else {
            show_response(cli, session.handle.request(request).await)?;
        }
        session.stop();
        return Ok(());
    }
    if watch {
        let mut display = status::Display::new();
        tokio::select! {
            result=runtime::watch_status(&paths,&request,|snapshot|{let _=show_snapshot(cli,&snapshot,&mut display);})=>result,
            _=tokio::signal::ctrl_c()=>Ok(()),
        }
    } else {
        let response = runtime::request(&paths, &request).await?;
        show_response(cli, response)
    }
}

#[cfg(not(target_os = "linux"))]
async fn run(cli: &Cli) -> Result<(), DaemonError> {
    let _ = cli.operation()?;
    Err(DaemonError::new(ErrorCode::Unsupported,"service","persistent service is currently supported on Linux; Windows transport builds remain validated separately"))
}

#[cfg(target_os = "linux")]
fn show_snapshot(
    cli: &Cli,
    snapshot: &librepods_daemon::snapshot::Snapshot,
    display: &mut status::Display,
) -> Result<(), DaemonError> {
    if cli.json {
        println!(
            "{}",
            serde_json::to_string(snapshot).map_err(|e| DaemonError::new(
                ErrorCode::Internal,
                "status",
                e.to_string()
            ))?
        );
    } else {
        display
            .update(snapshot)
            .map_err(|e| DaemonError::io("terminal", e))?;
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn show_response(cli: &Cli, response: Response) -> Result<(), DaemonError> {
    if let Some(error) = response.error {
        return Err(error);
    }
    let value = response
        .result
        .ok_or_else(|| DaemonError::new(ErrorCode::MalformedFrame, "response", "missing result"))?;
    if matches!(&cli.command,Some(Command::Compat{verb})if verb=="status") {
        let snapshot: librepods_daemon::snapshot::Snapshot = serde_json::from_value(value)
            .map_err(|e| DaemonError::new(ErrorCode::MalformedFrame, "status", e.to_string()))?;
        println!("{}", snapshot.legacy());
    } else if cli.json {
        println!("{value}");
    } else {
        println!(
            "{}",
            serde_json::to_string_pretty(&value).map_err(|e| DaemonError::new(
                ErrorCode::Internal,
                "response",
                e.to_string()
            ))?
        );
    }
    Ok(())
}

#[cfg(all(target_os = "linux", feature = "ble"))]
async fn scan(paths: &librepods_daemon::storage::Paths) -> Result<(), DaemonError> {
    use librepods_daemon::{backend::transport_error, runtime, storage::OwnerGuard};
    if paths.legacy_socket.exists() {
        return Err(DaemonError::new(
            ErrorCode::ServiceUnavailable,
            "BLE scan",
            "stop the existing connection owner before diagnostic scanning",
        ));
    }
    let _owner = OwnerGuard::acquire(paths, &runtime::instance_id())?;
    let monitor = librepods_linux::bluez_monitor::BluezMonitor::start()
        .map_err(|error| transport_error("BLE controller monitor", error))?;
    tokio::time::timeout(Duration::from_secs(2), async {
        while !monitor.current_snapshot().available {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .map_err(|_| {
        DaemonError::new(
            ErrorCode::AdapterUnavailable,
            "BLE scan",
            "BlueZ readiness is unknown; scanning was not started",
        )
    })?;
    let check_controller = || {
        let snapshot = monitor.current_snapshot();
        if !snapshot.adapter_ready() {
            return Err(DaemonError::new(
                ErrorCode::AdapterUnavailable,
                "BLE scan",
                "Bluetooth adapter unavailable",
            ));
        }
        if snapshot
            .devices
            .iter()
            .any(|device| device.airpods && device.connected)
        {
            return Err(DaemonError::new(ErrorCode::Unsupported,"BLE scan","connected diagnostic scanning requires a separate controller coexistence acceptance run"));
        }
        Ok(())
    };
    check_controller()?;
    let scanner = Arc::new(
        librepods_linux::ble::BtleplugScanner::start()
            .await
            .map_err(|error| transport_error("BLE scan", error))?,
    );
    let stop = tokio::signal::ctrl_c();
    tokio::pin!(stop);
    loop {
        check_controller()?;
        let source = scanner.clone();
        let mut next = tokio::task::spawn_blocking(move || {
            source.next_advertisement_timeout(Duration::from_millis(100))
        });
        let advert = tokio::select! {
            _=&mut stop=>{let _=next.await;return Ok(());},
            result=&mut next=>result.map_err(|error|DaemonError::new(ErrorCode::Internal,"BLE scan",error.to_string()))?.map_err(|error|transport_error("BLE scan",error))?,
        };
        if let Some(advert) = advert {
            if let Some(parsed) =
                librepods_core::device::ble_advert::parse_advertisement(&advert.manufacturer_data)
            {
                println!("Unverified advertisement: {parsed:?}");
            }
        }
    }
}
#[cfg(all(target_os = "linux", not(feature = "ble")))]
async fn scan(_paths: &librepods_daemon::storage::Paths) -> Result<(), DaemonError> {
    Err(DaemonError::new(
        ErrorCode::Unsupported,
        "BLE",
        "BLE is optional; rebuild with --features ble to enable scanning",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn compatibility_rejects_malformed_verbs_and_preserves_supported_controls() {
        for verb in [
            "noise:anc",
            "ca:on",
            "ear:both",
            "adaptive:100",
            "onebud:off",
        ] {
            assert!(compat_operation(verb).is_ok());
        }
        for verb in [
            "adaptive: 5",
            "adaptive:-1",
            "adaptive:101",
            "adaptive:",
            "raw:0400",
            "forget",
        ] {
            assert!(compat_operation(verb).is_err());
        }
    }
    #[test]
    fn cli_preserves_flags_and_rejects_conflicting_setting_writes() {
        assert!(
            Cli::try_parse_from(["librepods-cli", "--direct", "--noise-control", "anc"]).is_ok()
        );
        assert!(Cli::try_parse_from(["librepods-cli", "status", "--json"]).is_ok());
        assert!(Cli::try_parse_from([
            "librepods-cli",
            "--noise-control",
            "off",
            "--conversation-awareness",
            "on"
        ])
        .is_err());
        assert!(Cli::try_parse_from(["librepods-cli", "--noise-control", "invalid"]).is_err());
    }
}
