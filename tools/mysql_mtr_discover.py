#!/usr/bin/env python3
"""Inventory pinned MariaDB MTR tests and build rotating audit manifests.

Discovery is intentionally non-gating. It identifies complete upstream files
that are plausible external-server compatibility candidates, while the strict
manifest remains limited to cases proven to pass on both MariaDB and MySqweel.
"""

from __future__ import annotations

import argparse
import json
import re
from collections import Counter
from dataclasses import asdict, dataclass
from pathlib import Path

try:
    from tools.mysql_mtr_compat import (
        TEST_NAME,
        sha256_file,
        sql_statement_count,
    )
except ModuleNotFoundError:  # Direct execution adds tools/, not the repository root, to sys.path.
    from mysql_mtr_compat import TEST_NAME, sha256_file, sql_statement_count


DEPENDENT_DIRECTIVE = re.compile(
    r"(?im)^\s*(?:--\s*)?(?:source|include|let|eval|exec|system|perl|connect|connection|"
    r"send|reap|sleep|real_sleep|shutdown|restart|write_file|append_file|remove_file|"
    r"copy_file|move_file|chmod|mkdir|rmdir|cat_file)\b"
)
DELIMITER_DIRECTIVE = re.compile(r"(?im)^\s*(?:--\s*)?delimiter\b")
UNSUPPORTED_SQL = re.compile(
    r"(?is)\b(?:START\s+TRANSACTION|BEGIN\s+WORK|COMMIT|ROLLBACK|SAVEPOINT|"
    r"LOCK\s+TABLES?|UNLOCK\s+TABLES?|XA\s|GRANT\s|REVOKE\s|"
    r"CREATE\s+USER|ALTER\s+USER|DROP\s+USER|CREATE\s+(?:DEFINER\s*=\s*\S+\s+)?"
    r"(?:PROCEDURE|FUNCTION|TRIGGER|EVENT)|DROP\s+(?:PROCEDURE|FUNCTION|TRIGGER|EVENT)|"
    r"CHANGE\s+(?:MASTER|REPLICATION\s+SOURCE)|START\s+(?:SLAVE|REPLICA)|"
    r"STOP\s+(?:SLAVE|REPLICA)|RESET\s+(?:MASTER|REPLICA|SLAVE)|"
    r"INSTALL\s+(?:PLUGIN|COMPONENT)|UNINSTALL\s+(?:PLUGIN|COMPONENT)|"
    r"CREATE\s+RESOURCE\s+GROUP|ALTER\s+RESOURCE\s+GROUP|CLONE\s+INSTANCE)\b"
)
SPECIALIZED_SQL = re.compile(
    r"(?is)\b(?:PARTITION(?:ING)?|FULLTEXT|SPATIAL)\b|"
    r"\bENGINE\s*=\s*(?:ARCHIVE|CSV|MEMORY|HEAP|MYISAM|FEDERATED|NDB)\b"
)
SERVER_CONFIGURATION_SQL = re.compile(
    r"(?is)\b(?:SET\s+(?:@@)?GLOBAL|FLUSH\s|SHUTDOWN\b|RESTART\b|"
    r"SET\s+PERSIST(?:_ONLY)?\b)"
)
TOPOLOGY_NAME = re.compile(
    r"(?i)(?:^|_)(?:binlog|replication|replica|slave|master|group_replication|ndb)(?:_|$)"
)
COMPATIBILITY_SUITES = {
    "collations",
    "funcs_1",
    "funcs_2",
    "gcol",
    "information_schema",
    "innodb",
    "jp",
    "json",
}


@dataclass(frozen=True)
class DiscoveryCase:
    name: str
    feature: str
    statements: int
    test_sha256: str
    result_sha256: str
    test_file: str
    result_file: str
    exclusion: str | None


def without_mtr_comments(text: str) -> str:
    return "\n".join(
        line
        for line in text.splitlines()
        if not line.lstrip().startswith(("#", "--"))
    )


