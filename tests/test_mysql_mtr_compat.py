#!/usr/bin/env python3
import hashlib
import json
import os
import sys
import tempfile
import unittest
from argparse import Namespace
from pathlib import Path

from tools.mariadb_mtr_core import (
    Server,
    TestCase,
    mtr_case_timezone,
    mtr_command,
    parse_manifest,
    render_markdown,
    sql_statement_count,
    validate_cases,
    validate_distinct_servers,
    validate_mtr_runtime,
)
from tools.mariadb_mtr_discover_core import (
    discover_cases,
    rotating_selection,
    write_promotion_manifest,
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
            (root / "mysql-test" / "main").mkdir(parents=True)
            (root / "mysql-test" / "main" / "select_all.test").write_text("SELECT 1;\n")
            (root / "mysql-test" / "main" / "select_all.result").write_text("SELECT 1;\n1\n1\n")
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

    def test_sql_statement_count_ignores_directives_comments_and_quoted_semicolons(self):
        source = """
--disable_warnings
# comment;
CREATE TABLE t1 (value VARCHAR(20));
INSERT INTO t1 VALUES ('a;b'), ("c;d"), (`value`);
/* ignored; */ SELECT * FROM t1;
"""
        self.assertEqual(sql_statement_count(source), 3)

    def test_discovery_separates_static_candidates_from_harness_dependencies(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            test_dir = root / "mysql-test" / "main"
            result_dir = root / "mysql-test" / "main"
            test_dir.mkdir(parents=True)
            (test_dir / "plain.test").write_text("SELECT 1;\n")
            (result_dir / "plain.result").write_text("SELECT 1;\n1\n1\n")
            (test_dir / "sourced.test").write_text("-- source include/have_innodb.inc\nSELECT 1;\n")
            (result_dir / "sourced.result").write_text("SELECT 1;\n1\n1\n")
            cases = {case.name: case for case in discover_cases(root, "main", 200)}
            self.assertIsNone(cases["plain"].exclusion)
            self.assertEqual(cases["plain"].statements, 1)
            self.assertEqual(cases["sourced"].exclusion, "harness-dependency")

    def test_aggressive_discovery_follows_safe_sources_and_rejects_hidden_routines(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            test_dir = root / "mysql-test" / "main"
            result_dir = root / "mysql-test" / "main"
            include_dir = root / "mysql-test" / "include"
            test_dir.mkdir(parents=True)
            include_dir.mkdir(parents=True)
            (include_dir / "query.inc").write_text("SELECT 2;\n")
            (include_dir / "routine.inc").write_text(
                "CREATE PROCEDURE hidden_routine() SELECT 1;\n"
            )
            (test_dir / "sourced.test").write_text(
                "-- source include/query.inc\nSELECT 1;\n"
            )
            (result_dir / "sourced.result").write_text("SELECT 1;\n1\n1\n")
            (test_dir / "routine.test").write_text(
                "source 'include/routine.inc';\nSELECT 1;\n"
            )
            (result_dir / "routine.result").write_text("SELECT 1;\n1\n1\n")

            cases = {
                case.name: case
                for case in discover_cases(
                    root, "main", 200, include_safe_harness=True
                )
            }
            self.assertIsNone(cases["sourced"].exclusion)
            self.assertEqual(cases["sourced"].statements, 2)
            self.assertEqual(cases["routine"].exclusion, "outside-contract")

    def test_aggressive_discovery_rejects_harness_side_effects(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            test_dir = root / "mysql-test" / "main"
            result_dir = root / "mysql-test" / "main"
            test_dir.mkdir(parents=True)
            (test_dir / "writes_file.test").write_text(
                "-- write_file $MYSQL_TMP_DIR/data.txt\nvalue\nEOF\nSELECT 1;\n"
            )
            (result_dir / "writes_file.result").write_text("SELECT 1;\n1\n1\n")
            cases = {
                case.name: case
                for case in discover_cases(
                    root, "main", 200, include_safe_harness=True
                )
            }
            self.assertEqual(cases["writes_file"].exclusion, "harness-side-effect")

    def test_mariadb_layout_uses_main_for_top_level_cases(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            test_dir = root / "mysql-test" / "main"
            test_dir.mkdir(parents=True)
            (test_dir / "simple.test").write_text("SELECT 1;\n")
            (test_dir / "simple.result").write_text("SELECT 1;\n1\n")
            cases = discover_cases(root, "main", 200, layout="mariadb")
            self.assertEqual([case.name for case in cases], ["simple"])
            validate_cases(
                root,
                [
                    TestCase(
                        "simple",
                        "query",
                        hashlib.sha256((test_dir / "simple.test").read_bytes()).hexdigest(),
                        hashlib.sha256((test_dir / "simple.result").read_bytes()).hexdigest(),
                        "test",
                    )
                ],
                layout="mariadb",
            )

    def test_mtr_case_timezone_translates_posix_gmt_offset(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            test_dir = root / "mysql-test" / "main"
            test_dir.mkdir(parents=True)
            (test_dir / "timezone4.test").write_text("SELECT FROM_UNIXTIME(0);\n")
            (test_dir / "timezone4-master.opt").write_text("--timezone=GMT+10\n")
            case = TestCase("timezone4", "date-time", DIGEST_A, DIGEST_B, "test")
            self.assertEqual(mtr_case_timezone(root, case, "mariadb"), "-10:00")

    def test_mtr_case_timezone_defaults_to_utc(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            test_dir = root / "mysql-test" / "main"
            test_dir.mkdir(parents=True)
            (test_dir / "simple.test").write_text("SELECT 1;\n")
            case = TestCase("simple", "query", DIGEST_A, DIGEST_B, "test")
            self.assertEqual(mtr_case_timezone(root, case, "mariadb"), "+00:00")

    def test_mtr_case_timezone_rejects_nonportable_server_timezone(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            test_dir = root / "mysql-test" / "main"
            test_dir.mkdir(parents=True)
            (test_dir / "timezone.test").write_text("SELECT NOW();\n")
            (test_dir / "timezone-master.opt").write_text("--timezone=MET\n")
            case = TestCase("timezone", "date-time", DIGEST_A, DIGEST_B, "test")
            with self.assertRaisesRegex(RuntimeError, "fixed GMT offset"):
                mtr_case_timezone(root, case, "mariadb")

    def test_rotating_discovery_selection_wraps(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            test_dir = root / "mysql-test" / "main"
            result_dir = root / "mysql-test" / "main"
            test_dir.mkdir(parents=True)
            for name in ("alpha", "bravo", "charlie"):
                (test_dir / f"{name}.test").write_text("SELECT 1;\n")
                (result_dir / f"{name}.result").write_text("SELECT 1;\n1\n1\n")
            cases = discover_cases(root, "main", 200)
            selected = rotating_selection(cases, offset=2, limit=2)
            self.assertEqual([case.name for case in selected], ["charlie", "alpha"])

    def test_promotion_manifest_contains_only_dual_engine_passes(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            report = root / "report.json"
            manifest = root / "promoted.txt"
            report.write_text(
                json.dumps(
                    {
                        "source_revision": "abc123",
                        "results": [
                            {
                                "test": "passing",
                                "feature": "query",
                                "test_sha256": DIGEST_A,
                                "result_sha256": DIGEST_B,
                                "baseline": "pass",
                                "mysqweel": "pass",
                            },
                            {
                                "test": "failing",
                                "feature": "query",
                                "test_sha256": DIGEST_B,
                                "result_sha256": DIGEST_A,
                                "baseline": "pass",
                                "mysqweel": "fail",
                            },
                        ],
                    }
                )
            )
            write_promotion_manifest(
                Namespace(compat_report=report, promote_manifest=manifest)
            )
            promoted = manifest.read_text()
            self.assertIn("passing query", promoted)
            self.assertNotIn("failing query", promoted)


class ReportTests(unittest.TestCase):
    def test_report_contains_score_and_test_statuses(self):
        report = {
            "baseline_label": "MariaDB",
            "baseline_version": "10.11.7",
            "source_revision": "abc123",
            "status": "fail",
            "score_percent": 50.0,
            "counts": {"included": 2, "passed": 1, "failed": 1, "infrastructure": 0},
            "results": [
                {"test": "select_all", "feature": "select", "baseline": "pass", "mysqweel": "pass"},
                {"test": "join", "feature": "join", "baseline": "pass", "mysqweel": "fail"},
            ],
        }
        markdown = render_markdown(report)
        self.assertIn("Score: 50.0%", markdown)
        self.assertIn("`join` | `join` | 0 | pass | fail", markdown)

    def test_report_includes_threshold(self):
        report = {
            "baseline_label": "MariaDB",
            "baseline_version": "10.11.7",
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
            "baseline_label": "MariaDB",
            "baseline_version": "10.11.7",
            "source_revision": "abc123",
            "status": "invalid",
            "score_percent": 0.0,
            "counts": {"included": 1, "passed": 0, "failed": 1, "infrastructure": 0},
            "results": [
                {"test": "select_all", "feature": "select", "baseline": "fail", "mysqweel": "fail"}
            ],
            "invocations": [
                {
                    "test": "select_all",
                    "server": "mariadb",
                    "status": "fail",
                    "returncode": 1,
                    "stdout": "",
                    "stderr": "missing mariadb-import",
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
        self.assertIn("### mariadb: `select_all`", markdown)
        self.assertIn("missing mariadb-import", markdown)
        self.assertIn("### mysqweel: `select_all`", markdown)
        self.assertIn("result mismatch", markdown)

    def test_json_report_is_serializable(self):
        report = {"counts": {"included": 1}, "results": []}
        self.assertEqual(json.loads(json.dumps(report))["counts"]["included"], 1)


if __name__ == "__main__":
    unittest.main()
