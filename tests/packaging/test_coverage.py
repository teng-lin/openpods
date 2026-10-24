import importlib.util
from pathlib import Path
import unittest

ROOT = Path(__file__).resolve().parents[2]
spec = importlib.util.spec_from_file_location("coverage_check", ROOT / "scripts/check-coverage.py")
coverage = importlib.util.module_from_spec(spec)
spec.loader.exec_module(coverage)


class Coverage(unittest.TestCase):
    def test_counts_both_covered_and_uncovered_source_lines(self):
        report = {"data": [{"files": [
            {"filename": "/repo/librepods-core/src/parser.rs", "summary": {"lines": {"count": 100, "covered": 90}}},
            {"filename": "/repo/librepods-daemon/src/server.rs", "summary": {"lines": {"count": 50, "covered": 40}}},
            {"filename": "/repo/librepods-linux/src/socket.rs", "summary": {"lines": {"count": 50, "covered": 0}}},
            {"filename": "/repo/target/generated.rs", "summary": {"lines": {"count": 1000, "covered": 1000}}},
        ]}]}
        measured = coverage.measure(report)
        self.assertEqual(measured["core"]["percent"], 90)
        self.assertEqual(measured["service"]["percent"], 40)


if __name__ == "__main__":
    unittest.main()
