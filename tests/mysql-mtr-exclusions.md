# MySQL upstream test exclusions

The MTR compatibility percentage uses the explicit manifest in
`tests/mysql-mtr-allowlist.txt`. Each entry names one complete, unmodified
upstream file and pins the SHA-256 of both its `.test` and `.result` files.
The full MySQL suite is not the denominator: many files combine supported SQL
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

1. The complete, unmodified file passes against MySQL 8.0.43 through MTR's
   external-server mode.
2. Every statement and expected side effect in the file is inside MySqweel's
   documented compatibility boundary.
3. The complete file passes against MySqweel using the same official
   `mysqltest` binary and upstream expected result.
4. The manifest pins the exact upstream test and result hashes from source
   revision `2d6d5e10436a8f2b58d37af737c2a3e45855d0b7`.

An excluded test must not be added merely to improve the percentage. Broad
files such as `alter_table`, `select_all`, and `func_str` remain excluded even
when MySqweel supports part of their behavior, because their complete files
also exercise excluded capabilities.

## Current upstream coverage

| Test | Feature evidence |
| --- | --- |
| `ctype_filename` | Creating and dropping tables with MySQL-sensitive identifier names. |
| `gcc296` | Table creation with indexes, multi-row inserts, and row selection. |

Features without a suitable complete upstream file are covered by the
differential corpus and focused parity tests. They are not represented as an
upstream MTR pass until a complete qualifying file is found or MySqweel grows
to support the rest of the relevant upstream file.