def classify_feature(sql: str) -> str:
    categories: set[str] = set()
    keyword_categories = (
        (r"\b(?:CREATE|ALTER|DROP|TRUNCATE|RENAME)\s", "ddl"),
        (r"\b(?:INSERT|REPLACE)\s", "insert"),
        (r"\bUPDATE\s", "update"),
        (r"\bDELETE\s", "delete"),
        (r"\b(?:SELECT|WITH)\s", "query"),
        (r"\b(?:SHOW|DESCRIBE|DESC|EXPLAIN)\s", "metadata"),
        (r"\bSET\s", "session"),
    )
    for pattern, category in keyword_categories:
        if re.search(pattern, sql, re.IGNORECASE):
            categories.add(category)
    if not categories:
        return "other"
    return "-".join(sorted(categories))


def test_name(mysql_test_root: Path, test_file: Path, layout: str = "mysql") -> str | None:
    relative = test_file.relative_to(mysql_test_root)
    parts = relative.parts
    if layout == "mariadb" and len(parts) == 2 and parts[0] == "main":
        return test_file.stem
    if len(parts) == 2 and parts[0] == "t":
        return test_file.stem
    if parts and parts[0] == "suite" and "t" in parts[1:-1]:
        test_directory = parts.index("t", 1)
        suite = "/".join(parts[1:test_directory])
        return f"{suite}/{test_file.stem}"
    return None


def result_file_for_test(mysql_test_root: Path, test_file: Path, layout: str = "mysql") -> Path:
    relative = test_file.relative_to(mysql_test_root)
    parts = list(relative.parts)
    if layout == "mariadb" and parts[0] == "main":
        return test_file.with_suffix(".result")
    test_directory = parts.index("t")
    parts[test_directory] = "r"
    return mysql_test_root.joinpath(*parts).with_suffix(".result")


def companion_file_exists(test_file: Path) -> bool:
    stem = test_file.with_suffix("")
    companions = (
        stem.with_suffix(".opt"),
        stem.with_suffix(".cnf"),
        test_file.with_name(f"{test_file.stem}-master.opt"),
        test_file.with_name(f"{test_file.stem}-slave.opt"),
    )
    return any(path.exists() for path in companions)


def exclusion_reason(
    name: str,
    text: str,
    sql: str,
    statements: int,
    result_file: Path,
    test_file: Path,
    max_statements: int,
) -> str | None:
    if name.count("/") > 1:
        return "nested-suite-layout"
    if not TEST_NAME.fullmatch(name):
        return "invalid-manifest-name"
    if "/" in name and name.split("/", 1)[0] not in COMPATIBILITY_SUITES:
        return "outside-contract-suite"
    if not result_file.is_file():
        return "missing-result"
    if statements == 0:
        return "no-direct-sql"
    if statements > max_statements:
        return "over-statement-limit"
    if DELIMITER_DIRECTIVE.search(text):
        return "custom-delimiter"
    if DEPENDENT_DIRECTIVE.search(text):
        return "harness-dependency"
    if companion_file_exists(test_file):
        return "server-options"
    if TOPOLOGY_NAME.search(name):
        return "topology-suite"
    if UNSUPPORTED_SQL.search(sql) or SPECIALIZED_SQL.search(sql):
        return "outside-contract"
    if SERVER_CONFIGURATION_SQL.search(sql):
        return "server-configuration"
    return None


