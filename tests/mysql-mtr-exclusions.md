# MySQL upstream test exclusions

The MTR compatibility percentage uses the explicit allowlist in
`tests/mysql-mtr-allowlist.txt`. The full MySQL suite is not the denominator:
many tests exercise behavior that MySqweel intentionally does not provide.

| Excluded area | Reason |
| --- | --- |
| Transactions, isolation, savepoints, locking, and XA | Outside the development-only compatibility contract. |
| Replication, binary logging, group replication, and NDB | Require server topology or storage engines that MySqweel does not implement. |
| Users, grants, authentication plugins, and TLS | MySqweel exposes a local development wire endpoint, not MySQL access control. |
| Stored procedures, stored functions, triggers, and events | Outside the supported SQL surface. |
| Tests whose main path requires stored-function creation (`create`, `func_math`) | The allowlist measures the supported SQL surface, not routines. |
| File output/removal tests (`distinct`) | MTR file-system side effects are outside the SQL wire compatibility contract. |
| Optimizer plans, hints, index statistics, and performance tests | Exact optimizer behavior is not part of the contract. |
| GIS, full-text indexes, partitioning, and specialized storage engines | Not implemented by the in-memory engine. |
| Platform, crash, debug, and resource-limit tests | Environment or process behavior is not SQL compatibility. |

An excluded test must not be added to the allowlist merely to improve the
percentage. Add it only when its behavior is supported and reproducible on
both servers.
