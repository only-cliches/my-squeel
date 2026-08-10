#!/usr/bin/env python3
import json
import tempfile
import unittest
from pathlib import Path

from tools.mysql_mtr_compat import parse_manifest, render_markdown


class ManifestTests(unittest.TestCase):
    def test_comments_and_duplicates(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "allowlist.txt"
            path.write_text("# comment\nselect_all # supported\nfunc_math\n")
            self.assertEqual([case.name for case in parse_manifest(path)], ["select_all", "func_math"])

    def test_empty_manifest_is_rejected(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "allowlist.txt"
            path.write_text("# no tests\n")
            with self.assertRaises(ValueError):
                parse_manifest(path)

    def test_duplicate_manifest_is_rejected(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "allowlist.txt"
            path.write_text("select_all\nselect_all\n")
            with self.assertRaises(ValueError):
                parse_manifest(path)


class ReportTests(unittest.TestCase):
    def test_report_contains_score_and_test_statuses(self):
        report = {
            "mysql_version": "8.0.43",
            "source_revision": "abc123",
            "status": "fail",
            "score_percent": 50.0,
            "counts": {"included": 2, "passed": 1, "failed": 1, "infrastructure": 0},
            "results": [
                {"test": "select_all", "mysql": "pass", "mysqweel": "pass"},
                {"test": "join", "mysql": "pass", "mysqweel": "fail"},
            ],
        }
        markdown = render_markdown(report)
        self.assertIn("Score: 50.0%", markdown)
        self.assertIn("`join` | pass | fail", markdown)

    def test_report_includes_threshold(self):
        report = {
            "mysql_version": "8.0.43",
            "source_revision": "abc123",
            "status": "pass",
            "score_percent": 90.0,
            "minimum_percent": 90.0,
            "counts": {"included": 1, "passed": 1, "failed": 0, "infrastructure": 0},
            "results": [],
        }
        self.assertIn("Status: **pass**", render_markdown(report))

    def test_json_report_is_serializable(self):
        report = {"counts": {"included": 1}, "results": []}
        self.assertEqual(json.loads(json.dumps(report))["counts"]["included"], 1)


if __name__ == "__main__":
    unittest.main()
