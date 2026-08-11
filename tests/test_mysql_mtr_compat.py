#!/usr/bin/env python3
import json
import tempfile
import unittest
from pathlib import Path

from tools.mysql_mtr_compat import (
    parse_manifest,
    render_markdown,
    validate_distinct_servers,
)


class ManifestTests(unittest.TestCase):
    def test_same_host_and_port_for_both_servers_is_rejected(self):
        with self.assertRaisesRegex(ValueError, "same host and port"):
            validate_distinct_servers(
                "mysql://root:baseline@127.0.0.1:3306/test",
                "mysql://root@127.0.0.1:3306/test",
            )

    def test_separate_mysqweel_server_is_accepted(self):
        validate_distinct_servers(
            "mysql://root:baseline@127.0.0.1:3306/test",
            "mysql://root@127.0.0.1:3307/test",
        )

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

    def test_report_includes_one_representative_failure_per_server(self):
        report = {
            "mysql_version": "8.0.43",
            "source_revision": "abc123",
            "status": "invalid",
            "score_percent": 0.0,
            "counts": {"included": 1, "passed": 0, "failed": 1, "infrastructure": 0},
            "results": [{"test": "select_all", "mysql": "fail", "mysqweel": "fail"}],
            "invocations": [
                {
                    "test": "select_all",
                    "server": "mysql",
                    "status": "fail",
                    "returncode": 1,
                    "stdout": "",
                    "stderr": "missing mysqlimport",
                },
                {
                    "test": "select_all",
                    "server": "mysqweel",
                    "status": "fail",
                    "returncode": 1,
                    "stdout": "result mismatch",
                    "stderr": "",
                },
            ],
        }
        markdown = render_markdown(report)
        self.assertIn("Representative failure diagnostics", markdown)
        self.assertIn("### mysql: `select_all`", markdown)
        self.assertIn("missing mysqlimport", markdown)
        self.assertIn("### mysqweel: `select_all`", markdown)
        self.assertIn("result mismatch", markdown)

    def test_json_report_is_serializable(self):
        report = {"counts": {"included": 1}, "results": []}
        self.assertEqual(json.loads(json.dumps(report))["counts"]["included"], 1)


if __name__ == "__main__":
    unittest.main()
