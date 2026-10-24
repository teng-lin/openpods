// SPDX-License-Identifier: AGPL-3.0-only
//! Repeatable generated event traces. The oracle records externally observable
//! evidence and write instructions, rather than duplicating the command phases.
use librepods_core::device::state::{DeviceState, NoiseControlMode};
use librepods_core::protocol::{
    control_command::ControlCommandId,
    parser::ParsedPacket,
    settings::{CommandAction, CommandOutcome, PendingChange, SettingChange},
};

const ADDRESS: &str = "00:11:22:33:44:55";
const GENERATION: u64 = 7;
const DEADLINE: u64 = 400;
const SEEDS: u64 = 512;

struct Random(u64);
impl Random {
    fn next(&mut self, bound: u64) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (self.0 >> 32) % bound
    }
    fn shuffle<T>(&mut self, values: &mut [T]) {
        for index in (1..values.len()).rev() {
            values.swap(index, self.next(index as u64 + 1) as usize);
        }
    }
}

fn metadata() -> ParsedPacket {
    ParsedPacket::Metadata {
        name: "Synthetic Pods".into(),
        model_number: "A3048".into(),
        manufacturer: "Apple Inc.".into(),
    }
}
fn control(id: ControlCommandId, value: u8) -> ParsedPacket {
    ParsedPacket::ControlCommand {
        id,
        data: [value, 0, 0, 0],
    }
}
fn cases() -> [(SettingChange, Option<(ControlCommandId, u8)>); 4] {
    [
        (
            SettingChange::ListeningMode(NoiseControlMode::Transparency),
            None,
        ),
        (
            SettingChange::ConversationalAwareness(false),
            Some((ControlCommandId::ConversationDetectConfig, 2)),
        ),
        (
            SettingChange::SingleEarbudAnc(true),
            Some((ControlCommandId::OneBudAncMode, 1)),
        ),
        (
            SettingChange::AdaptiveNoiseLevel(75),
            Some((ControlCommandId::AutoAncStrength, 75)),
        ),
    ]
}
fn baseline(config: Option<(ControlCommandId, u8)>) -> DeviceState {
    let mut state = DeviceState::default();
    state.set_device_identity(ADDRESS, "Synthetic Pods");
    state.begin_session(GENERATION);
    state.apply_packet(&metadata(), GENERATION, 0);
    state.apply_packet(
        &ParsedPacket::NoiseControl(if config.is_some() {
            NoiseControlMode::Adaptive
        } else {
            NoiseControlMode::NoiseCancellation
        }),
        GENERATION,
        0,
    );
    if let Some((id, target)) = config {
        state.apply_packet(
            &control(
                id,
                if target == 2 {
                    1
                } else {
                    target.wrapping_add(1)
                },
            ),
            GENERATION,
            0,
        );
    }
    state
}

#[derive(Default)]
struct Evidence {
    sends: usize,
    written: bool,
    terminal: Option<CommandOutcome>,
}
impl Evidence {
    fn check(&mut self, action: CommandAction, now: u64, confirmation_witness: bool, seed: u64) {
        if let Some(outcome) = self.terminal {
            assert_eq!(
                action,
                CommandAction::Complete(outcome),
                "terminal request changed, seed={seed}"
            );
            return;
        }
        match action {
            CommandAction::Send(_) => {
                self.sends += 1;
                assert_eq!(self.sends, 1, "setting replayed, seed={seed}");
                assert!(now < DEADLINE, "expired request wrote, seed={seed}");
                assert!(
                    !self.written,
                    "written request dispatched again, seed={seed}"
                );
            }
            CommandAction::Complete(outcome) => {
                match outcome {
                    CommandOutcome::Confirmed => assert!(
                        self.written && confirmation_witness,
                        "confirmation without fresh external evidence, seed={seed}"
                    ),
                    CommandOutcome::OutcomeUnknown => {
                        assert!(self.written, "unwritten request uncertain, seed={seed}")
                    }
                    CommandOutcome::TimedOut
                    | CommandOutcome::Cancelled
                    | CommandOutcome::Superseded => assert!(
                        !self.written,
                        "written request claimed certainty, seed={seed}"
                    ),
                    _ => {}
                }
                self.terminal = Some(outcome);
            }
            _ => {}
        }
    }
}