def discover_cases(
    suite_root: Path, scope: str, max_statements: int, layout: str = "mysql"
) -> list[DiscoveryCase]:
    mysql_test_root = suite_root / "mysql-test"
    patterns = [mysql_test_root / ("main" if layout == "mariadb" else "t")]
    if scope == "all":
        patterns.extend((mysql_test_root / "suite").glob("**/t"))
    cases: list[DiscoveryCase] = []
    for test_dir in patterns:
        if not test_dir.is_dir():
            continue
        for test_file in sorted(test_dir.glob("*.test")):
            name = test_name(mysql_test_root, test_file, layout)
            if not name:
                continue
            result_file = result_file_for_test(mysql_test_root, test_file, layout)
            text = test_file.read_text(encoding="utf-8", errors="replace")
            sql = without_mtr_comments(text)
            statements = sql_statement_count(text)
            reason = exclusion_reason(
                name,
                text,
                sql,
                statements,
                result_file,
                test_file,
                max_statements,
            )
            cases.append(
                DiscoveryCase(
                    name=name,
                    feature=classify_feature(sql),
                    statements=statements,
                    test_sha256=sha256_file(test_file),
                    result_sha256=sha256_file(result_file) if result_file.is_file() else "",
                    test_file=str(test_file),
                    result_file=str(result_file),
                    exclusion=reason,
                )
            )
    return cases


def rotating_selection(cases: list[DiscoveryCase], offset: int, limit: int) -> list[DiscoveryCase]:
    if not cases or limit <= 0:
        return []
    ordered = sorted(cases, key=lambda case: case.name)
    start = offset % len(ordered)
    count = min(limit, len(ordered))
    return [ordered[(start + index) % len(ordered)] for index in range(count)]


def manifest_text(cases: list[DiscoveryCase], revision: str) -> str:
    lines = [
        "# Generated MTR discovery batch; not the strict compatibility manifest.",
        f"# Pinned source revision: {revision}",
        "# Columns: test feature test_sha256 result_sha256",
    ]
    lines.extend(
        f"{case.name} {case.feature} {case.test_sha256} {case.result_sha256}"
        for case in cases
    )
    return "\n".join(lines) + "\n"


def render_discovery_markdown(report: dict) -> str:
    counts = report["counts"]
    lines = [
        f"# {report['baseline_label']} MTR discovery inventory",
        "",
        f"- Source revision: `{report['source_revision']}`",
        f"- Scope: `{report['scope']}`",
        f"- Test files inspected: {counts['inspected']}",
        f"- Static audit candidates: {counts['candidates']}",
        f"- Candidate SQL statements: {counts['candidate_statements']}",
        f"- Tests selected in this batch: {counts['selected']}",
        f"- SQL statements selected in this batch: {counts['selected_statements']}",
        "",
        "Static candidacy only means a complete file appears viable in external-server mode. "
        "A case is promotable only after the generated batch passes against both the baseline and MySqweel.",
        "",
        "## Candidate test-shape coverage",
        "",
        "| Feature | Tests | SQL statements |",
        "| --- | ---: | ---: |",
    ]
    for feature, coverage in sorted(report["feature_coverage"].items()):
        lines.append(f"| `{feature}` | {coverage['tests']} | {coverage['statements']} |")
    lines.extend(
        [
            "",
            "## Static exclusions",
            "",
            "| Reason | Tests |",
            "| --- | ---: |",
        ]
    )
    for reason, count in sorted(report["exclusions"].items(), key=lambda item: (-item[1], item[0])):
        lines.append(f"| `{reason}` | {count} |")
    lines.extend(
        [
            "",
            "## Selected audit batch",
            "",
            "| Test | Feature | SQL statements |",
            "| --- | --- | ---: |",
        ]
    )
    for case in report["selected"]:
        lines.append(f"| `{case['name']}` | `{case['feature']}` | {case['statements']} |")
    return "\n".join(lines) + "\n"


