// LibrePods - AirPods liberated from Apple's ecosystem
// Copyright (C) 2025 LibrePods contributors
// SPDX-License-Identifier: AGPL-3.0-only

//! Bounded desktop adapters. They report facts and perform explicit actions;
//! automatic routing and playback ownership decisions belong to core policy.

use std::collections::HashMap;
use std::process::Stdio;
use std::time::Duration;

use librepods_core::transport::TransportError;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::AsyncReadExt;
use tokio::process::Command;

use crate::error::{ErrorKind, PlatformError};

pub use crate::player_monitor::{PlayerEvent, PlayerMonitor};
pub use librepods_core::policy::audio::VolumeChannel;
use librepods_core::policy::audio::{valid_channel_volumes, MAX_VOLUME_CHANNELS};

const TIMEOUT: Duration = Duration::from_secs(2);
const MAX_OUTPUT: u64 = 4 * 1024 * 1024;

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct AudioNode {
    pub index: u32,
    pub name: String,
    pub selected_device: bool,
    pub muted: bool,
    pub volume_percent: Option<f64>,
    /// Exact raw PulseAudio values in the server's channel-map order.
    #[serde(default)]
    pub volume_channels: Option<Vec<VolumeChannel>>,
    pub state: Option<String>,
    pub monitor: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct AudioProfile {
    pub name: String,
    pub available: bool,
    pub sinks: u32,
    pub sources: u32,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct AudioStream {
    pub index: u32,
    pub target_index: Option<u32>,
    pub application: Option<String>,
    pub corked: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct AudioSnapshot {
    pub server_name: String,
    pub card_name: Option<String>,
    pub default_output: Option<String>,
    pub default_input: Option<String>,
    pub profile: Option<String>,
    pub profiles: Vec<AudioProfile>,
    pub outputs: Vec<AudioNode>,
    pub inputs: Vec<AudioNode>,
    pub playback_streams: Vec<AudioStream>,
    pub recording_streams: Vec<AudioStream>,
    pub capture_active: bool,
    pub capture_known: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum AudioAction {
    SetDefaultOutput {
        name: String,
    },
    SetDefaultInput {
        name: String,
    },
    SetProfile {
        profile: String,
    },
    SetMute {
        name: String,
        input: bool,
        muted: bool,
    },
    SetVolume {
        name: String,
        input: bool,
        percent: u8,
    },
    SetRawVolume {
        name: String,
        input: bool,
        expected: Vec<VolumeChannel>,
        channels: Vec<VolumeChannel>,
    },
    MovePlayback {
        index: u32,
        output: String,
    },
    MoveRecording {
        index: u32,
        input: String,
    },
}

async fn pactl(arguments: &[&str]) -> Result<Vec<u8>, TransportError> {
    run_program(
        std::ffi::OsStr::new("pactl"),
        arguments,
        TIMEOUT,
        MAX_OUTPUT,
    )
    .await
}

async fn run_program(
    program: &std::ffi::OsStr,
    arguments: &[&str],
    timeout: Duration,
    max_output: u64,
) -> Result<Vec<u8>, TransportError> {
    let mut command = Command::new(program);
    command
        .args(arguments)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = command
        .spawn()
        .map_err(|error| PlatformError::io("start audio API client", error))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| PlatformError::new("capture audio API output", ErrorKind::Io))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| PlatformError::new("capture audio API error", ErrorKind::Io))?;
    let result = tokio::time::timeout(timeout, async {
        async fn read_limited(
            reader: impl tokio::io::AsyncRead + Unpin,
            limit: u64,
        ) -> Result<Vec<u8>, TransportError> {
            let mut output = Vec::new();
            reader
                .take(limit.saturating_add(1))
                .read_to_end(&mut output)
                .await
                .map_err(|error| PlatformError::io("read audio API output", error))?;
            if output.len() as u64 > limit {
                return Err(PlatformError::new(
                    "audio API output exceeds size limit",
                    ErrorKind::Truncated,
                )
                .into());
            }
            Ok(output)
        }
        let (output, error, status) = tokio::try_join!(
            read_limited(stdout, max_output),
            read_limited(stderr, 64 * 1024),
            async {
                child
                    .wait()
                    .await
                    .map_err(|error| PlatformError::io("wait for audio API client", error).into())
            }
        )?;
        Ok::<_, TransportError>((status, output, error))
    })
    .await
    .map_err(|_| PlatformError::new("audio API operation", ErrorKind::Timeout))??;
    if !result.0.success() {
        let mut error =
            PlatformError::new("audio server rejected operation", ErrorKind::Unavailable);
        error.source = Some(std::io::Error::other(
            String::from_utf8_lossy(&result.2).trim().to_owned(),
        ));
        return Err(error.into());
    }
    Ok(result.1)
}

async fn json(arguments: &[&str]) -> Result<Value, TransportError> {
    let mut args = vec!["--format=json"];
    args.extend_from_slice(arguments);
    serde_json::from_slice(&pactl(&args).await?).map_err(|source| {
        let mut error = PlatformError::new("decode audio API JSON", ErrorKind::Unavailable);
        error.source = Some(std::io::Error::new(std::io::ErrorKind::InvalidData, source));
        error.into()
    })
}

fn string(value: &Value, key: &str) -> Option<String> {
    value.get(key).and_then(Value::as_str).map(str::to_owned)
}
fn number(value: &Value, key: &str) -> Option<u32> {
    value
        .get(key)
        .and_then(Value::as_u64)
        .and_then(|number| u32::try_from(number).ok())
}
fn normalized(address: &str) -> String {
    address.replace(':', "_").to_ascii_lowercase()
}

fn device_matches(value: &Value, address: &str) -> bool {
    let target = normalized(address);
    if target.is_empty() {
        return false;
    }
    string(value, "name").is_some_and(|name| normalized(&name).contains(&target))
        || value
            .get("properties")
            .and_then(Value::as_object)
            .is_some_and(|properties| {
                [
                    "api.bluez5.address",
                    "device.string",
                    "device.name",
                    "node.name",
                ]
                .iter()
                .any(|key| {
                    properties
                        .get(*key)
                        .and_then(Value::as_str)
                        .is_some_and(|name| normalized(name).contains(&target))
                })
            })
}

fn raw_channel_volumes(node: &Value) -> Option<Vec<VolumeChannel>> {
    let map = node.get("channel_map")?;
    let names: Vec<&str> = if let Some(map) = map.as_str() {
        map.split(',')
            .map(str::trim)
            .take(MAX_VOLUME_CHANNELS + 1)
            .collect()
    } else {
        map.as_array()?
            .iter()
            .map(Value::as_str)
            .collect::<Option<_>>()?
    };
    if names.is_empty() || names.len() > MAX_VOLUME_CHANNELS {
        return None;
    }
    let values = node.get("volume")?.as_object()?;
    if values.len() != names.len() {
        return None;
    }
    let channels: Vec<VolumeChannel> = names
        .into_iter()
        .map(|name| {
            let value = values.get(name)?.get("value")?.as_u64()?.try_into().ok()?;
            Some(VolumeChannel {
                name: name.to_owned(),
                value,
            })
        })
        .collect::<Option<_>>()?;
    valid_channel_volumes(&channels).then_some(channels)
}

fn nodes(value: &Value, address: &str) -> Vec<AudioNode> {
    value
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|node| {
            let volume = node
                .get("volume")
                .and_then(Value::as_object)
                .and_then(|channels| {
                    let values: Vec<f64> = channels
                        .values()
                        .filter_map(|channel| channel.get("value").and_then(Value::as_f64))
                        .collect();
                    if values.is_empty() {
                        None
                    } else {
                        Some(values.iter().sum::<f64>() / values.len() as f64 / 65536.0 * 100.0)
                    }
                });
            Some(AudioNode {
                index: number(node, "index")?,
                name: string(node, "name")?,
                selected_device: device_matches(node, address),
                muted: node.get("mute").and_then(Value::as_bool).unwrap_or(false),
                volume_percent: volume,
                volume_channels: raw_channel_volumes(node),
                state: string(node, "state"),
                monitor: node.get("monitor_of_sink").is_some_and(|value| {
                    value.as_u64().is_some_and(|index| index != u32::MAX as u64)
                        || value
                            .as_str()
                            .is_some_and(|value| value != "n/a" && value != "4294967295")
                }),
            })
        })
        .collect()
}

fn streams(value: &Value, target: &str) -> Vec<AudioStream> {
    value
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|stream| {
            Some(AudioStream {
                index: number(stream, "index")?,
                target_index: number(stream, target),
                application: stream
                    .get("properties")
                    .and_then(|properties| string(properties, "application.name")),
                corked: stream
                    .get("corked")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
            })
        })
        .collect()
}

