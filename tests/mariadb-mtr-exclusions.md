# MariaDB upstream test exclusions

The MTR compatibility percentage uses the explicit manifest in
`tests/mariadb-mtr-allowlist.txt`. Each entry names one complete, unmodified
upstream file and pins the SHA-256 of both its `.test` and `.result` files.
The full MariaDB suite is not the denominator: many files combine supported SQL
with behavior that MySqweel intentionally does not provide.

| Excluded area | Reason |
| --- | --- |
| Transactions, isolation, savepoints, locking, and XA | Outside the streamlined compatibility contract. |
| Replication, binary logging, group replication, and NDB | Require server topology or storage engines that MySqweel does not implement. |
| Users, grants, authentication plugins, and TLS | MySqweel exposes a local development wire endpoint, not MySQL access control. |
| Stored procedures, stored functions, triggers, and events | Outside the supported SQL surface. |
| Tests whose main path requires stored-function creation (`create`, `func_math`) | The allowlist measures the supported SQL surface, not routines. |
| File output/removal tests (`distinct`) | MTR file-system side effects are outside the SQL wire compatibility contract. |
| Optimizer plans, hints, index statistics, and performance tests | Exact optimizer behavior is not part of the contract. |
| GIS, full-text indexes, partitioning, and specialized storage engines | Not implemented by the in-memory engine. |
| Platform, crash, debug, and resource-limit tests | Environment or process behavior is not SQL compatibility. |

## Admission rules

An upstream test is admitted only when all of the following are true:

1. The complete, unmodified file passes against MariaDB 10.11.7 through MTR's
   external-server mode.
2. Every statement and expected side effect in the file is inside MySqweel's
   documented compatibility boundary.
3. The complete file passes against MySqweel using the same official
   MariaDB `mysqltest`-compatible binary and upstream expected result.
4. The manifest pins the exact upstream test and result hashes from Ubuntu
   package revision `1:10.11.7-2ubuntu2`.

An excluded test must not be added merely to improve the percentage. Broad
files such as `alter_table`, `select_all`, and `func_str` remain excluded even
when MySqweel supports part of their behavior, because their complete files
also exercise excluded capabilities.

## Current upstream coverage

| Test | Feature evidence |
| --- | --- |
| `adddate_454` | Date-column writes, interval arithmetic, and warning behavior. |
| `timezone4` | `FROM_UNIXTIME` and `UNIX_TIMESTAMP` date/time behavior. |

The focused non-gating audit scope in
[`tests/mariadb-mtr-scope.txt`](mariadb-mtr-scope.txt) expands this to 21 complete files and
305 SQL statements across DDL, DML, aggregates, subqueries, date/time, window functions, and
JSON. Its results are reported separately from the strict manifest; a file is promoted only
after it passes in full against both engines.

Features without a suitable complete upstream file are covered by the
differential corpus and focused parity tests. They are not represented as an
upstream MTR pass until a complete qualifying file is found or MySqweel grows
to support the rest of the relevant upstream file.

## Automated discovery

The non-gating
[MariaDB MTR discovery workflow](../.github/workflows/mariadb-mtr-discovery.yml)
inventories the test files in the pinned MariaDB 10.11.7 ARM64 distribution. The
static audit identifies complete-file candidates containing direct SQL statements after
excluding tests with missing results, custom delimiters, harness dependencies,
server options, topology requirements, or behavior outside the compatibility
contract.

Each weekly or manually triggered run selects a rotating batch of 100
candidates, validates each file against MariaDB first, and runs baseline passes
against MySqweel. The workflow publishes the inventory, complete execution
reports, and a generated promotion manifest containing only files that passed
both engines. Promotion into the strict manifest still requires review against
the admission rules above.