def write_inventory(args: argparse.Namespace) -> int:
    suite_root = args.suite_root.resolve()
    output_dir = args.output_dir.resolve()
    output_dir.mkdir(parents=True, exist_ok=True)
    cases = discover_cases(suite_root, args.scope, args.max_statements, args.mtr_layout)
    candidates = [case for case in cases if case.exclusion is None]
    selected = rotating_selection(candidates, args.offset, args.limit)
    exclusions = Counter(case.exclusion for case in cases if case.exclusion)
    feature_coverage: dict[str, dict[str, int]] = {}
    for case in candidates:
        coverage = feature_coverage.setdefault(case.feature, {"tests": 0, "statements": 0})
        coverage["tests"] += 1
        coverage["statements"] += case.statements
    report = {
        "schema": "my-sqweel.mtr-discovery.v2",
        "baseline_label": args.baseline_label,
        "source_revision": args.source_revision,
        "scope": args.scope,
        "offset": args.offset,
        "limit": args.limit,
        "max_statements": args.max_statements,
        "counts": {
            "inspected": len(cases),
            "candidates": len(candidates),
            "candidate_statements": sum(case.statements for case in candidates),
            "selected": len(selected),
            "selected_statements": sum(case.statements for case in selected),
        },
        "exclusions": dict(exclusions),
        "feature_coverage": feature_coverage,
        "inventory": [asdict(case) for case in cases],
        "candidates": [asdict(case) for case in candidates],
        "selected": [asdict(case) for case in selected],
    }
    manifest = output_dir / "mariadb-mtr-discovery-manifest.txt"
    manifest.write_text(manifest_text(selected, args.source_revision))
    (output_dir / "mariadb-mtr-discovery.json").write_text(json.dumps(report, indent=2) + "\n")
    markdown = render_discovery_markdown(report)
    (output_dir / "mariadb-mtr-discovery.md").write_text(markdown)
    print(markdown, end="")
    return 0


def write_promotion_manifest(args: argparse.Namespace) -> int:
    report = json.loads(args.compat_report.read_text())
    promoted = [
        result
        for result in report.get("results", [])
        if result.get("baseline", result.get("mysql")) == "pass"
        and result.get("mysqweel") == "pass"
    ]
    lines = [
        "# Generated candidates that passed the audited MariaDB and MySqweel targets.",
        f"# Pinned source revision: {report.get('source_revision', 'unknown')}",
        "# Review compatibility-boundary fit before merging into the strict manifest.",
        "# Columns: test feature test_sha256 result_sha256",
    ]
    lines.extend(
        f"{result['test']} {result['feature']} {result['test_sha256']} "
        f"{result['result_sha256']}"
        for result in promoted
    )
    args.promote_manifest.parent.mkdir(parents=True, exist_ok=True)
    args.promote_manifest.write_text("\n".join(lines) + "\n")
    print(f"Promotable complete upstream files: {len(promoted)}")
    return 0


def parser() -> argparse.ArgumentParser:
    result = argparse.ArgumentParser(description=__doc__)
    result.add_argument("--suite-root", type=Path)
    result.add_argument("--output-dir", type=Path, default=Path("artifacts/mariadb-mtr-discovery"))
    result.add_argument("--scope", choices=("main", "all"), default="main")
    result.add_argument("--offset", type=int, default=0)
    result.add_argument("--limit", type=int, default=100)
    result.add_argument("--max-statements", type=int, default=200)
    result.add_argument("--source-revision", default="mariadb-10.11.7-2ubuntu2")
    result.add_argument("--mtr-layout", choices=("mysql", "mariadb"), default="mysql")
    result.add_argument("--baseline-label", default="MariaDB")
    result.add_argument("--compat-report", type=Path)
    result.add_argument("--promote-manifest", type=Path)
    return result


def main(args: argparse.Namespace) -> int:
    if args.compat_report or args.promote_manifest:
        if not args.compat_report or not args.promote_manifest:
            raise ValueError("--compat-report and --promote-manifest must be used together")
        return write_promotion_manifest(args)
    if not args.suite_root:
        raise ValueError("--suite-root is required for discovery")
    return write_inventory(args)


if __name__ == "__main__":
    try:
        raise SystemExit(main(parser().parse_args()))
    except (FileNotFoundError, ValueError) as error:
        print(f"MTR discovery: {error}", file=__import__("sys").stderr)
        raise SystemExit(2)