#[test]
fn generated_success_traces_require_fresh_target_and_new_metadata_in_either_order() {
    let mut confirmed = [0_usize; 4];
    let mut metadata_first = 0;
    let mut config_first = 0;
    for seed in 0..SEEDS {
        let mut random = Random(seed ^ 0x7b57_12da_29f3);
        for (case, (change, config)) in cases().into_iter().enumerate() {
            let mut state = baseline(config);
            let mut machine = PendingChange::new(change, GENERATION, DEADLINE);
            let mut evidence = Evidence::default();
            let mut now = 1;
            evidence.check(machine.begin(&state, now), now, false, seed);
            assert_eq!(evidence.sends, 1);
            evidence.written = true; // The emulated owner completed that one write.
            let sent = machine.mark_sent(GENERATION, now);
            let generation = if config.is_some() {
                assert_eq!(
                    sent,
                    CommandAction::ReopenForReadback {
                        generation: GENERATION + 1
                    }
                );
                state.begin_session(GENERATION + 1);
                now += 1;
                evidence.check(
                    machine.mark_readback_reopened(GENERATION + 1, now),
                    now,
                    false,
                    seed,
                );
                GENERATION + 1
            } else {
                evidence.check(sent, now, false, seed);
                GENERATION
            };
            // Shuffle prerequisite observations, unrelated packets and stale
            // reducer input. The witness is tracked from inputs, not machine state.
            let mut events = vec![0, 1, 2, 3, 4, 4, 4, 4];
            random.shuffle(&mut events);
            if config.is_some() {
                if events.iter().position(|event| *event == 0)
                    < events.iter().position(|event| *event == 1)
                {
                    metadata_first += 1;
                } else {
                    config_first += 1;
                }
            }
            let mut fresh_metadata = false;
            let mut fresh_target = false;
            for event in events {
                now += 1 + random.next(3);
                let (packet, packet_generation) = match event {
                    0 => {
                        fresh_metadata = true;
                        (metadata(), generation)
                    }
                    1 => {
                        fresh_target = true;
                        (
                            config.map_or(
                                ParsedPacket::NoiseControl(NoiseControlMode::Transparency),
                                |(id, target)| control(id, target),
                            ),
                            generation,
                        )
                    }
                    2 => (metadata(), generation - 1),
                    3 => (
                        config.map_or(
                            ParsedPacket::NoiseControl(NoiseControlMode::Transparency),
                            |(id, target)| control(id, target),
                        ),
                        generation - 1,
                    ),
                    _ => (ParsedPacket::Unknown(vec![0x55, 0xaa]), generation),
                };
                state.apply_packet(&packet, packet_generation, now);
                let action = if config.is_some() {
                    machine.observe_readback(&state, now)
                } else {
                    machine.on_packet(&packet, packet_generation, now)
                };
                // Old-generation packets are ignored by the reducer. For live
                // ACK commands, their delivery cancels the transaction instead.
                evidence.check(
                    action,
                    now,
                    fresh_target && (config.is_none() || fresh_metadata),
                    seed,
                );
            }
            if config.is_some() {
                assert_eq!(
                    evidence.terminal,
                    Some(CommandOutcome::Confirmed),
                    "seed={seed}"
                );
            }
            if evidence.terminal == Some(CommandOutcome::Confirmed) {
                confirmed[case] += 1;
            }
            assert_eq!(evidence.sends, 1);
            terminal_tail(&mut machine, &state, &mut evidence, &mut random, now, seed);
        }
    }
    assert!(confirmed[0] > 0, "live ACK success must be exercised");
    assert_eq!(&confirmed[1..], &[SEEDS as usize; 3]);
    assert!(
        metadata_first > 0 && config_first > 0,
        "both fresh readback orders must execute"
    );
}