/// Read structured server state without changing any routing, profile or volume.
pub async fn audio_snapshot(address: &str) -> Result<AudioSnapshot, TransportError> {
    let (info, cards, outputs, inputs, playback, recording) = tokio::try_join!(
        json(&["info"]),
        json(&["list", "cards"]),
        json(&["list", "sinks"]),
        json(&["list", "sources"]),
        json(&["list", "sink-inputs"]),
        json(&["list", "source-outputs"])
    )?;
    if !info.is_object()
        || [&cards, &outputs, &inputs, &playback, &recording]
            .iter()
            .any(|value| !value.is_array())
    {
        return Err(PlatformError::new(
            "audio API returned an invalid snapshot shape",
            ErrorKind::Unavailable,
        )
        .into());
    }
    Ok(decode_audio(
        address, info, cards, outputs, inputs, playback, recording,
    ))
}

fn decode_audio(
    address: &str,
    info: Value,
    cards: Value,
    outputs: Value,
    inputs: Value,
    playback: Value,
    recording: Value,
) -> AudioSnapshot {
    let card = cards
        .as_array()
        .into_iter()
        .flatten()
        .find(|card| device_matches(card, address));
    let mut profiles = Vec::new();
    if let Some(entries) = card
        .and_then(|card| card.get("profiles"))
        .and_then(Value::as_object)
    {
        for (name, profile) in entries {
            profiles.push(AudioProfile {
                name: name.clone(),
                available: profile
                    .get("available")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                sinks: number(profile, "sinks").unwrap_or(0),
                sources: number(profile, "sources").unwrap_or(0),
            });
        }
    }
    let inputs = nodes(&inputs, address);
    let recording_streams = streams(&recording, "source");
    let capture_active = recording_streams.iter().any(|stream| {
        !stream.corked
            && inputs.iter().any(|input| {
                input.selected_device && !input.monitor && Some(input.index) == stream.target_index
            })
    });
    let capture_known = recording_streams
        .iter()
        .filter(|stream| !stream.corked)
        .all(|stream| {
            stream
                .target_index
                .is_some_and(|index| inputs.iter().any(|input| input.index == index))
        });
    AudioSnapshot {
        card_name: card.and_then(|card| string(card, "name")),
        server_name: string(&info, "server_name").unwrap_or_default(),
        default_output: string(&info, "default_sink_name"),
        default_input: string(&info, "default_source_name"),
        profile: card.and_then(|card| string(card, "active_profile")),
        profiles,
        outputs: nodes(&outputs, address),
        inputs,
        playback_streams: streams(&playback, "sink"),
        recording_streams,
        capture_active,
        capture_known,
    }
}

/// Perform only the requested mutation, after resolving current node/profile state.
pub async fn audio_action(address: &str, action: AudioAction) -> Result<(), TransportError> {
    let state = audio_snapshot(address).await?;
    let args = action_arguments(&state, action)?;
    pactl(&args.iter().map(String::as_str).collect::<Vec<_>>()).await?;
    Ok(())
}

