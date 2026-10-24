# Release acceptance evidence

Status: pending. This file is a checklist template, not completed acceptance.
Do not mark a gate passed without linking the exact command output or recording.

## Environment record

| Field | Evidence |
| --- | --- |
| Rust commit and tree cleanliness | Pending |
| Rust compiler and dependency lock hash | Pending |
| OS/kernel, BlueZ, PipeWire and WirePlumber versions | Pending |
| Adapter model/driver | Pending |
| AirPods model and firmware | Pending |
| Build features and runtime BLE state | Pending |
| Original listening mode, CA, one-bud ANC and adaptive level | Pending |
| Original profile, defaults and stream routes | Pending |
| Restoration result after each hardware test | Pending |

## Gates

| Gate | Required evidence | Status |
| --- | --- | --- |
| Automated matrix | Linux/Windows default and BLE variants, Android core compile, fmt/Clippy | Pending |
| Coverage | Measured JSON, 90% core/80% service line gate, critical transition tests | Pending |
| Protocol robustness | Corpus replay, fuzz artifacts/regressions, Miri or documented platform limitation | Pending |
| Native IPC | Concurrent clients, deadline expiry, unknown mutation outcome, stop/restart behavior | Pending |
| Legacy integration | Rust-generated snapshots accepted by actual Omarchy Model.js | Pending |
| Package restrictions | Real user-unit install/start/stop/restart/uninstall and permission evidence | Pending |
| Mode/settings | At least 20 accepted mode transitions, Adaptive/CA/one-bud/adaptive-level as supported | Pending |
| Reconnect | At least 20 case/reconnect cycles, control-only drop, adapter/BlueZ restart | Pending |
| Suspend | At least 10 real suspend/resume cycles, including queued-command handling | Pending |
| Audio | Playback continuity, profile-off/node recreation/mute/wrong route, server restart, user override | Pending |
| Capture | External mic preserved; explicit AirPods capture profile choice and restoration | Pending |
| No-BLE telemetry | Case unavailable vs genuine zero, partial earbud reports, no BLE dependency/runtime task | Pending |
| BLE optional isolation | Absent/disabled/failing scanner cannot break control; separate hardware coexistence matrix | Pending |
| Performance | Ten-minute idle measurement, 24-hour no-BLE soak, p95 status/command latency, memory trend | Pending |
| Migration/rollback | Saved old service/panel state, no duplicate connection owners, restored original state | Pending |

Record each hardware cycle with timestamp, expected outcome, actual outcome and
restoration status. Synthetic adapter events are automated coverage, not real
case/suspend cycles. A process that is merely still running is not measured soak
evidence.

## Soak capture

After starting the release daemon in its production user unit, attach without
changing its settings:

```bash
python3 scripts/soak.py --pid DAEMON_PID \
  --binary /absolute/path/to/librepods-cli \
  --output /private/path/to/new-soak-directory \
  --source-commit EXACT_COMMIT --source-tree-hash EXACT_TREE \
  --source-tree-state clean --unit librepods-rs.service
```

The default duration is 86,400 seconds. The runner samples the same process start
identity, CPU, RSS and native status latency, checks that BLE is explicitly off,
and keeps summary/JSONL evidence. Shorter diagnostic runs are allowed but cannot
claim the release-duration gate. Review RSS growth as well as peak memory.
Snapshots in evidence must be redacted before sharing; never retain BLE keys.

The runner hashes `/proc/PID/exe` and the requested executable and rejects a
mismatch or any subsequent process/binary replacement. It records the actual
executable identity and daemon instance, and requires every status sample to
prove both runtime BLE disabled and BLE support absent from the build.

Source commit/tree metadata describes the declared build input; it is not a
cryptographic build attestation. Unknown or dirty source metadata cannot satisfy
the release soak result. The optional `--unit` collects only bounded journal
volume counts for the measured time window; it retains no journal content.
Truncated, timed-out or absent journal evidence also prevents a complete release
soak claim. A shorter diagnostic run can still produce useful samples.