fn terminal_tail(
    machine: &mut PendingChange,
    state: &DeviceState,
    evidence: &mut Evidence,
    random: &mut Random,
    mut now: u64,
    seed: u64,
) {
    // Move well beyond the deadline, mixing duplicate sends, fresh/old reports,
    // generation changes, cancellation and reconnect completion after termination.
    for _ in 0..32 {
        now += 1 + random.next(100);
        let generation = GENERATION + random.next(4);
        let action = match random.next(7) {
            0 => machine.begin(state, now),
            1 => machine.mark_sent(generation, now),
            2 => machine.on_packet(&metadata(), generation, now),
            3 => machine.mark_readback_reopened(generation, now),
            4 => machine.tick(generation, now),
            5 => machine.cancel(),
            _ => machine.transport_lost(),
        };
        evidence.check(action, now, false, seed);
    }
}

#[test]
fn generated_interruption_traces_never_replay_or_hide_written_uncertainty() {
    let mut interruptions = [0_usize; 5];
    let mut reopened = 0;
    for seed in 0..SEEDS {
        let mut random = Random(seed ^ 0xae91_7ac5_403b);
        for (change, config) in cases() {
            for written in [false, true] {
                let mut state = baseline(config);
                let mut generation = GENERATION;
                let mut machine = PendingChange::new(change.clone(), GENERATION, DEADLINE);
                let mut evidence = Evidence::default();
                let mut now = 1;
                evidence.check(machine.begin(&state, now), now, false, seed);
                if written {
                    evidence.written = true;
                    evidence.check(machine.mark_sent(generation, now), now, false, seed);
                    if let Some((id, target)) = config.filter(|_| random.next(2) == 0) {
                        reopened += 1;
                        generation += 1;
                        state.begin_session(generation);
                        now += 1;
                        evidence.check(
                            machine.mark_readback_reopened(generation, now),
                            now,
                            false,
                            seed,
                        );
                        // A matching field alone in the reopened session cannot
                        // confirm until metadata independently establishes identity.
                        state.apply_packet(&control(id, target), generation, now);
                        evidence.check(machine.observe_readback(&state, now), now, false, seed);
                    }
                }
                // Unrelated events and duplicate begin calls
                // precede a generated cancellation, loss, stale generation or expiry.
                for _ in 0..random.next(12) {
                    now += 1 + random.next(4);
                    let action = if random.next(2) == 0 {
                        machine.begin(&state, now)
                    } else {
                        machine.on_packet(&ParsedPacket::Unknown(vec![0x42]), generation, now)
                    };
                    evidence.check(action, now, false, seed);
                }
                let interruption = random.next(5) as usize;
                interruptions[interruption] += 1;
                let action = match interruption {
                    0 => machine.cancel(),
                    1 => machine.transport_lost(),
                    2 => machine.supersede(),
                    3 => machine.on_packet(&metadata(), generation - 1, now),
                    _ => {
                        now = DEADLINE + random.next(100);
                        machine.tick(generation, now)
                    }
                };
                evidence.check(action, now, false, seed);
                assert!(
                    evidence.terminal.is_some(),
                    "interruption did not terminate, seed={seed}"
                );
                if written {
                    assert_eq!(
                        evidence.terminal,
                        Some(CommandOutcome::OutcomeUnknown),
                        "seed={seed}"
                    );
                }
                terminal_tail(&mut machine, &state, &mut evidence, &mut random, now, seed);
                assert_eq!(evidence.sends, 1);
            }
        }
    }
    assert!(interruptions.into_iter().all(|count| count > 0));
    assert!(
        reopened > 0,
        "readback-confirmation interruptions must execute"
    );
}

#[test]
fn generated_expired_admissions_never_issue_an_initial_write() {
    for seed in 0..SEEDS {
        let mut random = Random(seed ^ 0xcd37_9481_6eba);
        for (change, config) in cases() {
            let state = baseline(config);
            let now = DEADLINE + if seed == 0 { 0 } else { random.next(10_000) };
            let mut machine = PendingChange::new(change, GENERATION, DEADLINE);
            let mut evidence = Evidence::default();
            evidence.check(machine.begin(&state, now), now, false, seed);
            assert_eq!(evidence.terminal, Some(CommandOutcome::TimedOut));
            terminal_tail(&mut machine, &state, &mut evidence, &mut random, now, seed);
            assert_eq!(evidence.sends, 0, "expired admission wrote, seed={seed}");
        }
    }
}
