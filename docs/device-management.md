# Explicit Bluetooth device management

Device commands use BlueZ through the same persistent session owner as AACP
commands. They do not start another connection owner. The normal recovery loop
continues to operate only on the vendor channel; it never retries a Bluetooth
connect, disconnect or pairing removal automatically.

| CLI | Native operation | Completion means |
| --- | --- | --- |
| `device connect` | `device_connect` | Fresh BlueZ state confirms a connected paired device; AACP recovery is scheduled separately |
| `device disconnect` | `device_disconnect` | Fresh BlueZ state confirms disconnection; automatic AACP recovery is disabled |
| `device forget --yes` with `--address` | `device_forget` with `confirmed: true` inside `for_device` | Disconnection, pairing removal and selected-device private-key cleanup completed |
| `reconnect` | `reconnect` | Existing AACP-only recovery behavior; no BlueZ connection method is called |

`device connect` returns `device.bluetooth_connected`, `device.paired`,
`device.changed`, `aacp_ready`, `aacp_reconnect_scheduled` and
`connection_desired`. `device.changed` describes the Bluetooth state, not whether
the control channel was reconciled. AACP readiness remains observable through
status/watch; a successful Bluetooth connection does not prove usable audio.

An explicit address asserts the selected device and cannot retarget an already
bound daemon. Before initial selection, an explicit address can select a paired
AirPods device; without one, selection requires a single unambiguous paired
AirPods object. The adapter comes from that object, rather than a hardcoded
controller. Ambiguous identities, non-AirPods and unpaired connect targets are
rejected before the owner releases its channel. There is no implicit discovery,
pairing, trust change, adapter power change or privilege escalation.

The implementation follows the [BlueZ Device API](https://bluez.readthedocs.io/en/latest/device-api/)
and [Adapter API](https://bluez.readthedocs.io/en/latest/adapter-api/). It pins
the unique BlueZ bus owner for calls and validates it during readback, so a
service replacement cannot confirm an old operation. AlreadyConnected and
NotConnected replies require the same fresh postconditions as successful calls.

## Intent, deadlines and failure recovery

Device actions have FIFO priority over ordinary queued settings and can cancel
an active exchange. The owner invalidates the generation, cancels older ordinary
requests, and advances a device-intent epoch before effects. An already-written
setting is reported as uncertain and never replayed. Older channel-pending
requests and in-flight audio work cannot publish success across this boundary.
Later explicit device requests remain serialized rather than being coalesced.

BlueZ phases share the original request deadline, including queueing, BlueZ
lookup, disconnection before forget, pairing removal and readback. Cancellation
is checked before effects and during waits. BlueZ can finish an operation after
the client loses its reply; a timeout, interruption or service loss after dispatch
therefore returns `outcome_unknown`. Neither retries nor compensating Bluetooth
operations are issued automatically. Inspect status and the desktop Bluetooth
pairing state before repeating a request. Filesystem durability work retains the
existing persistence layer's OS I/O limitations; a caller can time out while
local cleanup finishes.

An intentional disconnect/forget disables AACP recovery through watchdog,
adapter changes and suspend/resume. This runtime intent is reset by restarting
the daemon, or explicitly re-enabled by `device connect`/`reconnect`. A definitive
refusal restores the prior control recovery policy; this may reopen AACP on an
already-connected device but does not repeat the BlueZ operation.

Bluetooth disconnection affects audio profiles too. These commands do not issue
extra Rust codec/default-routing changes; the desktop can still react normally
to a Bluetooth device appearing or disappearing. Profile, input, output and
stream selection remain separate explicit audio commands.

If pairing removal succeeds but private-key cleanup fails, the response is
`persistence_failed` and explicitly says that the pairing was removed. Other
devices' key entries are preserved. Fix the private-store problem and use
`keys forget` for cleanup; do not assume the pairing survived an error. Removing
keys also clears the reducer's private cached key values. This is logical key
removal, not a claim of cryptographic memory zeroization.

## Verification

Ordinary tests use a private fake BlueZ service and synthetic AACP peers. They
exercise the actual D-Bus methods, adapter selection, postconditions, owner loss,
policy denial, late completion, cancellation between forget phases and no replay.
An isolated child process also exercises the public system-bus entry point with
its bus address redirected to the private test bus. Parent environment and host
Bluetooth are untouched.

Daemon tests cover preemption, intentional disconnect through resume, initial
target selection, key cleanup/partial failure, identity mismatch and in-flight
audio cancellation. CLI process tests require explicit forget confirmation and
verify uncertainty after a lost mutation response. Scheduled stress includes
the device-management suite. Native hardware acceptance for these three new
commands has not been performed; the existing soak runs the prior master build.

The full suite also exposed a failed-startup ownership-lock race: closing a
locked file did not release it while a child retained the same open description.
A deterministic duplicate-description regression failed before explicit RAII
unlocking was added and passes afterward. The fix covers acquisition errors as
well as successful owner shutdown.
