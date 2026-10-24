import importlib.util
import os
import subprocess
import sys
from unittest import mock
from pathlib import Path
import unittest

ROOT = Path(__file__).resolve().parents[2]
spec = importlib.util.spec_from_file_location("soak", ROOT / "scripts/soak.py")
soak = importlib.util.module_from_spec(spec)
spec.loader.exec_module(soak)


class SoakEvidence(unittest.TestCase):
    def test_short_run_never_claims_24_hour_acceptance(self):
        samples = [{"cpu_ticks": 1, "rss_kib": 100, "status_ms": 1},
                   {"cpu_ticks": 1, "rss_kib": 100, "status_ms": 2}]
        result = soak.summarize(samples, 10, [])
        self.assertFalse(result["release_duration_met"])
        self.assertFalse(result["release_soak_passed"])

    def test_failures_and_memory_growth_remain_visible(self):
        samples = [{"cpu_ticks": 1, "rss_kib": 100, "status_ms": 1},
                   {"cpu_ticks": 1, "rss_kib": 101, "status_ms": 2}]
        result = soak.summarize(samples, soak.RELEASE_SECONDS, ["injected failure"])
        self.assertTrue(result["release_duration_met"])
        self.assertFalse(result["release_soak_passed"])
        self.assertTrue(result["memory_growth_review_required"])
        self.assertFalse(soak.summarize(samples, soak.RELEASE_SECONDS, [])["release_soak_passed"])

    def test_completed_release_gate_failure_is_a_nonzero_process_exit(self):
        self.assertEqual(soak.exit_code({"failures": [], "release_duration_met": True, "release_soak_passed": False}), 1)
        self.assertEqual(soak.exit_code({"failures": [], "release_duration_met": False, "release_soak_passed": False}), 0)
        self.assertEqual(soak.exit_code({"failures": [], "release_duration_met": True, "release_soak_passed": True}), 0)

    def test_percentile_uses_upper_tail_not_average(self):
        self.assertEqual(soak.p95(list(range(1, 101))), 95)


    def test_process_binary_mismatch_and_pid_reuse_are_rejected(self):
        with self.assertRaisesRegex(ValueError, "does not match"):
            soak.process_identity(os.getpid(), Path("/usr/bin/true"))
        baseline = soak.process_identity(os.getpid(), Path(sys.executable))
        with mock.patch.object(soak, "process_identity", return_value={**baseline, "start_ticks": baseline["start_ticks"] + 1}):
            with self.assertRaisesRegex(ValueError, "changed"):
                soak.verify_identity(os.getpid(), Path(sys.executable), baseline)

    def test_source_and_integrity_gaps_cannot_claim_release(self):
        self.assertFalse(soak.source_evidence_valid(None, None, "unknown"))
        self.assertFalse(soak.source_evidence_valid("a" * 40, "b" * 40, "dirty"))
        self.assertTrue(soak.source_evidence_valid("a" * 40, "b" * 40, "clean"))
        samples = [{"cpu_ticks": 1, "rss_kib": 100, "status_ms": 1},
                   {"cpu_ticks": 1, "rss_kib": 100, "status_ms": 2}]
        self.assertFalse(soak.summarize(samples, soak.RELEASE_SECONDS, [])["release_soak_passed"])
        self.assertTrue(soak.summarize(samples, soak.RELEASE_SECONDS, [],
            {"release_evidence_complete": True})["release_soak_passed"])
        self.assertFalse(soak.summarize(samples, 10, [],
            {"release_evidence_complete": True})["release_soak_passed"])

    def test_journal_capture_limits_bytes_time_and_never_returns_content(self):
        actual_popen = subprocess.Popen
        def flood(_command, **kwargs):
            return actual_popen([sys.executable, "-c", "import os; os.write(1, b'private log content' * 1000)"], **kwargs)
        with mock.patch.object(soak.subprocess, "Popen", side_effect=flood):
            result = soak.journal_volume("example.service", 0, 1, byte_limit=64)
        self.assertTrue(result["truncated"])
        self.assertLessEqual(result["bytes_observed"], 65)
        self.assertNotIn("private log content", str(result))
        def stall(_command, **kwargs):
            return actual_popen([sys.executable, "-c", "import time; time.sleep(10)"], **kwargs)
        with mock.patch.object(soak.subprocess, "Popen", side_effect=stall):
            result = soak.journal_volume("example.service", 0, 1, timeout=0.02)
        self.assertTrue(result["timed_out"])
        self.assertFalse(result["complete"])
        with self.assertRaises(ValueError):
            soak.journal_volume("unsafe\nunit.service", 0, 1)


if __name__ == "__main__":
    unittest.main()
