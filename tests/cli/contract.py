#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-only
"""Run the actual CLI against isolated native IPC peers; never access Bluetooth."""
import argparse
import fcntl
import json
import os
from pathlib import Path
import socket
import signal
import time
import subprocess
import tempfile
import threading
import unittest

BINARY = None
BLE_BUILD = False
FIXTURES = Path(__file__).resolve().parents[2] / "target/native-fixtures"


class CliContract(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="pods-cli-")
        self.addCleanup(self.temp.cleanup)
        root = Path(self.temp.name)
        self.env = {**os.environ, "HOME": str(root), "XDG_RUNTIME_DIR": str(root),
                    "XDG_CONFIG_HOME": str(root / "config"),
                    "XDG_STATE_HOME": str(root / "state"), "RUST_LOG": "off"}
        self.env["PULSE_SERVER"] = "unix:" + str(root / "no-pulse-server")
        self.env["PIPEWIRE_REMOTE"] = "librepods-test-no-pipewire"
        self.env["DBUS_SYSTEM_BUS_ADDRESS"] = "unix:path=" + str(root / "no-system-bus")
        self.env["DBUS_SESSION_BUS_ADDRESS"] = "unix:path=" + str(root / "no-session-bus")
        (root / "librepods-rs").mkdir(mode=0o700)
        self.socket = root / "librepods-rs/control.sock"

    def call(self, *args, machine=True):
        return subprocess.run([str(BINARY), *(["--json"] if machine else []), *args], env=self.env,
                              capture_output=True, text=True, timeout=5)

    def exchange(self, args, response, machine=True):
        received, failures = [], []
        server = socket.socket(socket.AF_UNIX)
        self.addCleanup(server.close)
        server.bind(str(self.socket))
        server.listen(1)
        server.settimeout(3)

        def peer():
            try:
                conn, _ = server.accept()
                with conn:
                    conn.settimeout(3)
                    line = conn.makefile("rb").readline(16386)
                    received.append(json.loads(line))
                    wire = response(received[0])
                    conn.sendall(wire if isinstance(wire, bytes) else
                                 json.dumps(wire).encode() + b"\n")
            except Exception as error:
                failures.append(error)

        thread = threading.Thread(target=peer, daemon=True)
        thread.start()
        result = self.call(*args, machine=machine)
        thread.join(4)
        server.close()
        self.socket.unlink(missing_ok=True)
        self.assertFalse(thread.is_alive(), "IPC peer did not finish")
        self.assertEqual(failures, [])
        self.assertEqual(len(received), 1)
        return result, received[0]

    @staticmethod
    def ok(request):
        return {"version": 1, "id": request["id"], "result": {"marker": "ok"},
                "error": None}

    def test_status_prints_result_and_uses_versioned_native_protocol(self):
        result, request = self.exchange(["status"], self.ok)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(json.loads(result.stdout), {"marker": "ok"})
        self.assertEqual(request["version"], 1)
        self.assertEqual(request["operation"], {"type": "status"})

    def test_control_audio_and_policy_verbs_forward_typed_operations(self):
        cases = [
            (["doctor"], {"type": "doctor"}),
            (["capabilities"], {"type": "capabilities"}),
            (["reconnect"], {"type": "reconnect"}),
            (["ble", "on"], {"type": "ble_enabled", "value": True}),
            (["ble", "off"], {"type": "ble_enabled", "value": False}),
            (["keys", "fetch"], {"type": "fetch_keys"}),
            (["keys", "forget"], {"type": "forget_keys"}),
            (["adaptive-level", "42"], {"type": "change", "value": {"AdaptiveNoiseLevel": 42}}),
            (["rename", "Example AirPods"], {"type": "change", "value": {"Rename": "Example AirPods"}}),
            (["long-press-modes", "13"], {"type": "change", "value": {"LongPressModes": 13}}),
            (["--one-bud-anc", "off"], {"type": "change", "value": {"SingleEarbudAnc": False}}),
            (["--noise-control", "off"], {"type": "change", "value": {"ListeningMode": "Off"}}),
            (["--noise-control", "transparency"], {"type": "change", "value": {"ListeningMode": "Transparency"}}),
            (["--noise-control", "adaptive"], {"type": "change", "value": {"ListeningMode": "Adaptive"}}),
            (["audio", "unmute"], {"type": "unmute"}),
            (["audio", "output", "example-sink"], {"type": "set_output", "value": "example-sink"}),
            (["--noise-control", "anc"], {"type": "change", "value": {"ListeningMode": "NoiseCancellation"}}),
            (["--conversation-awareness", "off"], {"type": "change", "value": {"ConversationalAwareness": False}}),
            (["compat", "onebud:on"], {"type": "change", "value": {"SingleEarbudAnc": True}}),
            (["ear", "both"], {"type": "ear_policy", "value": "both_removed"}),
            (["ear", "off"], {"type": "ear_policy", "value": "off"}),
            (["ear", "one"], {"type": "ear_policy", "value": "one_removed"}),
            (["audio", "input", "example-source"], {"type": "set_input", "value": "example-source"}),
            (["audio", "profile", "a2dp-sink"], {"type": "set_profile", "value": "a2dp-sink"}),
            (["audio", "move-stream", "17", "example-sink"], {"type": "move_stream", "value": {"stream": "17", "output": "example-sink"}}),
        ]
        for args, expected in cases:
            with self.subTest(args=args):
                result, request = self.exchange(args, self.ok)
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(request["operation"], expected)

    def test_watch_consumes_actual_serializer_snapshots(self):
        snapshots = [json.loads((FIXTURES / name).read_text())
                     for name in ["pro2.json", "full-controls.json", "disconnected.json"]]
        def frames(request):
            return b"".join(json.dumps({"version": 1, "id": request["id"],
                "result": snapshot, "error": None}).encode() + b"\n"
                for snapshot in snapshots)
        result, request = self.exchange(["--status"], frames)
        self.assertEqual(request["operation"], {"type": "watch"})
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual([json.loads(line) for line in result.stdout.splitlines()], snapshots)
        self.assertTrue(snapshots[0]["connected"])
        self.assertFalse(snapshots[-1]["connected"])
        self.assertTrue(snapshots[-1]["device"]["observations"]["battery.left"]["stale"])

    def test_status_consumes_complete_snapshot_larger_than_request_frame_limit(self):
        snapshot = json.loads((FIXTURES / "full-controls.json").read_text())
        self.assertGreater(len(json.dumps(snapshot)), 16384)
        result, _ = self.exchange(["status"], lambda req: {
            "version": 1, "id": req["id"], "result": snapshot, "error": None})
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(json.loads(result.stdout), snapshot)
        self.assertEqual(len(snapshot["device"]["reported_controls"]), 256)

    def test_terminal_watch_preserves_unknown_settings_and_component_availability(self):
        snapshot = json.loads((FIXTURES / "full-controls.json").read_text())
        result, _ = self.exchange(["--status"], lambda req: {
            "version": 1, "id": req["id"], "result": snapshot, "error": None}, machine=False)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("0xFE", result.stdout)
        self.assertIn("Unknown setting", result.stdout)
        self.assertIn("Disabled", result.stdout)
        left = next(line for line in result.stdout.splitlines() if line.startswith("Left battery"))
        right = next(line for line in result.stdout.splitlines() if line.startswith("Right battery"))
        self.assertIn("100%", left)
        self.assertIn("Unavailable", right)
        self.assertNotIn("0% (", right)

    def test_terminal_watch_marks_retained_battery_as_stale_after_disconnect(self):
        snapshot = json.loads((FIXTURES / "disconnected.json").read_text())
        result, _ = self.exchange(["--status"], lambda req: {
            "version": 1, "id": req["id"], "result": snapshot, "error": None}, machine=False)
        self.assertEqual(result.returncode, 0, result.stderr)
        reading = next(line for line in result.stdout.splitlines() if line.startswith("Left battery"))
        self.assertRegex(reading.lower(), r"stale|cached|unavailable",
            "a retained disconnected measurement must not appear live")

    def test_compat_status_matches_actual_legacy_serializer(self):
        snapshot = json.loads((FIXTURES / "pro2.json").read_text())
        expected = json.loads((FIXTURES.parent / "compat-fixtures/pro2.json").read_text())
        result, request = self.exchange(["compat", "status"], lambda req: {
            "version": 1, "id": req["id"], "result": snapshot, "error": None})
        self.assertEqual(request["operation"], {"type": "status"})
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(json.loads(result.stdout), expected)

    def test_remote_error_is_structured_and_nonzero(self):
        error = {"code": "unsupported", "stage": "audio", "message": "no backend",
                 "retryable": False}
        result, _ = self.exchange(["audio", "status"], lambda req: {
            "version": 1, "id": req["id"], "result": None, "error": error})
        self.assertEqual(result.returncode, 1)
        self.assertEqual(result.stdout, "")
        self.assertEqual(json.loads(result.stderr), error)

    def test_post_dispatch_transport_and_invalid_response_are_uncertain_only_for_mutations(self):
        cases = [
            (lambda req: b"", "service_unavailable"),
            (lambda req: b"not JSON\n", "malformed_frame"),
            (lambda req: {"version": 1, "id": req["id"] + 1, "result": {}, "error": None}, "version_mismatch"),
            (lambda req: {"version": 1, "id": req["id"], "result": None, "error": None}, "malformed_frame"),
        ]
        for response, readonly_code in cases:
            for args, expected in [(["status"], readonly_code), (["--conversation-awareness", "off"], "outcome_unknown")]:
                with self.subTest(expected=expected, readonly_code=readonly_code):
                    result, _ = self.exchange(args, response)
                    self.assertEqual(result.returncode, 1)
                    self.assertEqual(result.stdout, "")
                    self.assertEqual(json.loads(result.stderr)["code"], expected)

    def test_typed_daemon_rejection_of_mutation_keeps_its_precise_error(self):
        error = {"code": "unsupported", "stage": "capabilities", "message": "unsupported setting", "retryable": False}
        result, _ = self.exchange(["--conversation-awareness", "off"], lambda req: {
            "version": 1, "id": req["id"], "result": None, "error": error})
        self.assertEqual(result.returncode, 1)
        self.assertEqual(json.loads(result.stderr), error)

    def test_watch_validates_correlation_and_payload_invariant_before_daemon_error(self):
        error = {"code": "unsupported", "stage": "watch", "message": "example rejection", "retryable": False}
        cases = [
            (lambda req: {"version": 1, "id": req["id"] + 1, "result": None, "error": error}, "version_mismatch"),
            (lambda req: {"version": 99, "id": req["id"], "result": None, "error": error}, "version_mismatch"),
            (lambda req: {"version": 1, "id": req["id"], "result": {}, "error": error}, "malformed_frame"),
            (lambda req: {"version": 1, "id": req["id"], "result": None, "error": error}, "unsupported"),
        ]
        for response, expected in cases:
            with self.subTest(expected=expected):
                result, _ = self.exchange(["--status"], response)
                self.assertEqual(result.returncode, 1)
                self.assertEqual(result.stdout, "")
                self.assertEqual(json.loads(result.stderr)["code"], expected)

    def test_foreign_response_identifier_and_version_are_rejected(self):
        for version, identifier in [(1, 99), (99, 1)]:
            with self.subTest(version=version, identifier=identifier):
                result, _ = self.exchange(["status"], lambda _: {
                    "version": version, "id": identifier, "result": {}, "error": None})
                self.assertEqual(result.returncode, 1)
                self.assertEqual(json.loads(result.stderr)["code"], "version_mismatch")

    def test_bad_or_missing_result_does_not_print_success(self):
        for response in [b"not JSON\n", b"{\"version\":1,\"id\":1,\"result\":null,\"error\":null}\n"]:
            with self.subTest(response=response):
                result, _ = self.exchange(["status"], lambda _: response)
                self.assertEqual(result.returncode, 1)
                self.assertEqual(result.stdout, "")
                self.assertEqual(json.loads(result.stderr)["code"], "malformed_frame")

    def test_direct_refuses_an_existing_owner_without_opening_bluetooth(self):
        self.socket.touch()
        result = self.call("--direct", "status")
        self.assertEqual(result.returncode, 1)
        self.assertEqual(json.loads(result.stderr)["code"], "service_unavailable")

    def test_direct_respects_native_owner_lock_even_before_owner_creates_socket(self):
        lock = self.socket.parent / "owner.lock"
        with lock.open("w") as owned:
            lock.chmod(0o600)
            fcntl.flock(owned, fcntl.LOCK_EX | fcntl.LOCK_NB)
            self.assertFalse(self.socket.exists())
            result = self.call("--direct", "status")
            self.assertEqual(result.returncode, 1)
            error = json.loads(result.stderr)
            self.assertEqual(error["code"], "service_unavailable")
            self.assertEqual(error["stage"], "owner lock")
            self.assertFalse(self.socket.exists())

    def test_explicit_address_wraps_native_request_without_retargeting_the_service(self):
        address = "00:11:22:33:44:55"
        for args, operation in [(["status"], {"type": "status"}),
            (["--conversation-awareness", "off"], {"type": "change", "value": {"ConversationalAwareness": False}})]:
            result, request = self.exchange(["--address", address, *args], self.ok)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(request["operation"], {"type": "for_device", "value": {
                "address": address, "operation": operation}})

    def test_scan_respects_owner_lock_or_rejects_absent_build_feature_before_hardware(self):
        lock = self.socket.parent / "owner.lock"
        with lock.open("w") as owned:
            lock.chmod(0o600)
            fcntl.flock(owned, fcntl.LOCK_EX | fcntl.LOCK_NB)
            result = self.call("--scan")
            self.assertEqual(result.returncode, 1)
            error = json.loads(result.stderr)
            self.assertEqual(error["code"], "service_unavailable" if BLE_BUILD else "unsupported")
            if BLE_BUILD:
                self.assertEqual(error["stage"], "owner lock")

    def test_real_serve_reports_private_bus_failure_and_stops_cleanly_on_sigint(self):
        server = subprocess.Popen([str(BINARY), "serve"], env=self.env,
                                  stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        try:
            deadline = time.monotonic() + 4
            while not self.socket.exists():
                self.assertIsNone(server.poll(), "service exited before creating native socket")
                self.assertLess(time.monotonic(), deadline, "service did not start")
                time.sleep(0.01)
            while True:
                result = self.call("status")
                self.assertEqual(result.returncode, 0, result.stderr)
                state = json.loads(result.stdout)
                if (state.get("last_error") or {}).get("code") == "adapter_unavailable":
                    break
                self.assertLess(time.monotonic(), deadline, "unavailable private bus was not diagnosed")
                time.sleep(0.01)
            self.assertFalse(state["connected"])
            self.assertFalse(state["ble_enabled"])
            self.assertEqual(state["ble"]["compiled"], BLE_BUILD)
            # No system/session bus or audio server points at the user's session.
            # This exercises production owners under a real unavailable-adapter failure.
            server.send_signal(signal.SIGINT)
            stdout, stderr = server.communicate(timeout=4)
            self.assertEqual(server.returncode, 0, stderr)
            self.assertEqual(stdout, "")
            self.assertFalse(self.socket.exists(), "owner socket survived clean process shutdown")
            state_file = Path(self.env["XDG_STATE_HOME"]) / "librepods-rs/status.json"
            self.assertFalse(state_file.exists(), "owner status survived clean process shutdown")
        finally:
            if server.poll() is None:
                server.kill()
                server.communicate(timeout=2)

    def test_human_output_and_no_media_override_keep_client_protocol_unchanged(self):
        result, request = self.exchange(["--no-media", "status"], self.ok, machine=False)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(json.loads(result.stdout), {"marker": "ok"})
        self.assertIn("\n", result.stdout.strip())
        self.assertEqual(request["operation"], {"type": "status"})
        failure = self.call("compat", "invalid-command", machine=False)
        self.assertEqual(failure.returncode, 1)
        self.assertIn("compat: unsupported legacy verb", failure.stderr)

    def test_direct_serve_is_rejected_before_starting_an_owner(self):
        result = self.call("--direct", "serve")
        self.assertEqual(result.returncode, 1)
        self.assertEqual(json.loads(result.stderr)["code"], "invalid_input")
        self.assertFalse(self.socket.exists())

    def test_invalid_compat_verb_is_rejected_before_connecting(self):
        result = self.call("compat", "adaptive:101")
        self.assertEqual(result.returncode, 1)
        self.assertEqual(json.loads(result.stderr)["code"], "invalid_input")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary", type=Path)
    parser.add_argument("--ble-build", action="store_true")
    args, remaining = parser.parse_known_args()
    BLE_BUILD = args.ble_build
    BINARY = args.binary.resolve(strict=True)
    unittest.main(argv=[__file__, *remaining])
