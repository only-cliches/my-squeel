#!/usr/bin/env python3
import json
import os
import sys
import tempfile
import unittest
from pathlib import Path

from tools.mysql_mtr_compat import (
    Server,
    TestCase,
    mtr_command,
    parse_manifest,
    render_markdown,
    validate_cases,
    validate_distinct_servers,
    validate_mtr_runtime,
)

DIGEST_A = "a" * 64
DIGEST_B = "b" * 64


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
            path.write_text(
                f"# comment\nselect_all query {DIGEST_A} {DIGEST_B} # supported\n"
                f"func_math scalar {DIGEST_B} {DIGEST_A}\n"
            )
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
            path.write_text(
                f"select_all query {DIGEST_A} {DIGEST_B}\n"
                f"select_all query {DIGEST_A} {DIGEST_B}\n"
            )
            with self.assertRaises(ValueError):
                parse_manifest(path)

    def test_upstream_hash_mismatch_is_rejected(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "mysql-test" / "t").mkdir(parents=True)
            (root / "mysql-test" / "r").mkdir(parents=True)
            (root / "mysql-test" / "t" / "select_all.test").write_text("SELECT 1;\n")
            (root / "mysql-test" / "r" / "select_all.result").write_text("SELECT 1;\n1\n1\n")
            manifest = root / "allowlist.txt"
            manifest.write_text(f"select_all query {DIGEST_A} {DIGEST_B}\n")
            with self.assertRaisesRegex(ValueError, "test hash mismatch"):
                validate_cases(root, parse_manifest(manifest))

    def test_noop_safe_process_is_rejected(self):
        with tempfile.TemporaryDirectory() as directory:
            bindir = Path(directory)
            safe_process = bindir / "mysqltest_safe_process"
            safe_process.write_text("#!/bin/sh\nexit 0\n")
            os.chmod(safe_process, 0o755)
            with self.assertRaisesRegex(RuntimeError, "did not execute the canary child process"):
                validate_mtr_runtime(bindir, Path(sys.executable))

    def test_suite_test_uses_its_suite_and_basename(self):
        case = TestCase("json/functions", "json", DIGEST_A, DIGEST_B, "manifest")
        command = mtr_command(
            Path("/mysql"),
            Path("/mysql/mysql-test/mysql-test-run.pl"),
            Path("/mysql/bin"),
            Server("mysql", "mysql://root@127.0.0.1:3306/test"),
            case,
            Path("/tmp/vardir"),
        )
        self.assertIn("--suite=json", command)
        self.assertEqual(command[-1], "functions")


class ReportTests(unittest.TestCase):
    def test_report_contains_score_and_test_statuses(self):
        report = {
            "mysql_version": "8.0.43",
            "source_revision": "abc123",
            "status": "fail",
            "score_percent": 50.0,
            "counts": {"included": 2, "passed": 1, "failed": 1, "infrastructure": 0},
            "results": [
                {"test": "select_all", "feature": "select", "mysql": "pass", "mysqweel": "pass"},
                {"test": "join", "feature": "join", "mysql": "pass", "mysqweel": "fail"},
            ],
        }
        markdown = render_markdown(report)
        self.assertIn("Score: 50.0%", markdown)
        self.assertIn("`join` | `join` | pass | fail", markdown)

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
            "results": [
                {"test": "select_all", "feature": "select", "mysql": "fail", "mysqweel": "fail"}
            ],
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
