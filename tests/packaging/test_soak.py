import importlib.util
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

    def test_percentile_uses_upper_tail_not_average(self):
        self.assertEqual(soak.p95(list(range(1, 101))), 95)


if __name__ == "__main__":
    unittest.main()
