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

class ProductionCoverage(unittest.TestCase):
    def test_test_only_spans_do_not_inflate_production_counts(self):
        lcov = "SF:/repo/librepods-core/src/lib.rs\nDA:1,0\nDA:2,7\nDA:4,9\nDA:5,9\nend_of_record\n"
        result = coverage.production_measure(lcov, {"/repo/librepods-core/src/lib.rs": [[4, 5]]})
        self.assertEqual(result["core"], {"count": 2, "covered": 1, "test_lines_excluded": 2, "percent": 50.0})

    def test_feature_variants_merge_lines_and_missing_spans_fail_closed(self):
        name = "/repo/librepods-daemon/src/lib.rs"
        lcov = f"SF:{name}\nDA:1,0\nend_of_record\nSF:{name}\nDA:1,3\nend_of_record\n"
        self.assertEqual(coverage.production_measure(lcov, {name: []})["service"]["count"], 1)
        with self.assertRaises(ValueError):
            coverage.production_measure(lcov, {})


if __name__ == "__main__":
    unittest.main()
