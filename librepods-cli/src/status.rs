// LibrePods - AirPods liberated from Apple's ecosystem
// Copyright (C) 2025 LibrePods contributors
// SPDX-License-Identifier: AGPL-3.0-only

//! Terminal presentation of values reported by the current AirPods connection.

use std::fmt::Write as _;
use std::io::{self, IsTerminal, Write};

use librepods_core::device::battery::{BatteryState, BatteryStatus};
use librepods_core::device::state::DeviceState;
use librepods_core::protocol::control_command::ControlCommandId;

pub struct Display {
    previous: String,
    terminal: bool,
}

impl Display {
    pub fn new() -> Self {
        Self {
            previous: String::new(),
            terminal: io::stdout().is_terminal(),
        }
    }

    pub fn update(&mut self, state: &DeviceState) -> io::Result<()> {
        let next = render(state);
        if next != self.previous {
            let mut stdout = io::stdout().lock();
            if self.terminal {
                write!(stdout, "\x1b[2J\x1b[H")?;
            }
            stdout.write_all(next.as_bytes())?;
            stdout.flush()?;
            self.previous = next;
        }
        Ok(())
    }
}

fn text(value: &str) -> String {
    if value.is_empty() {
        "Not reported".into()
    } else {
        // Device-provided names must not inject terminal control sequences.
        value.chars().filter(|c| !c.is_control()).collect()
    }
}

fn battery(reading: BatteryState) -> String {
    match reading.status {
        BatteryStatus::Disconnected => "Unavailable".into(),
        BatteryStatus::Charging => format!("{}% (charging)", reading.level),
        BatteryStatus::Discharging => format!("{}% (discharging)", reading.level),
        BatteryStatus::OptimizedCharging => format!("{}% (charging paused)", reading.level),
    }
}

fn label(id: u8) -> String {
    match ControlCommandId::from_byte(id) {
        Some(ControlCommandId::Ssl) => "SSL (raw)".into(),
        Some(id) => {
            let name = format!("{id:?}");
            let mut spaced = String::new();
            for (index, ch) in name.chars().enumerate() {
                if index > 0 && ch.is_uppercase() {
                    spaced.push(' ');
                }
                spaced.push(ch);
            }
            spaced
        }
        None => "Unknown setting".into(),
    }
}

pub fn render(state: &DeviceState) -> String {
    let mut output = String::new();
    // Writing to a String cannot fail.
    let _ = writeln!(output, "AirPods live status — Ctrl+C to exit\n");
    for (name, value) in [
        ("Device", text(&state.device_name)),
        ("Model", text(&state.model_number)),
        ("Manufacturer", text(&state.manufacturer)),
        ("Bluetooth address", text(&state.bluetooth_address)),
        ("Left battery", battery(state.battery.left)),
        ("Right battery", battery(state.battery.right)),
        ("Case battery", battery(state.battery.case_)),
        (
            "Listening mode",
            if state.noise_control_reported {
                format!("{:?}", state.noise_control)
            } else {
                "Not reported".into()
            },
        ),
        (
            "Ear detection",
            if state.ear_detection_reported {
                format!(
                    "primary: {}, secondary: {}",
                    if state.primary_in_ear {
                        "in-ear"
                    } else {
                        "out"
                    },
                    if state.secondary_in_ear {
                        "in-ear"
                    } else {
                        "out"
                    },
                )
            } else {
                "Not reported".into()
            },
        ),
    ] {
        let _ = writeln!(output, "{name:<22} {value}");
    }
    let _ = writeln!(output, "\nReported settings (four raw bytes, hexadecimal):");
    if state.reported_controls.is_empty() {
        let _ = writeln!(output, "Waiting for setting notifications…");
    }
    for (id, data) in &state.reported_controls {
        let _ = write!(
            output,
            "0x{id:02X}  {:<30} {:02X} {:02X} {:02X} {:02X}",
            label(*id),
            data[0],
            data[1],
            data[2],
            data[3],
        );
        // Only these existing boolean fields have a verified 01/02 convention.
        if matches!(
            ControlCommandId::from_byte(*id),
            Some(ControlCommandId::ConversationDetectConfig | ControlCommandId::OneBudAncMode)
        ) && data[1..] == [0, 0, 0]
        {
            match data[0] {
                1 => output.push_str("  Enabled"),
                2 => output.push_str("  Disabled"),
                _ => {}
            }
        }
        output.push('\n');
    }
    let _ = writeln!(
        output,
        "\nOnly received values are shown; unavailable does not mean 0%."
    );
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unreported_values_are_not_presented_as_real_measurements() {
        let rendered = render(&DeviceState::default());
        assert!(rendered.contains("Unavailable"));
        assert!(rendered.contains("Not reported"));
        assert!(!rendered.contains("0% ("));
        assert!(!rendered.contains("Off\n"));
        assert_eq!(
            battery(BatteryState {
                level: 0,
                status: BatteryStatus::Charging
            }),
            "0% (charging)"
        );
    }

    #[test]
    fn unknown_settings_survive_without_being_interpreted_as_booleans() {
        let mut state = DeviceState::default();
        state.reported_controls.insert(0x3E, [1, 2, 3, 4]);
        let rendered = render(&state);
        assert!(rendered.contains("Unknown setting"));
        assert!(rendered.contains("01 02 03 04"));
        assert!(!rendered.contains("Enabled"));
    }

    #[test]
    fn device_names_cannot_emit_terminal_escape_sequences() {
        let state = DeviceState {
            device_name: "Pods\x1b[2J\n".into(),
            ..DeviceState::default()
        };
        assert!(!render(&state).contains('\x1b'));
    }
}
