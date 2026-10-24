#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-only
"""Run the actual CLI against isolated native IPC peers; never access Bluetooth."""
import argparse
import json
import os
from pathlib import Path
import socket
import subprocess
import tempfile
import threading
import unittest

BINARY = None
FIXTURES = Path(__file__).resolve().parents[2] / "target/native-fixtures"


class CliContract(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="pods-cli-")
        self.addCleanup(self.temp.cleanup)
        root = Path(self.temp.name)
        self.env = {**os.environ, "HOME": str(root), "XDG_RUNTIME_DIR": str(root),
                    "XDG_CONFIG_HOME": str(root / "config"),
                    "XDG_STATE_HOME": str(root / "state"), "RUST_LOG": "off"}
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
            (["audio", "input", "example-source"], {"type": "set_input", "value": "example-source"}),
            (["audio", "profile", "a2dp-sink"], {"type": "set_profile", "value": "a2dp-sink"}),
            (["audio", "move-stream", "17", "example-sink"], {"type": "move_stream", "value": {"stream": "17", "output": "example-sink"}}),
        ]
        for args, expected in cases:
            with self.subTest(args=args):
                result, request = self.exchange(args, self.ok)
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(request["operation"], expected)
                self.socket.unlink()

    def test_watch_consumes_actual_serializer_snapshots(self):
        snapshots = [json.loads((FIXTURES / name).read_text())
                     for name in ["pro2.json", "disconnected.json"]]
        def frames(request):
            return b"".join(json.dumps({"version": 1, "id": request["id"],
                "result": snapshot, "error": None}).encode() + b"\n"
                for snapshot in snapshots)
        result, request = self.exchange(["--status"], frames)
        self.assertEqual(request["operation"], {"type": "watch"})
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual([json.loads(line) for line in result.stdout.splitlines()], snapshots)
        self.assertTrue(snapshots[0]["connected"])
        self.assertFalse(snapshots[1]["connected"])
        self.assertTrue(snapshots[1]["device"]["observations"]["battery.left"]["stale"])

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

    def test_foreign_response_identifier_and_version_are_rejected(self):
        for version, identifier in [(1, 99), (99, 1)]:
            with self.subTest(version=version, identifier=identifier):
                result, _ = self.exchange(["status"], lambda _: {
                    "version": version, "id": identifier, "result": {}, "error": None})
                self.assertEqual(result.returncode, 1)
                self.assertEqual(json.loads(result.stderr)["code"], "version_mismatch")
                self.socket.unlink()

    def test_bad_or_missing_result_does_not_print_success(self):
        for response in [b"not JSON\n", b"{\"version\":1,\"id\":1,\"result\":null,\"error\":null}\n"]:
            with self.subTest(response=response):
                result, _ = self.exchange(["status"], lambda _: response)
                self.assertEqual(result.returncode, 1)
                self.assertEqual(result.stdout, "")
                self.assertEqual(json.loads(result.stderr)["code"], "malformed_frame")
                self.socket.unlink()

    def test_direct_refuses_an_existing_owner_without_opening_bluetooth(self):
        self.socket.touch()
        result = self.call("--direct", "status")
        self.assertEqual(result.returncode, 1)
        self.assertEqual(json.loads(result.stderr)["code"], "service_unavailable")

    def test_invalid_compat_verb_is_rejected_before_connecting(self):
        result = self.call("compat", "adaptive:101")
        self.assertEqual(result.returncode, 1)
        self.assertEqual(json.loads(result.stderr)["code"], "invalid_input")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary", type=Path)
    args, remaining = parser.parse_known_args()
    BINARY = args.binary.resolve(strict=True)
    unittest.main(argv=[__file__, *remaining])
