# SPDX-License-Identifier: AGPL-3.0-only
import importlib.util
from pathlib import Path
import unittest

ROOT = Path(__file__).resolve().parents[2]
spec = importlib.util.spec_from_file_location("audit", ROOT / "scripts/coverage-audit.py")
audit = importlib.util.module_from_spec(spec)
spec.loader.exec_module(audit)


class ChangedCoverage(unittest.TestCase):
    def test_diff_tracks_only_new_lines_and_handles_zero_length_deletions(self):
        diff = "+++ b/librepods-daemon/src/runtime.rs\n@@ -1,2 +1,3 @@\n@@ -10,1 +11,0 @@\n"
        self.assertEqual(audit.changed_lines(diff), {"librepods-daemon/src/runtime.rs": {1, 2, 3}})

    def test_uncovered_changed_production_remains_in_critical_denominator(self):
        path = "/repo/librepods-daemon/src/runtime.rs"
        result = audit.audit(f"SF:{path}\nDA:1,0\nDA:2,1\nDA:3,9\n", {path: [[3, 3]]},
                             {"librepods-daemon/src/runtime.rs": {1, 3}}, root=Path("/repo"))
        self.assertEqual(result["changed_production"], {"covered": 0, "count": 1, "percent": 0.0})
        self.assertEqual(result["critical_changed_production"]["percent"], 0.0)
        self.assertEqual(result["files"]["librepods-daemon/src/runtime.rs"]["uncovered_changed_lines"], [1])
