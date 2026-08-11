#!/usr/bin/env python3
"""Run a pinned, allowlisted MySQL MTR surface against two servers.

The MySQL source tree and its mysqltest binary are intentionally supplied by
the caller.  They are not vendored in this repository because the upstream
test sources are GPL-licensed.  The runner uses MTR's --extern mode, so the
same upstream test and expected result are executed against MySQL and
MySqweel independently.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import re
import shutil
import socket
import subprocess
import sys
import tempfile
import time
from dataclasses import asdict, dataclass
from pathlib import Path
from urllib.parse import unquote, urlsplit


DEFAULT_ALLOWLIST = Path("tests/mysql-mtr-allowlist.txt")
TEST_NAME = re.compile(r"^[A-Za-z0-9_.-]+(?:/[A-Za-z0-9_.-]+)?$")


@dataclass(frozen=True)
class Server:
    name: str
    url: str


@dataclass(frozen=True)
class TestCase:
    name: str
    feature: str
    test_sha256: str
    result_sha256: str
    source: str


@dataclass
class Invocation:
    test: str
    server: str
    status: str
    returncode: int | None
    command: list[str]
    stdout: str
    stderr: str
    artifact_dir: str


def parse_manifest(path: Path) -> list[TestCase]:
    cases: list[TestCase] = []
    seen: set[str] = set()
    for line_number, raw_line in enumerate(
        path.read_text(encoding="utf-8", errors="replace").splitlines(), 1
    ):
        line = raw_line.split("#", 1)[0].strip()
        if not line:
            continue
        fields = line.split()
        if len(fields) != 4:
            raise ValueError(
                f"{path}:{line_number}: expected test, feature, test SHA-256, "
                "and result SHA-256"
            )
        name = fields[0]
        if not TEST_NAME.fullmatch(name):
            raise ValueError(f"{path}:{line_number}: invalid MTR test name {name!r}")
        if name in seen:
            raise ValueError(f"{path}:{line_number}: duplicate MTR test {name!r}")
        feature, test_sha256, result_sha256 = fields[1:]
        if not TEST_NAME.fullmatch(feature):
            raise ValueError(f"{path}:{line_number}: invalid feature name {feature!r}")
        for label, digest in (("test", test_sha256), ("result", result_sha256)):
            if not re.fullmatch(r"[0-9a-f]{64}", digest):
                raise ValueError(f"{path}:{line_number}: invalid {label} SHA-256 {digest!r}")
        seen.add(name)
        cases.append(
            TestCase(
                name=name,
                feature=feature,
                test_sha256=test_sha256,
                result_sha256=result_sha256,
                source=str(path),
            )
        )
    if not cases:
        raise ValueError(f"{path}: allowlist is empty")
    return cases


def mysql_test_file(suite_root: Path, name: str) -> Path:
    mysql_test = suite_root / "mysql-test"
    if "/" not in name:
        return mysql_test / "t" / f"{name}.test"
    suite, test = name.split("/", 1)
    return mysql_test / "suite" / suite / "t" / f"{test}.test"


def mysql_result_file(suite_root: Path, name: str) -> Path:
    mysql_test = suite_root / "mysql-test"
    if "/" not in name:
        return mysql_test / "r" / f"{name}.result"
    suite, test = name.split("/", 1)
    return mysql_test / "suite" / suite / "r" / f"{test}.result"


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def validate_cases(suite_root: Path, cases: list[TestCase]) -> None:
    missing = [
        case.name
        for case in cases
        if not mysql_test_file(suite_root, case.name).is_file()
        or not mysql_result_file(suite_root, case.name).is_file()
    ]
    if missing:
        joined = ", ".join(missing)
        raise ValueError(f"allowlisted MTR test or result files are missing from {suite_root}: {joined}")
    for case in cases:
        test_file = mysql_test_file(suite_root, case.name)
        result_file = mysql_result_file(suite_root, case.name)
        actual_test = sha256_file(test_file)
        actual_result = sha256_file(result_file)
        if actual_test != case.test_sha256:
            raise ValueError(
                f"upstream test hash mismatch for {case.name}: "
                f"expected {case.test_sha256}, got {actual_test}"
            )
        if actual_result != case.result_sha256:
            raise ValueError(
                f"upstream result hash mismatch for {case.name}: "
                f"expected {case.result_sha256}, got {actual_result}"
            )


def parse_server_url(url: str) -> dict[str, str]:
    parsed = urlsplit(url)
    if parsed.scheme != "mysql" or not parsed.hostname:
        raise ValueError(f"expected a mysql:// URL, got {url!r}")
    return {
        "host": parsed.hostname,
        "port": str(parsed.port or 3306),
        "user": unquote(parsed.username or "root"),
        "password": unquote(parsed.password or ""),
        "database": parsed.path.lstrip("/") or "test",
    }


def validate_distinct_servers(mysql_url: str, mysqweel_url: str | None) -> None:
    """Reject a comparison that would run both MTR sides against one server."""
    if not mysqweel_url:
        return
    mysql = parse_server_url(mysql_url)
    mysqweel = parse_server_url(mysqweel_url)
    if (mysql["host"], mysql["port"]) == (mysqweel["host"], mysqweel["port"]):
        raise ValueError(
            "--mysql-url and --mysqweel-url point to the same host and port; "
            "use --mysqweel-bin or a separately running MySqweel server"
        )


def validate_mtr_runtime(client_bindir: Path, mysqltest_path: Path) -> None:
    if not mysqltest_path.is_file():
        raise FileNotFoundError(f"mysqltest not found: {mysqltest_path}")
    safe_process = client_bindir / "mysqltest_safe_process"
    if not safe_process.is_file():
        raise FileNotFoundError(f"mysqltest_safe_process not found: {safe_process}")
    with tempfile.TemporaryDirectory() as directory:
        marker = Path(directory) / "safe-process-canary"
        try:
            canary = subprocess.run(
                [
                    str(safe_process),
                    "--",
                    sys.executable,
                    "-c",
                    "from pathlib import Path; Path(__import__('sys').argv[1]).write_text('ok')",
                    str(marker),
                ],
                capture_output=True,
                text=True,
                errors="replace",
                timeout=10,
                check=False,
            )
        except subprocess.TimeoutExpired as error:
            raise RuntimeError("mysqltest_safe_process execution canary timed out") from error
        if canary.returncode != 0 or not marker.is_file() or marker.read_text() != "ok":
            raise RuntimeError(
                "mysqltest_safe_process did not execute the canary child process; "
                f"exit code {canary.returncode}"
            )


def wait_for_port(host: str, port: int, process: subprocess.Popen[str], timeout: float = 30) -> None:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if process.poll() is not None:
            raise RuntimeError(f"MySqweel exited before opening {host}:{port}")
        try:
            with socket.create_connection((host, port), timeout=0.25):
                return
        except OSError:
            time.sleep(0.1)
    raise RuntimeError(f"timed out waiting for MySqweel at {host}:{port}")


def free_port(host: str) -> int:
    with socket.socket() as sock:
        sock.bind((host, 0))
        return int(sock.getsockname()[1])


def ensure_mtr_database(server: Server, client_bindir: Path) -> None:
    connection = parse_server_url(server.url)
    mysql = client_bindir / "mysql"
    command = [
        str(mysql),
        "--no-defaults",
        f"--user={connection['user']}",
        f"--password={connection['password']}",
        f"--host={connection['host']}",
        f"--port={connection['port']}",
        "--protocol=TCP",
        "--execute=CREATE DATABASE IF NOT EXISTS test; "
        "CREATE DATABASE IF NOT EXISTS mtr; "
        "SET GLOBAL log_bin_trust_function_creators = 1; "
        "SET GLOBAL sql_require_primary_key = 0; "
        "DROP PROCEDURE IF EXISTS mtr.add_suppression; "
        "CREATE PROCEDURE mtr.add_suppression(IN message TEXT) BEGIN END",
    ]
    completed = subprocess.run(
        command,
        capture_output=True,
        text=True,
        errors="replace",
        timeout=30,
        check=False,
        env=os.environ.copy(),
    )
    if completed.returncode:
        raise RuntimeError(
            "could not provision the MTR helper database on the MySQL baseline: "
            f"{completed.stderr.strip()}"
        )


def reset_test_database(server: Server, client_bindir: Path) -> None:
    connection = parse_server_url(server.url)
    base_command = [
        str(client_bindir / "mysql"),
        "--no-defaults",
        f"--user={connection['user']}",
        f"--password={connection['password']}",
        f"--host={connection['host']}",
        f"--port={connection['port']}",
        "--protocol=TCP",
    ]

    def query(sql: str, tabular: bool = False) -> subprocess.CompletedProcess[str]:
        command = [*base_command]
        if tabular:
            command.extend(("--batch", "--skip-column-names"))
        command.append(f"--execute={sql}")
        return subprocess.run(
            command,
            capture_output=True,
            text=True,
            errors="replace",
            timeout=30,
            check=False,
        )

    databases = query("SHOW DATABASES", tabular=True)
    users = query(
        "SELECT CONCAT(user, CHAR(9), host) FROM mysql.user "
        "WHERE user NOT IN ('root', 'mysql.infoschema', 'mysql.session', 'mysql.sys')",
        tabular=True,
    )
    if databases.returncode or users.returncode:
        stderr = (databases.stderr or users.stderr).strip()
        raise RuntimeError(f"could not inspect MySQL state before an MTR case: {stderr}")

    statements = []
    protected_databases = {"information_schema", "mysql", "performance_schema", "sys", "mtr", "test"}
    for database in databases.stdout.splitlines():
        database = database.strip()
        if database and database not in protected_databases:
            escaped = database.replace("`", "``")
            statements.append(f"DROP DATABASE IF EXISTS `{escaped}`")
    for user_host in users.stdout.splitlines():
        user, _, host = user_host.partition("\t")
        if user and host:
            escaped_user = user.replace("'", "''")
            escaped_host = host.replace("'", "''")
            statements.append(f"DROP USER IF EXISTS '{escaped_user}'@'{escaped_host}'")
    statements.extend(
        (
            "SET GLOBAL log_bin_trust_function_creators = 1",
            "SET GLOBAL sql_require_primary_key = 0",
            "DROP DATABASE IF EXISTS test",
            "CREATE DATABASE test",
        )
    )
    completed = query("; ".join(statements))
    if completed.returncode:
        raise RuntimeError(
            "could not reset the MySQL MTR test database: " f"{completed.stderr.strip()}"
        )


def mtr_command(
    suite_root: Path,
    mysqltest_runner: Path,
    client_bindir: Path,
    server: Server,
    case: TestCase,
    vardir: Path,
) -> list[str]:
    connection = parse_server_url(server.url)
    suite, test = (case.name.split("/", 1) if "/" in case.name else ("main", case.name))
    command = [
        "perl",
        str(mysqltest_runner),
        f"--vardir={vardir}",
        f"--client-bindir={client_bindir}",
        "--retry=0",
        "--skip-rpl",
        f"--suite={suite}",
    ]
    for key in ("host", "port", "user", "password", "database"):
        command.append(f"--extern={key}={connection[key]}")
    command.append(test)
    return command


def run_case(
    suite_root: Path,
    mysqltest_runner: Path,
    client_bindir: Path,
    server: Server,
    case: TestCase,
    artifact_dir: Path,
    mysqltest_bin: Path,
) -> Invocation:
    case_artifact = artifact_dir / server.name / case.name.replace("/", "_")
    case_artifact.mkdir(parents=True, exist_ok=True)
    # MTR copies its roughly 500 MiB std_data directory into every vardir by
    # default.  The allowlist runs against an external server, so the data is
    # read-only input; linking it keeps the report bounded and avoids making a
    # separate copy for every case.
    std_data = suite_root / "mysql-test" / "std_data"
    vardir_std_data = case_artifact / "std_data"
    if std_data.is_dir() and not vardir_std_data.exists():
        vardir_std_data.symlink_to(std_data.resolve(), target_is_directory=True)
    command = mtr_command(suite_root, mysqltest_runner, client_bindir, server, case, case_artifact)
    environment = os.environ.copy()
    environment["MYSQL_TEST"] = str(mysqltest_bin)
    try:
        completed = subprocess.run(
            command,
            cwd=suite_root / "mysql-test",
            env=environment,
            capture_output=True,
            text=True,
            errors="replace",
            timeout=300,
            check=False,
        )
        stdout = completed.stdout
        stderr = completed.stderr
        status = "pass" if completed.returncode == 0 else "fail"
        suite, test = (
            case.name.split("/", 1) if "/" in case.name else ("main", case.name)
        )
        if completed.returncode == 0 and (
            f"{suite}.{test}" not in stdout or "Completed: All" not in stdout
        ):
            status = "infrastructure"
            stderr = (
                f"{stderr}\nMTR execution canary failed: the runner did not report "
                f"a completed pass for {case.name}"
            ).strip()
        (case_artifact / "stdout.log").write_text(stdout)
        (case_artifact / "stderr.log").write_text(stderr)
        return Invocation(
            test=case.name,
            server=server.name,
            status=status,
            returncode=completed.returncode,
            command=command,
            stdout=stdout,
            stderr=stderr,
            artifact_dir=str(case_artifact),
        )
    except subprocess.TimeoutExpired as error:
        stdout = error.stdout or ""
        stderr = error.stderr or ""
        (case_artifact / "stdout.log").write_text(stdout)
        (case_artifact / "stderr.log").write_text(stderr)
        return Invocation(
            test=case.name,
            server=server.name,
            status="infrastructure",
            returncode=None,
            command=command,
            stdout=stdout,
            stderr=f"MTR test timed out after 300 seconds\n{stderr}",
            artifact_dir=str(case_artifact),
        )


def start_mysqweel(binary: Path, report_dir: Path) -> tuple[Server, subprocess.Popen[str]]:
    host = "127.0.0.1"
    port = free_port(host)
    report_dir.mkdir(parents=True, exist_ok=True)
    log = report_dir / "mysqweel.log"
    stream = log.open("w")
    process = subprocess.Popen(
        [str(binary), "--bind", f"{host}:{port}", "--mysql-strict", "serve"],
        stdout=stream,
        stderr=subprocess.STDOUT,
        text=True,
        errors="replace",
    )
    try:
        wait_for_port(host, port, process)
    except Exception:
        process.terminate()
        process.wait(timeout=5)
        stream.close()
        raise
    return Server("mysqweel", f"mysql://root@{host}:{port}/test"), process


def render_markdown(report: dict) -> str:
    counts = report["counts"]
    lines = [
        f"# MySQL {report['mysql_version']} upstream compatibility",
        "",
        f"- Source revision: `{report['source_revision']}`",
        f"- Target: `{report.get('target', 'both')}`",
        f"- Included tests: {counts['included']}",
        f"- Passed: {counts['passed']}",
        f"- Failed: {counts['failed']}",
        f"- Infrastructure failures: {counts['infrastructure']}",
        f"- Score: {report['score_percent']:.1f}%",
        f"- Required floor: {report.get('minimum_percent', 90.0):.1f}%",
        f"- Status: **{report['status']}**",
        "",
        (
            "A compatibility test is counted only when the unmodified, hash-pinned upstream "
            "MTR test passes for every server evaluated by this report."
        ),
        "",
        "| Test | Feature | MySQL baseline | MySqweel |",
        "| --- | --- | --- | --- |",
    ]
    for result in report["results"]:
        lines.append(
            f"| `{result['test']}` | `{result['feature']}` | "
            f"{result['mysql']} | {result['mysqweel']} |"
        )

    failed_by_server: dict[str, dict] = {}
    for invocation in report.get("invocations", []):
        if invocation.get("status") == "pass":
            continue
        failed_by_server.setdefault(invocation["server"], invocation)
    if failed_by_server:
        lines.extend(["", "## Representative failure diagnostics", ""])
        for server, invocation in failed_by_server.items():
            streams = []
            for stream_name in ("stdout", "stderr"):
                output = (invocation.get(stream_name) or "").strip()
                if len(output) > 1_000:
                    output = "... output truncated ...\n" + output[-1_000:]
                if output:
                    streams.append(f"{stream_name}:\n{output}")
            output = "\n\n".join(streams)
            lines.extend(
                [
                    f"### {server}: `{invocation['test']}`",
                    "",
                    f"Return code: `{invocation.get('returncode')}`",
                    "",
                ]
            )
            lines.extend(f"    {line}" for line in (output or "No output captured.").splitlines())
    return "\n".join(lines) + "\n"


def run(args: argparse.Namespace) -> int:
    suite_root = args.suite_root.resolve()
    allowlist = args.allowlist.resolve()
    report_dir = args.report_dir.resolve()
    report_dir.mkdir(parents=True, exist_ok=True)
    cases = parse_manifest(allowlist)
    validate_cases(suite_root, cases)

    runner = (args.mtr_runner or suite_root / "mysql-test" / "mysql-test-run.pl").resolve()
    if not runner.is_file():
        raise FileNotFoundError(f"MTR runner not found: {runner}")
    mysqltest = args.mysqltest_bin or shutil.which("mysqltest")
    if not mysqltest:
        raise FileNotFoundError("mysqltest not found; pass --mysqltest-bin or set MYSQL_TEST")
    mysqltest_path = Path(mysqltest).resolve()
    client_bindir = args.client_bindir.resolve() if args.client_bindir else mysqltest_path.parent
    if not (client_bindir / "mysql").exists():
        raise FileNotFoundError(f"mysql client not found in {client_bindir}")
    validate_mtr_runtime(client_bindir, mysqltest_path)

    run_mysql = args.target in ("mysql", "both")
    run_mysqweel = args.target in ("mysqweel", "both")
    mysql_server: Server | None = None
    mysql_url = args.mysql_url or os.environ.get("MYSQL_COMPARE_URL")
    if run_mysql:
        if not mysql_url:
            raise ValueError("--mysql-url or MYSQL_COMPARE_URL is required for the MySQL target")
        mysql_server = Server("mysql", mysql_url)
        ensure_mtr_database(mysql_server, client_bindir)
    if mysql_url:
        validate_distinct_servers(mysql_url, args.mysqweel_url)
    binary = (args.mysqweel_bin or Path("target/debug/sqwl")).resolve()
    if run_mysqweel and not args.mysqweel_url and not binary.is_file():
        raise FileNotFoundError(f"MySqweel binary not found: {binary}")

    results: list[dict] = []
    invocations: list[Invocation] = []
    for case in cases:
        mysql_result: Invocation | None = None
        mysqweel_result: Invocation | None = None
        if mysql_server is not None:
            reset_test_database(mysql_server, client_bindir)
            mysql_result = run_case(
                suite_root, runner, client_bindir, mysql_server, case, report_dir, mysqltest_path
            )
            invocations.append(mysql_result)

        mysqweel_process: subprocess.Popen[str] | None = None
        if run_mysqweel:
            if args.mysqweel_url:
                mysqweel_server = Server("mysqweel", args.mysqweel_url)
            else:
                mysqweel_server, mysqweel_process = start_mysqweel(
                    binary, report_dir / "mysqweel" / case.name.replace("/", "_")
                )
            try:
                mysqweel_result = run_case(
                    suite_root,
                    runner,
                    client_bindir,
                    mysqweel_server,
                    case,
                    report_dir,
                    mysqltest_path,
                )
                invocations.append(mysqweel_result)
            finally:
                if mysqweel_process is not None:
                    mysqweel_process.terminate()
                    try:
                        mysqweel_process.wait(timeout=5)
                    except subprocess.TimeoutExpired:
                        mysqweel_process.kill()
                        mysqweel_process.wait()

        evaluated = [result for result in (mysql_result, mysqweel_result) if result is not None]
        results.append(
            {
                "test": case.name,
                "feature": case.feature,
                "test_sha256": case.test_sha256,
                "result_sha256": case.result_sha256,
                "mysql": mysql_result.status if mysql_result else "not-run",
                "mysqweel": mysqweel_result.status if mysqweel_result else "not-run",
                "status": "pass"
                if evaluated and all(result.status == "pass" for result in evaluated)
                else "fail",
            }
        )

    baseline_failures = sum(
        result["mysql"] != "pass" for result in results if result["mysql"] != "not-run"
    )
    infrastructure = sum(
        invocation.status == "infrastructure" for invocation in invocations
    )
    passed = sum(result["status"] == "pass" for result in results)
    failed = len(results) - passed
    status = (
        "invalid"
        if baseline_failures or infrastructure
        else ("pass" if passed * 100.0 / len(results) >= args.minimum_percent else "fail")
    )
    report = {
        "schema": "my-sqweel.mysql-mtr-compatibility.v2",
        "status": status,
        "target": args.target,
        "mysql_version": args.mysql_version,
        "source_revision": args.source_revision,
        "minimum_percent": args.minimum_percent,
        "counts": {
            "included": len(results),
            "passed": passed,
            "failed": failed,
            "infrastructure": infrastructure,
            "baseline_failures": baseline_failures,
        },
        "score_percent": passed * 100.0 / len(results),
        "results": results,
        "invocations": [asdict(invocation) for invocation in invocations],
    }
    (report_dir / "mysql-mtr-report.json").write_text(json.dumps(report, indent=2) + "\n")
    (report_dir / "mysql-mtr-report.md").write_text(render_markdown(report))
    print(render_markdown(report), end="")
    return 0 if status == "pass" else 1


def parser() -> argparse.ArgumentParser:
    result = argparse.ArgumentParser(description=__doc__)
    result.add_argument("--suite-root", type=Path, required=True)
    result.add_argument("--allowlist", type=Path, default=DEFAULT_ALLOWLIST)
    result.add_argument("--report-dir", type=Path, default=Path("artifacts/mysql-mtr"))
    result.add_argument("--target", choices=("mysql", "mysqweel", "both"), default="both")
    result.add_argument("--mysql-url")
    result.add_argument("--mysqweel-url")
    result.add_argument("--mysqweel-bin", type=Path)
    result.add_argument("--mtr-runner", type=Path)
    result.add_argument("--mysqltest-bin", type=Path)
    result.add_argument("--client-bindir", type=Path)
    result.add_argument("--mysql-version", default="8.0.43")
    result.add_argument("--source-revision", default="mysql-8.0.43")
    result.add_argument("--minimum-percent", type=float, default=100.0)
    return result


if __name__ == "__main__":
    try:
        raise SystemExit(run(parser().parse_args()))
    except (FileNotFoundError, RuntimeError, ValueError) as error:
        print(f"mysql MTR compatibility runner: {error}", file=sys.stderr)
        raise SystemExit(2)