fn action_arguments(
    state: &AudioSnapshot,
    action: AudioAction,
) -> Result<Vec<String>, TransportError> {
    let valid_node = |name: &str, input: bool| -> bool {
        (if input { &state.inputs } else { &state.outputs })
            .iter()
            .any(|node| node.name == name)
    };
    let args: Vec<String> = match action {
        AudioAction::SetDefaultOutput { name } if valid_node(&name, false) => {
            vec!["set-default-sink".into(), name]
        }
        AudioAction::SetDefaultInput { name } if valid_node(&name, true) => {
            vec!["set-default-source".into(), name]
        }
        AudioAction::SetProfile { profile }
            if state
                .profiles
                .iter()
                .any(|candidate| candidate.name == profile && candidate.available) =>
        {
            let name = state.card_name.clone().ok_or_else(|| {
                PlatformError::new("selected audio card unavailable", ErrorKind::Unavailable)
            })?;
            vec!["set-card-profile".into(), name, profile]
        }
        AudioAction::SetMute { name, input, muted } if valid_node(&name, input) => vec![
            if input {
                "set-source-mute"
            } else {
                "set-sink-mute"
            }
            .into(),
            name,
            if muted { "1" } else { "0" }.into(),
        ],
        AudioAction::SetVolume {
            name,
            input,
            percent,
        } if valid_node(&name, input) && percent <= 100 => vec![
            if input {
                "set-source-volume"
            } else {
                "set-sink-volume"
            }
            .into(),
            name,
            format!("{percent}%"),
        ],
        AudioAction::SetRawVolume {
            name,
            input,
            expected,
            channels,
        } => {
            let current = (if input { &state.inputs } else { &state.outputs })
                .iter()
                .find(|node| node.name == name);
            if !state.capture_known
                || state.capture_active
                || !valid_channel_volumes(&expected)
                || !valid_channel_volumes(&channels)
                || expected.len() != channels.len()
                || expected
                    .iter()
                    .zip(&channels)
                    .any(|(before, after)| before.name != after.name)
                || current.and_then(|node| node.volume_channels.as_ref()) != Some(&expected)
            {
                return Err(PlatformError::new(
                    "audio volume ownership, channel map or capture state changed",
                    ErrorKind::InvalidInput,
                )
                .into());
            }
            let mut args = vec![
                if input {
                    "set-source-volume"
                } else {
                    "set-sink-volume"
                }
                .to_owned(),
                name,
            ];
            args.extend(channels.iter().map(|channel| channel.value.to_string()));
            args
        }
        AudioAction::MovePlayback { index, output }
            if valid_node(&output, false)
                && state
                    .playback_streams
                    .iter()
                    .any(|stream| stream.index == index) =>
        {
            vec!["move-sink-input".into(), index.to_string(), output]
        }
        AudioAction::MoveRecording { index, input }
            if valid_node(&input, true)
                && state
                    .recording_streams
                    .iter()
                    .any(|stream| stream.index == index) =>
        {
            vec!["move-source-output".into(), index.to_string(), input]
        }
        _ => {
            return Err(PlatformError::new(
                "audio action target or value is unavailable",
                ErrorKind::InvalidInput,
            )
            .into())
        }
    };
    Ok(args)
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PlayerSnapshot {
    pub bus_name: String,
    pub owner: String,
    pub playback_status: String,
    pub can_pause: bool,
    pub can_play: bool,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PlayerAction {
    Pause,
    Play,
}

async fn player_on(
    connection: &zbus::Connection,
    name: &str,
    owner: &str,
) -> Result<PlayerSnapshot, TransportError> {
    let proxy = zbus::Proxy::new(
        connection,
        owner,
        "/org/mpris/MediaPlayer2",
        "org.mpris.MediaPlayer2.Player",
    )
    .await
    .map_err(|error| TransportError::with_source("open MPRIS player", error))?;
    let (playback_status, can_pause, can_play) = tokio::try_join!(
        proxy.get_property::<String>("PlaybackStatus"),
        proxy.get_property::<bool>("CanPause"),
        proxy.get_property::<bool>("CanPlay")
    )
    .map_err(|error| TransportError::with_source("read MPRIS player", error))?;
    Ok(PlayerSnapshot {
        bus_name: name.to_owned(),
        owner: owner.to_owned(),
        playback_status,
        can_pause,
        can_play,
    })
}

pub async fn players_on(
    connection: &zbus::Connection,
) -> Result<Vec<PlayerSnapshot>, TransportError> {
    tokio::time::timeout(TIMEOUT, async {
        let bus = zbus::fdo::DBusProxy::new(connection)
            .await
            .map_err(|error| TransportError::with_source("open session bus proxy", error))?;
        let names = bus
            .list_names()
            .await
            .map_err(|error| TransportError::with_source("enumerate MPRIS players", error))?;
        use futures::StreamExt;
        let mut probes = futures::stream::FuturesUnordered::new();
        for name in names
            .into_iter()
            .filter(|name| name.as_str().starts_with("org.mpris.MediaPlayer2."))
            .take(64)
        {
            let bus = &bus;
            probes.push(async move {
                // A nonresponsive advertised player must not hide responsive peers.
                tokio::time::timeout(Duration::from_millis(500), async {
                    let owner = bus
                        .get_name_owner(name.as_str().try_into().ok()?)
                        .await
                        .ok()?;
                    player_on(connection, name.as_str(), owner.as_str())
                        .await
                        .ok()
                })
                .await
                .ok()
                .flatten()
            });
        }
        let mut players = Vec::new();
        while let Some(player) = probes.next().await {
            if let Some(player) = player {
                players.push(player);
            }
        }
        players.sort_by(|left, right| left.bus_name.cmp(&right.bus_name));
        Ok(players)
    })
    .await
    .map_err(|_| PlatformError::new("enumerate MPRIS players", ErrorKind::Timeout))?
}

pub async fn players() -> Result<Vec<PlayerSnapshot>, TransportError> {
    tokio::time::timeout(TIMEOUT, async {
        let connection = zbus::Connection::session()
            .await
            .map_err(|error| TransportError::with_source("open user bus", error))?;
        players_on(&connection).await
    })
    .await
    .map_err(|_| PlatformError::new("open MPRIS connection", ErrorKind::Timeout))?
}

pub async fn player_action_on(
    connection: &zbus::Connection,
    player: &PlayerSnapshot,
    action: PlayerAction,
) -> Result<PlayerSnapshot, TransportError> {
    tokio::time::timeout(TIMEOUT, async {
        let bus = zbus::fdo::DBusProxy::new(connection)
            .await
            .map_err(|error| TransportError::with_source("open session bus proxy", error))?;
        let owner = bus
            .get_name_owner(
                player
                    .bus_name
                    .as_str()
                    .try_into()
                    .map_err(|error| TransportError::with_source("validate player name", error))?,
            )
            .await
            .map_err(|error| TransportError::with_source("check MPRIS owner", error))?;
        if owner.as_str() != player.owner {
            return Err(
                PlatformError::new("MPRIS player owner changed", ErrorKind::Unavailable).into(),
            );
        }
        let current = player_on(connection, &player.bus_name, &player.owner).await?;
        if (action == PlayerAction::Pause && !current.can_pause)
            || (action == PlayerAction::Play && !current.can_play)
        {
            return Err(
                PlatformError::new("MPRIS action unsupported", ErrorKind::Unsupported).into(),
            );
        }
        let proxy = zbus::Proxy::new(
            connection,
            player.owner.as_str(),
            "/org/mpris/MediaPlayer2",
            "org.mpris.MediaPlayer2.Player",
        )
        .await
        .map_err(|error| TransportError::with_source("open MPRIS player", error))?;
        proxy
            .call_method(
                match action {
                    PlayerAction::Pause => "Pause",
                    PlayerAction::Play => "Play",
                },
                &(),
            )
            .await
            .map_err(|error| TransportError::with_source("MPRIS action failed", error))?;
        let expected = match action {
            PlayerAction::Pause => "Paused",
            PlayerAction::Play => "Playing",
        };
        loop {
            let observed = player_on(connection, &player.bus_name, &player.owner).await?;
            if observed.playback_status == expected {
                return Ok(observed);
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .map_err(|_| PlatformError::new("MPRIS action", ErrorKind::Timeout))?
}

pub async fn player_action(
    player: &PlayerSnapshot,
    action: PlayerAction,
) -> Result<PlayerSnapshot, TransportError> {
    tokio::time::timeout(TIMEOUT, async {
        let connection = zbus::Connection::session()
            .await
            .map_err(|error| TransportError::with_source("open user bus", error))?;
        player_action_on(&connection, player, action).await
    })
    .await
    .map_err(|_| PlatformError::new("open MPRIS connection", ErrorKind::Timeout))?
}

pub async fn notify(summary: &str, body: &str) -> Result<(), TransportError> {
    tokio::time::timeout(TIMEOUT, async {
        let connection = zbus::Connection::session()
            .await
            .map_err(|error| TransportError::with_source("open notification bus", error))?;
        let proxy = zbus::Proxy::new(
            &connection,
            "org.freedesktop.Notifications",
            "/org/freedesktop/Notifications",
            "org.freedesktop.Notifications",
        )
        .await
        .map_err(|error| TransportError::with_source("open desktop notification service", error))?;
        let hints: HashMap<&str, zbus::zvariant::Value<'_>> = HashMap::new();
        proxy
            .call_method(
                "Notify",
                &(
                    "LibrePods",
                    0u32,
                    "audio-headphones",
                    summary,
                    body,
                    Vec::<String>::new(),
                    hints,
                    5000i32,
                ),
            )
            .await
            .map_err(|error| TransportError::with_source("send desktop notification", error))?;
        Ok(())
    })
    .await
    .map_err(|_| PlatformError::new("desktop notification", ErrorKind::Timeout))?
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn explicit_audio_actions_validate_targets_and_preserve_argument_boundaries() {
        let state = AudioSnapshot {
            card_name: Some("selected-card".into()),
            profiles: vec![AudioProfile {
                name: "a2dp".into(),
                available: true,
                sinks: 1,
                sources: 0,
            }],
            outputs: vec![AudioNode {
                index: 1,
                name: "output with spaces".into(),
                ..Default::default()
            }],
            inputs: vec![AudioNode {
                index: 2,
                name: "external microphone".into(),
                ..Default::default()
            }],
            playback_streams: vec![AudioStream {
                index: 3,
                ..Default::default()
            }],
            recording_streams: vec![AudioStream {
                index: 4,
                ..Default::default()
            }],
            ..Default::default()
        };
        let cases = [
            (
                AudioAction::SetDefaultOutput {
                    name: "output with spaces".into(),
                },
                vec!["set-default-sink", "output with spaces"],
            ),
            (
                AudioAction::SetDefaultInput {
                    name: "external microphone".into(),
                },
                vec!["set-default-source", "external microphone"],
            ),
            (
                AudioAction::SetProfile {
                    profile: "a2dp".into(),
                },
                vec!["set-card-profile", "selected-card", "a2dp"],
            ),
            (
                AudioAction::SetMute {
                    name: "external microphone".into(),
                    input: true,
                    muted: true,
                },
                vec!["set-source-mute", "external microphone", "1"],
            ),
            (
                AudioAction::SetMute {
                    name: "output with spaces".into(),
                    input: false,
                    muted: false,
                },
                vec!["set-sink-mute", "output with spaces", "0"],
            ),
            (
                AudioAction::SetVolume {
                    name: "external microphone".into(),
                    input: true,
                    percent: 20,
                },
                vec!["set-source-volume", "external microphone", "20%"],
            ),
            (
                AudioAction::SetVolume {
                    name: "output with spaces".into(),
                    input: false,
                    percent: 40,
                },
                vec!["set-sink-volume", "output with spaces", "40%"],
            ),
            (
                AudioAction::MovePlayback {
                    index: 3,
                    output: "output with spaces".into(),
                },
                vec!["move-sink-input", "3", "output with spaces"],
            ),
            (
                AudioAction::MoveRecording {
                    index: 4,
                    input: "external microphone".into(),
                },
                vec!["move-source-output", "4", "external microphone"],
            ),
        ];
        for (action, expected) in cases {
            assert_eq!(
                action_arguments(&state, action).expect("explicit target"),
                expected
            );
        }
        for action in [
            AudioAction::SetProfile {
                profile: "missing".into(),
            },
            AudioAction::SetVolume {
                name: "output with spaces".into(),
                input: false,
                percent: 101,
            },
            AudioAction::SetDefaultInput {
                name: "missing".into(),
            },
            AudioAction::MovePlayback {
                index: 99,
                output: "output with spaces".into(),
            },
        ] {
            assert!(action_arguments(&state, action).is_err());
        }
    }

    #[test]
    fn unresolved_recording_target_does_not_claim_capture_is_idle() {
        let state = decode_audio(
            "AA:BB:CC:DD:EE:FF",
            json!({}),
            json!([]),
            json!([]),
            json!([]),
            json!([]),
            json!([{"index":1,"corked":false}]),
        );
        assert!(!state.capture_known);
    }

    #[tokio::test]
    async fn audio_process_output_deadlines_and_failures_are_bounded() {
        use std::ffi::OsStr;
        assert_eq!(
            run_program(
                OsStr::new("/usr/bin/printf"),
                &["hello"],
                Duration::from_secs(1),
                8
            )
            .await
            .expect("small output"),
            b"hello"
        );
        let error = run_program(
            OsStr::new("/usr/bin/printf"),
            &["too long"],
            Duration::from_secs(1),
            2,
        )
        .await
        .expect_err("output bound");
        assert_eq!(crate::error::kind(&error), Some(ErrorKind::Truncated));
        let error = run_program(
            OsStr::new("/usr/bin/sleep"),
            &["30"],
            Duration::from_millis(20),
            8,
        )
        .await
        .expect_err("deadline");
        assert_eq!(crate::error::kind(&error), Some(ErrorKind::Timeout));
        assert!(
            run_program(OsStr::new("/usr/bin/false"), &[], Duration::from_secs(1), 8)
                .await
                .is_err()
        );
        assert!(run_program(
            OsStr::new("/no/such/audio-client"),
            &[],
            Duration::from_secs(1),
            8
        )
        .await
        .is_err());
    }

    #[derive(Default)]
    struct FakePlayer {
        playing: bool,
        unsupported: bool,
        stall: bool,
    }
    #[zbus::interface(name = "org.mpris.MediaPlayer2.Player")]
    impl FakePlayer {
        #[zbus(property)]
        fn playback_status(&self) -> &str {
            if self.playing {
                "Playing"
            } else {
                "Paused"
            }
        }
        #[zbus(property)]
        fn can_pause(&self) -> bool {
            !self.unsupported
        }
        #[zbus(property)]
        fn can_play(&self) -> bool {
            !self.unsupported
        }
        fn pause(&mut self) {
            if !self.stall {
                self.playing = false;
            }
        }
        fn play(&mut self) {
            self.playing = true;
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn private_bus_player_actions_confirm_state_and_reject_owner_replacement() {
        let bus = crate::test_bus::TestBus::start();
        let name = "org.mpris.MediaPlayer2.librepods_test";
        let service = zbus::connection::Builder::address(bus.address.as_str())
            .expect("bus builder")
            .name(name)
            .expect("name")
            .serve_at(
                "/org/mpris/MediaPlayer2",
                FakePlayer {
                    playing: true,
                    ..Default::default()
                },
            )
            .expect("player")
            .build()
            .await
            .expect("service");
        let client = zbus::connection::Builder::address(bus.address.as_str())
            .expect("client builder")
            .build()
            .await
            .expect("client");
        let players = players_on(&client).await.expect("enumerate");
        assert_eq!(players.len(), 1);
        let paused = player_action_on(&client, &players[0], PlayerAction::Pause)
            .await
            .expect("pause");
        assert_eq!(paused.playback_status, "Paused");
        service.release_name(name).await.expect("release player");
        let replacement = zbus::connection::Builder::address(bus.address.as_str())
            .expect("replacement builder")
            .name(name)
            .expect("name")
            .serve_at(
                "/org/mpris/MediaPlayer2",
                FakePlayer {
                    playing: true,
                    ..Default::default()
                },
            )
            .expect("player")
            .build()
            .await
            .expect("replacement");
        let error = player_action_on(&client, &paused, PlayerAction::Pause)
            .await
            .expect_err("owner changed");
        assert_eq!(crate::error::kind(&error), Some(ErrorKind::Unavailable));
        assert!(
            replacement
                .object_server()
                .interface::<_, FakePlayer>("/org/mpris/MediaPlayer2")
                .await
                .expect("player")
                .get()
                .await
                .playing
        );
    }

    #[test]
    fn invalid_monitor_sentinel_is_a_real_capture_source() {
        let inputs = nodes(
            &json!([{"index":1,"name":"bluez_input.AA_BB_CC_DD_EE_FF","monitor_of_sink":4294967295u64}]),
            "AA:BB:CC:DD:EE:FF",
        );
        assert!(!inputs[0].monitor);
    }

    #[test]
    fn audio_snapshot_distinguishes_missing_microphone_and_preserves_external_input() {
        let state = decode_audio(
            "AA:BB:CC:DD:EE:FF",
            json!({"server_name":"PipeWire","default_sink_name":"bluez_output.AA_BB_CC_DD_EE_FF.1","default_source_name":"webcam"}),
            json!([{"name":"bluez_card.AA_BB_CC_DD_EE_FF","active_profile":"a2dp-sink","profiles":{"a2dp-sink":{"available":true,"sinks":1,"sources":0},"headset-head-unit":{"available":true,"sinks":1,"sources":1}}}]),
            json!([{"index":5,"name":"bluez_output.AA_BB_CC_DD_EE_FF.1","mute":false,"volume":{"left":{"value":32768}}}]),
            json!([{"index":8,"name":"webcam","mute":false}]),
            json!([]),
            json!([]),
        );
        assert_eq!(state.default_input.as_deref(), Some("webcam"));
        assert_eq!(state.outputs[0].volume_percent, Some(50.0));
        assert!(state.outputs[0].selected_device);
        assert!(!state.capture_active);
        assert_eq!(
            state
                .profiles
                .iter()
                .find(|p| p.name == "a2dp-sink")
                .expect("profile")
                .sources,
            0
        );
        assert!(!state.inputs[0].selected_device);
    }

    #[test]
    fn recording_streams_on_other_devices_and_monitors_are_not_airpods_capture() {
        let state = decode_audio(
            "AA:BB:CC:DD:EE:FF",
            json!({}),
            json!([]),
            json!([]),
            json!([{"index":4,"name":"bluez_output.AA_BB_CC_DD_EE_FF.monitor","monitor_of_sink":3},{"index":5,"name":"webcam"}]),
            json!([]),
            json!([{"index":1,"source":4,"corked":false},{"index":2,"source":5,"corked":false}]),
        );
        assert!(!state.capture_active);
    }
    #[tokio::test]
    async fn noisy_audio_subprocess_fails_as_truncated_before_its_deadline() {
        let started = std::time::Instant::now();
        let error = run_program(
            std::ffi::OsStr::new("/usr/bin/python3"),
            &[
                "-c",
                "import sys; sys.stdout.write('x'*1000000); sys.stdout.flush()",
            ],
            Duration::from_secs(2),
            100,
        )
        .await
        .expect_err("output flood");
        assert_eq!(crate::error::kind(&error), Some(ErrorKind::Truncated));
        assert!(started.elapsed() < Duration::from_secs(1));
        let error = run_program(
            std::ffi::OsStr::new("/usr/bin/python3"),
            &[
                "-c",
                "import sys; sys.stderr.write('x'*1000000); sys.stderr.flush()",
            ],
            Duration::from_secs(2),
            100,
        )
        .await
        .expect_err("stderr flood");
        assert_eq!(crate::error::kind(&error), Some(ErrorKind::Truncated));
        let error = run_program(
            std::ffi::OsStr::new("/usr/bin/python3"),
            &[
                "-c",
                "import sys; sys.stderr.write('audio unavailable'); sys.exit(1)",
            ],
            Duration::from_secs(2),
            100,
        )
        .await
        .expect_err("server rejected");
        assert_eq!(crate::error::kind(&error), Some(ErrorKind::Unavailable));
        use std::error::Error;
        let source = error
            .source()
            .and_then(|source| source.source())
            .expect("original command failure retained");
        assert!(source.to_string().contains("audio unavailable"));
    }
    #[test]
    fn raw_volume_decode_preserves_channel_order_balance_and_fractional_percent_units() {
        let node = json!({"index":1,"name":"sink","channel_map":"front-right,front-left","volume":{"front-left":{"value":65537},"front-right":{"value":32769}}});
        let decoded = nodes(&json!([node.clone()]), "");
        assert_eq!(
            decoded[0].volume_channels,
            Some(vec![
                VolumeChannel {
                    name: "front-right".into(),
                    value: 32769
                },
                VolumeChannel {
                    name: "front-left".into(),
                    value: 65537
                }
            ])
        );
        for map in [
            json!("front-left,front-left"),
            json!("front-left"),
            json!([]),
            json!(""),
            json!(["front-left", 1]),
        ] {
            let mut invalid = node.clone();
            invalid["channel_map"] = map;
            assert!(raw_channel_volumes(&invalid).is_none());
        }
        let mut invalid = node.clone();
        invalid["volume"]["front-left"]["value"] = json!(0x8000_0000u32);
        assert!(raw_channel_volumes(&invalid).is_none());
        invalid["volume"]["front-left"]["value"] = json!(6.5);
        assert!(raw_channel_volumes(&invalid).is_none());
        let mut array_map = node;
        array_map["channel_map"] = json!(["front-right", "front-left"]);
        assert_eq!(raw_channel_volumes(&array_map), decoded[0].volume_channels);
    }

    #[test]
    fn raw_volume_action_validates_exact_current_values_and_channel_topology() {
        let channels = vec![
            VolumeChannel {
                name: "front-right".into(),
                value: 6553,
            },
            VolumeChannel {
                name: "front-left".into(),
                value: 13107,
            },
        ];
        let mut state = AudioSnapshot {
            capture_known: true,
            outputs: vec![AudioNode {
                name: "sink with spaces".into(),
                volume_channels: Some(channels.clone()),
                ..Default::default()
            }],
            inputs: vec![AudioNode {
                name: "microphone".into(),
                volume_channels: Some(channels.clone()),
                ..Default::default()
            }],
            ..Default::default()
        };
        for input in [false, true] {
            let name = if input {
                "microphone"
            } else {
                "sink with spaces"
            };
            assert_eq!(
                action_arguments(
                    &state,
                    AudioAction::SetRawVolume {
                        name: name.into(),
                        input,
                        expected: channels.clone(),
                        channels: channels.clone()
                    }
                )
                .expect("exact raw target"),
                vec![
                    if input {
                        "set-source-volume"
                    } else {
                        "set-sink-volume"
                    },
                    name,
                    "6553",
                    "13107"
                ]
            );
        }
        for (known, active) in [(false, false), (true, true)] {
            state.capture_known = known;
            state.capture_active = active;
            assert!(action_arguments(
                &state,
                AudioAction::SetRawVolume {
                    name: "sink with spaces".into(),
                    input: false,
                    expected: channels.clone(),
                    channels: channels.clone()
                }
            )
            .is_err());
        }
        state.capture_known = true;
        state.capture_active = false;
        let mut manual = channels.clone();
        manual[0].value += 1;
        state.outputs[0].volume_channels = Some(manual);
        assert!(action_arguments(
            &state,
            AudioAction::SetRawVolume {
                name: "sink with spaces".into(),
                input: false,
                expected: channels.clone(),
                channels: channels.clone()
            }
        )
        .is_err());
        state.outputs[0].volume_channels = Some(channels.clone());
        let mut reordered = channels.clone();
        reordered.reverse();
        for target in [
            reordered,
            vec![],
            vec![VolumeChannel {
                name: "front-right".into(),
                value: u32::MAX,
            }],
            vec![channels[0].clone(), channels[0].clone()],
        ] {
            assert!(action_arguments(
                &state,
                AudioAction::SetRawVolume {
                    name: "sink with spaces".into(),
                    input: false,
                    expected: channels.clone(),
                    channels: target
                }
            )
            .is_err());
        }
    }

    #[test]
    fn raw_volume_api_roundtrips_exact_values_and_rejects_manual_changes_before_effect() {
        const CHILD: &str = "LIBREPODS_RAW_AUDIO_FIXTURE";
        if let Some(path) = std::env::var_os(CHILD) {
            let path = std::path::PathBuf::from(path);
            let node = json!({"index":1,"name":"headset output with spaces","channel_map":"front-right,front-left","volume":{"front-left":{"value":65537},"front-right":{"value":32769}}});
            std::fs::write(&path, serde_json::to_vec(&node).expect("fixture encoding"))
                .expect("fixture");
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("runtime");
            runtime.block_on(async {
                let original = audio_snapshot("").await.expect("raw snapshot").outputs[0]
                    .volume_channels
                    .clone()
                    .expect("ordered values");
                let ducked: Vec<_> = original
                    .iter()
                    .map(|channel| VolumeChannel {
                        name: channel.name.clone(),
                        value: channel.value / 5,
                    })
                    .collect();
                let set = |expected, channels| AudioAction::SetRawVolume {
                    name: "headset output with spaces".into(),
                    input: false,
                    expected,
                    channels,
                };
                audio_action("", set(original.clone(), ducked.clone()))
                    .await
                    .expect("scale per channel");
                assert_eq!(
                    audio_snapshot("").await.expect("ducked snapshot").outputs[0]
                        .volume_channels
                        .as_ref(),
                    Some(&ducked)
                );
                audio_action("", set(ducked.clone(), original.clone()))
                    .await
                    .expect("exact restoration");
                assert_eq!(
                    audio_snapshot("").await.expect("restored snapshot").outputs[0]
                        .volume_channels
                        .as_ref(),
                    Some(&original)
                );
                audio_action("", set(original.clone(), ducked.clone()))
                    .await
                    .expect("second speech");
                let mut manual: Value =
                    serde_json::from_slice(&std::fs::read(&path).expect("current fixture"))
                        .expect("fixture JSON");
                manual["volume"]["front-right"]["value"] = json!(ducked[0].value + 1);
                std::fs::write(&path, serde_json::to_vec(&manual).expect("manual edit"))
                    .expect("manual volume");
                let error = audio_action("", set(ducked.clone(), original.clone()))
                    .await
                    .expect_err("manual volume ownership changed");
                assert_eq!(crate::error::kind(&error), Some(ErrorKind::InvalidInput));
                manual["volume"]["front-right"]["value"] = json!(ducked[0].value);
                std::fs::write(&path, serde_json::to_vec(&manual).expect("fixture"))
                    .expect("reset raw ownership");
                std::fs::write(path.with_extension("capture"), b"active")
                    .expect("capture starts before effect");
                let error = audio_action("", set(ducked, original))
                    .await
                    .expect_err("fresh capture suppresses automatic mutation");
                assert_eq!(crate::error::kind(&error), Some(ErrorKind::InvalidInput));
            });
            let actions: Vec<Vec<String>> = std::fs::read_to_string(path.with_extension("log"))
                .expect("actions")
                .lines()
                .map(|line| serde_json::from_str(line).expect("argument JSON"))
                .collect();
            assert_eq!(
                actions,
                vec![
                    vec![
                        "set-sink-volume",
                        "headset output with spaces",
                        "6553",
                        "13107"
                    ],
                    vec![
                        "set-sink-volume",
                        "headset output with spaces",
                        "32769",
                        "65537"
                    ],
                    vec![
                        "set-sink-volume",
                        "headset output with spaces",
                        "6553",
                        "13107"
                    ]
                ]
            );
            return;
        }
        use std::os::unix::fs::PermissionsExt;
        let directory = tempfile::tempdir().expect("fake audio directory");
        let program = directory.path().join("pactl");
        std::fs::write(&program,r##"#!/usr/bin/python3
import json,os,sys
from pathlib import Path
path=Path(os.environ['LIBREPODS_RAW_AUDIO_FIXTURE'])
args=sys.argv[1:]
if args[0]=='--format=json':
    if args[1:]==['info']: print('{}')
    elif args[1:]==['list','sinks']: print('['+path.read_text()+']')
    elif args[1:]==['list','source-outputs'] and path.with_suffix('.capture').exists(): print('[{"index":1,"source":999,"corked":false}]')
    else: print('[]')
else:
    with path.with_suffix('.log').open('a') as log: log.write(json.dumps(args)+'\n')
    node=json.loads(path.read_text())
    for channel,value in zip(node['channel_map'].split(','),args[2:]): node['volume'][channel]['value']=int(value)
    path.write_text(json.dumps(node))
"##).expect("fake pactl");
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700))
            .expect("executable");
        let output = std::process::Command::new(std::env::current_exe().expect("test executable")).args(["--exact","desktop::tests::raw_volume_api_roundtrips_exact_values_and_rejects_manual_changes_before_effect","--nocapture"]).env("PATH",directory.path()).env(CHILD,directory.path().join("volume.json")).output().expect("isolated audio child");
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    struct FakeNotifications(std::sync::Arc<std::sync::atomic::AtomicUsize>);
    // This signature follows org.freedesktop.Notifications.Notify's standard eight arguments.
    #[allow(clippy::too_many_arguments)]
    #[zbus::interface(name = "org.freedesktop.Notifications")]
    impl FakeNotifications {
        fn notify(
            &self,
            app_name: &str,
            replaces_id: u32,
            app_icon: &str,
            summary: &str,
            body: &str,
            actions: Vec<String>,
            hints: HashMap<String, zbus::zvariant::OwnedValue>,
            expire_timeout: i32,
        ) -> u32 {
            assert_eq!(app_name, "LibrePods");
            assert_eq!(replaces_id, 0);
            assert_eq!(app_icon, "audio-headphones");
            assert_eq!(summary, "test summary");
            assert_eq!(body, "test body");
            assert!(actions.is_empty() && hints.is_empty());
            assert_eq!(expire_timeout, 5000);
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            1
        }
    }

    #[test]
    fn user_bus_wrappers_confirm_playback_notify_and_bound_failed_actions() {
        const CHILD: &str = "LIBREPODS_DESKTOP_PRIVATE_BUS";
        if let Ok(case) = std::env::var(CHILD) {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .expect("private userbus runtime");
            runtime.block_on(async {
                if case == "unavailable" {
                    assert!(players().await.is_err());
                    let player = PlayerSnapshot {
                        bus_name: "org.mpris.MediaPlayer2.test".into(),
                        owner: ":1.99".into(),
                        playback_status: "Playing".into(),
                        can_pause: true,
                        can_play: true,
                    };
                    assert!(player_action(&player, PlayerAction::Pause).await.is_err());
                    assert!(notify("test summary", "test body").await.is_err());
                    return;
                }
                let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
                let service = zbus::connection::Builder::session()
                    .expect("session builder")
                    .name("org.mpris.MediaPlayer2.test")
                    .expect("playername")
                    .name("org.freedesktop.Notifications")
                    .expect("notificationname")
                    .serve_at(
                        "/org/mpris/MediaPlayer2",
                        FakePlayer {
                            playing: true,
                            ..Default::default()
                        },
                    )
                    .expect("player")
                    .serve_at(
                        "/org/freedesktop/Notifications",
                        FakeNotifications(calls.clone()),
                    )
                    .expect("notifications")
                    .build()
                    .await
                    .expect("private service");
                let _broken = zbus::connection::Builder::session()
                    .expect("broken service builder")
                    .name("org.mpris.MediaPlayer2.broken")
                    .expect("bad player name")
                    .build()
                    .await
                    .expect("player without MPRIS object");
                let discovered = players()
                    .await
                    .expect("userbus enumeration skips malformed player");
                assert_eq!(discovered.len(), 1);
                let player = discovered
                    .iter()
                    .find(|player| player.bus_name == "org.mpris.MediaPlayer2.test")
                    .expect("selected player");
                let paused = player_action(player, PlayerAction::Pause)
                    .await
                    .expect("confirmed userbus pause");
                assert_eq!(paused.playback_status, "Paused");
                let playing = player_action(&paused, PlayerAction::Play)
                    .await
                    .expect("confirmed userbus play");
                assert_eq!(playing.playback_status, "Playing");
                notify("test summary", "test body")
                    .await
                    .expect("userbus notification");
                assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
                let interface = service
                    .object_server()
                    .interface::<_, FakePlayer>("/org/mpris/MediaPlayer2")
                    .await
                    .expect("player interface");
                interface.get_mut().await.unsupported = true;
                assert_eq!(
                    crate::error::kind(
                        &player_action(&playing, PlayerAction::Pause)
                            .await
                            .expect_err("unsupported pause")
                    ),
                    Some(ErrorKind::Unsupported)
                );
                interface.get_mut().await.unsupported = false;
                interface.get_mut().await.stall = true;
                let started = std::time::Instant::now();
                assert_eq!(
                    crate::error::kind(
                        &player_action(&playing, PlayerAction::Pause)
                            .await
                            .expect_err("unconfirmed pause")
                    ),
                    Some(ErrorKind::Timeout)
                );
                assert!(started.elapsed() < Duration::from_secs(3));
                let invalid = PlayerSnapshot {
                    bus_name: "not a bus name".into(),
                    ..playing
                };
                assert!(player_action(&invalid, PlayerAction::Pause).await.is_err());
                service
                    .release_name("org.freedesktop.Notifications")
                    .await
                    .expect("notification service loss");
                assert!(notify("test summary", "test body").await.is_err());
            });
            return;
        }
        for case in ["normal", "unavailable"] {
            let bus = crate::test_bus::TestBus::start();
            let address = if case == "normal" {
                bus.address.as_str()
            } else {
                "unix:path=/no/such/librepods-private-bus"
            };
            let output=std::process::Command::new(std::env::current_exe().expect("test executable")).args(["--exact","desktop::tests::user_bus_wrappers_confirm_playback_notify_and_bound_failed_actions","--nocapture"]).env(CHILD,case).env("DBUS_SESSION_BUS_ADDRESS",address).output().expect("private userbus child");
            assert!(
                output.status.success(),
                "{case}: {}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }
}
