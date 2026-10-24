# SPDX-License-Identifier: AGPL-3.0-only
"""Acceptance-runner tests use a fake service and never access real hardware."""
import importlib.util
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location("hardware_modes", Path(__file__).with_name("hardware_modes.py"))
modes = importlib.util.module_from_spec(spec)
spec.loader.exec_module(modes)


class AcceptanceCleanup(unittest.TestCase):
    def exercise(self, fail_at=None):
        current = "anc"
        count = 0
        audio = {"profile": "a2dp", "default_input": "external", "default_output": "airpods"}
        def state(_binary):
            return {"device": {"listening_mode": next(key for key,value in modes.MODES.items() if value == current), "model_number": "A3048"}}
        def request(_binary, *arguments):
            nonlocal current, count
            if arguments[:2] == ("audio", "status"):
                return audio.copy(), 1
            count += 1
            if count == fail_at:
                raise RuntimeError("injected exchange failure")
            current = arguments[1]
            return "Confirmed", 10
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            binary = root / "binary"
            binary.write_bytes(b"fixture")
            with patch.object(modes,"ready",side_effect=state), patch.object(modes,"request",side_effect=request), patch.object(modes.time,"sleep"):
                result = modes.run(binary,root / "evidence",4,["off","anc","transparency"],True)
            self.assertEqual(current,"anc")
            self.assertEqual(result["restoration"],"verified")
            return result

    def test_success_counts_real_confirmed_transitions_and_restores_original(self):
        result = self.exercise()
        self.assertEqual(result["successful_transitions"],4)
        self.assertEqual(result["failures"],[])
        self.assertTrue(all(item["from"] != item["to"] for item in result["transitions"]))

    def test_mid_run_failure_still_restores_and_cannot_claim_full_cycle_count(self):
        result = self.exercise(fail_at=2)
        self.assertEqual(result["successful_transitions"],1)
        self.assertEqual(result["failures"],["injected exchange failure"])


if __name__ == "__main__":
    unittest.main()
