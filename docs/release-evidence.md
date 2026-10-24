# Release acceptance evidence

Status: incomplete; do not merge for release yet. Implementation is committed on
`daemon-port`; physical reconnect/suspend/audio gates and the 24-hour soak remain
open. Automated evidence is recorded separately in [automated-evidence.md](automated-evidence.md).
A final instrumentation run also exposed a queued-client cancellation race under
investigation; no passing rerun or earlier checkpoint overrides that failure.

## Environment record

| Field | Evidence |
| --- | --- |
| Rust commit and tree cleanliness | Pending |
| Rust compiler and dependency lock hash | Pending |
| OS/kernel, BlueZ, PipeWire and WirePlumber versions | [Recorded host](evidence/2026-10-24/hardware-environment.json) |
| Adapter model/driver | Intel USB 8087:0033 / btusb; host record above |
| AirPods model and firmware | A3048, 90.3431000025000000.6807; host record above |
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
| Package restrictions | [Native and compatibility unit](evidence/2026-10-24/hardware-unit-smoke.json); [staged uninstall/reinstall](evidence/2026-10-24/package-uninstall-reinstall.json), original config preserved, no root/capabilities | Passed for staged units at recorded binaries; final deployment pending |
| Mode/settings | [20 mode transitions](evidence/2026-10-24/hardware-mode-cycles.json); [CA](evidence/2026-10-24/configuration-ca-readback.json); [Adaptive, one-bud ANC and levels 0/50/100](evidence/2026-10-24/configuration-settings-readback.json), with restoration | Hardware checks passed at their recorded binaries; final source reconciliation pending |
| Reconnect | [5 actual case cycles](evidence/2026-10-24/case-reconnect-smoke.json); original requirement 20 plus control-only drop and adapter/BlueZ restart | Partial; further manual testing stopped by user |
| Suspend | At least 10 real suspend/resume cycles, including queued-command handling | Unverified; manual testing stopped by user |
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

## Performance observations and open acceptance work

The 20 physical listening-mode transitions had p95 confirmation 2150.096 ms
and a maximum of 2155.729 ms, missing the initial 2000 ms target.
[Eight additional instrumented commands](evidence/2026-10-24/hardware-command-timing.json)
separated the host write and reply-publication time from the remote-report wait.
These observations describe this adapter/model/firmware only; they do not pass
the original target or establish audible playback continuity.

A completed 600-second diagnostic on the earlier `33e181f` application checkpoint
measured 0.1733% CPU of one core, p95 local status latency 3.378 ms and maximum
RSS 11644 KiB. RSS increased from its first sample; this is a warm-up diagnostic,
not the required 24-hour stability proof. [Redacted diagnostic summary](evidence/2026-10-24/idle-diagnostic.json);
raw private samples are in `target/idle-33e181f/`. The final 24-hour run has not started.

The user explicitly stopped manual testing after [5 successful physical
case/reconnect cycles](evidence/2026-10-24/case-reconnect-smoke.json). These
cycles restored fresh metadata automatically without an explicit Bluetooth
connect. Further manual testing is deferred at the user's request; the original
20-cycle gate is incomplete.

Unverified release checks: the remaining case/reconnect cycles, adapter/BlueZ restart,
10 suspend/resume cycles (including queued work), continuous playback listening,
profile-off and node recreation, pinned-stream/mute/zero-volume conditions,
audio-server restart, and explicit capture selection/restoration. Synthetic
coverage does not replace these gates. No previous daemon was initially active;
the staged install cycle therefore does not prove a full previous-daemon
migration and rollback. Optional BLE hardware coexistence remains unverified;
connected scanning remains disabled and BLE is experimental.
