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

const TIMEOUT: Duration = Duration::from_secs(2);
const MAX_OUTPUT: u64 = 4 * 1024 * 1024;

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct AudioNode {
    pub index: u32,
    pub name: String,
    pub selected_device: bool,
    pub muted: bool,
    pub volume_percent: Option<f64>,
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
    pub default_output: Option<String>,
    pub default_input: Option<String>,
    pub profile: Option<String>,
    pub profiles: Vec<AudioProfile>,
    pub outputs: Vec<AudioNode>,
    pub inputs: Vec<AudioNode>,
    pub playback_streams: Vec<AudioStream>,
    pub recording_streams: Vec<AudioStream>,
    pub capture_active: bool,
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
    let mut command = Command::new("pactl");
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
    let result = tokio::time::timeout(TIMEOUT, async {
        let mut output = Vec::new();
        let mut error = Vec::new();
        let mut stdout = stdout.take(MAX_OUTPUT + 1);
        let mut stderr = stderr.take(64 * 1024);
        let (out, err, status) = tokio::join!(
            stdout.read_to_end(&mut output),
            stderr.read_to_end(&mut error),
            child.wait()
        );
        out?;
        err?;
        Ok::<_, std::io::Error>((status?, output, error))
    })
    .await
    .map_err(|_| PlatformError::new("audio API operation", ErrorKind::Timeout))?
    .map_err(|error| PlatformError::io("audio API operation", error))?;
    if result.1.len() > MAX_OUTPUT as usize {
        return Err(PlatformError::new(
            "audio API output exceeds size limit",
            ErrorKind::Truncated,
        )
        .into());
    }
    if !result.0.success() {
        return Err(TransportError::with_source(
            "audio server rejected operation",
            std::io::Error::other(String::from_utf8_lossy(&result.2).trim().to_owned()),
        ));
    }
    Ok(result.1)
}

async fn json(arguments: &[&str]) -> Result<Value, TransportError> {
    let mut args = vec!["--format=json"];
    args.extend_from_slice(arguments);
    serde_json::from_slice(&pactl(&args).await?)
        .map_err(|error| TransportError::with_source("decode audio API JSON", error))
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
    AudioSnapshot {
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
    }
}

/// Perform only the requested mutation, after resolving current node/profile state.
pub async fn audio_action(address: &str, action: AudioAction) -> Result<(), TransportError> {
    let state = audio_snapshot(address).await?;
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
            let cards = json(&["list", "cards"]).await?;
            let name = cards
                .as_array()
                .into_iter()
                .flatten()
                .find(|card| device_matches(card, address))
                .and_then(|card| string(card, "name"))
                .ok_or_else(|| {
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
    pactl(&args.iter().map(String::as_str).collect::<Vec<_>>()).await?;
    Ok(())
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
        let mut players = Vec::new();
        for name in names
            .into_iter()
            .filter(|name| name.as_str().starts_with("org.mpris.MediaPlayer2."))
            .take(64)
        {
            let owner =
                match bus
                    .get_name_owner(name.as_str().try_into().map_err(|error| {
                        TransportError::with_source("validate player name", error)
                    })?)
                    .await
                {
                    Ok(owner) => owner,
                    Err(_) => continue,
                };
            if let Ok(player) = player_on(connection, name.as_str(), owner.as_str()).await {
                players.push(player);
            }
        }
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

    struct FakePlayer {
        playing: bool,
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
            true
        }
        #[zbus(property)]
        fn can_play(&self) -> bool {
            true
        }
        fn pause(&mut self) {
            self.playing = false;
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
            .serve_at("/org/mpris/MediaPlayer2", FakePlayer { playing: true })
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
            .serve_at("/org/mpris/MediaPlayer2", FakePlayer { playing: true })
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
}
