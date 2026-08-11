# Changelog

All notable changes to MySqweel will be documented in this file.

## 0.4.0 Aug 10, 2026

- Repositioned MySqweel as streamlined, embeddable MySQL for applications, testing, and QA, with a prominent in-process `Engine` workflow and an explicit transactions/atomicity compatibility boundary.
- Added an opt-in query event stream for embedded Rust users, with unique query IDs, received/completed lifecycle events, query text, execution duration, result-set counts and row sizes, and failure details.
- Added optional full query-result payloads to completion events; result payloads remain disabled by default to avoid copying large result sets.
- Expanded MySQL sorting parity across primitive and compound ordering, including exact integer/decimal handling, FLOAT rounding, temporal, binary, text collation, JSON, ENUM, and SET behavior across window, aggregate, DML, derived-table, and set-operation paths.
- Added regression coverage for sorting edge cases, compound-key stress, aliases and expressions, `GROUP_CONCAT` ordering, `DELETE ... ORDER BY`, windows, derived tables, and `UNION` results.
- Expanded MySQL wire compatibility with declared result-column types, nullability, unsigned and decimal metadata, character sets and collations, warning propagation, `SHOW WARNINGS`, zero date and datetime values, and additional MySQL error-code mappings.
- Expanded query compatibility with correlated `EXISTS`, nested joins, `DUAL`, user variables, aggregate expressions, `EXPLAIN`, `CREATE TABLE ... AS SELECT`, ordered and limited deletes, and stricter MySQL value, safe-update, auto-increment, and decimal semantics.
- Expanded DDL and metadata coverage for case-insensitive schemas, foreign-key indexes, index comments, `SHOW FULL COLUMNS`, `SHOW TABLE STATUS`, `information_schema` engines, process lists, variables, indexes, constraints, and richer table/index introspection.
- Added MySQL date/time and scalar compatibility for `TIME_FORMAT`, `STR_TO_DATE`, `GET_FORMAT`, `FROM_UNIXTIME`, `INTERVAL`, `SOUNDEX`, `MD5`, `SHA`, `SHA2`, `CRC32`, `BIT_LENGTH`, and expanded aggregate support including variance, standard deviation, and bitwise aggregates.
- Added a persistent engine performance benchmark covering filtered and compound `ORDER BY`/`LIMIT` queries over 4,000 rows.
- Improved query execution by resolving sort keys once per row, reusing stored schema column order, avoiding unnecessary row/schema/projection/window/aggregate allocations, and using hash-based lookup and deduplication paths where ordering is not required.
- Added a no-subscriber fast path that avoids query-event timing, IDs, and result processing when query lifecycle events are not being observed.
- Added logical row/cell read and physical row/cell write metrics to query completion events, including filtered rows and aggregate multi-statement totals.

## 0.3.2 July 24, 2026

### Added

- Added the `mysql-test-server` helper under `vendor/` and installed MySQL client/server packages in the Rust image (`vendor/rust.dockerfile`) so image-based test flows can provision a local MySQL instance for parity checks.
- Added dedicated MySQL parity coverage for conditional and null-control expressions (`COALESCE`, `NULLIF`, `IF`, `CASE`) including prepared-statement execution.
- Added MySQL parity coverage for `CASE` combined with window functions under prepared execution.
- Added MySQL parity coverage for `CASE` combined with ranking window functions (`ROW_NUMBER`, `RANK`) under prepared execution.
- Added MySQL parity coverage for JSON/datetime expressions, including prepared JSON construction and extraction paths.
- Added MySQL parity coverage for JSON collection-path semantics (nested arrays/objects, multi-path extraction, index mutation/removal, and prepared JSON mutation/composition).

### Fixed

- Improved `mysql-test-server` startup behavior to be deterministic for already-running servers vs. servers started by the helper, and removed `MYSQL_ROOT_PASSWORD`-specific URL/user setup from helper flow.
- Fixed SQL projection metadata inference so computed expressions now default to nullable metadata when value-based inference cannot prove non-nullability, preventing MySQL protocol `NOT NULL` false positives (for example `NULLIF(...) AS not_alice`).

## 0.3.1 July 24, 2026

### Added

- Added `engine` (`InnoDB`) and live `table_rows` values to `information_schema.tables` metadata.

### Fixed

- Fixed MySQL wire results to accept MySQL, ISO 8601, and RFC 3339 datetime values, and to return a MySQL error for invalid non-null, date/time, or numeric result values before beginning a result set.
- Fixed mysql2 prepared `LIMIT` and `OFFSET` parameters encoded as integral floating-point values.
- Fixed integer and `BIGINT` comparisons/casts to preserve exact 64-bit integer semantics for parseable integral values, avoiding lossy float coercion and overflow in unary arithmetic.
- Fixed SQL statement splitting when quoted values contain backslash-escaped quotes, including mysql2 query-protocol JSON payloads.
- Fixed `DEFAULT` handling in inserts and updates: it no longer conflicts with the literal string `DEFAULT`, nullable columns without declared defaults receive `NULL`, and explicit `NULL` values remain unchanged.
- Fixed single-table `DELETE` predicates that qualify columns with the table name or its alias.
- Fixed aggregate `HAVING` evaluation to correctly use both grouped aggregate aliases and base-row expressions (including predicates that combine grouped and non-grouped terms).
- Fixed SQL function-call parsing to only treat `name(...)` as a function when the closing parenthesis is the terminal wrapper, avoiding false positives while parsing function-like text.
- Fixed MySQL decimal result encoding to avoid panicking when per-column scale metadata is missing by defaulting to zero scale.

## 0.3.0 July 13, 2026

### Added

- Added an opt-in `--mysql-strict` compatibility profile. Strict mode disables implicit table and column creation, enforces declared types, ranges, lengths, nullability, defaults, generated columns, unique keys, and foreign keys, and returns MySQL wire error numbers for common failures. The default profile remains drift tolerant.
- Added declared and inferred result-column metadata for integer widths, unsigned values, floating point and decimal types, date/time, binary, JSON, nullability, source tables, character sets, collations, and scale. Prepared statements now expose this metadata before execution.
- Added typed MySQL wire result encoding for signed and unsigned numeric widths, floats, doubles, decimals, `DATE`, `DATETIME`, `TIMESTAMP`, `TIME` (including negative and fractional values), JSON, and binary data, plus prepared-parameter decoding for native binary date/time values.
- Added qualified wildcards; `RIGHT`, `CROSS`, `USING`, `NATURAL`, and derived-table joins; and nonrecursive common table expressions with optional column aliases.
- Added `UNION`, `INTERSECT`, and `EXCEPT` set semantics, including `ALL`/`DISTINCT` handling, left-branch column names, and branch-arity validation.
- Added named and inline window specifications, common `ROWS` and peer-aware `RANGE` frames, aggregate windows, and `ROW_NUMBER`, `RANK`, `DENSE_RANK`, `PERCENT_RANK`, `CUME_DIST`, `NTILE`, `LAG`, `LEAD`, `FIRST_VALUE`, `LAST_VALUE`, and `NTH_VALUE`.
- Added MySQL multi-table `DELETE` target-list and `DELETE ... USING` forms.
- Added migration-oriented DDL support for temporary tables; virtual and stored generated columns; prefix indexes; `ALTER TABLE` add, drop, rename, change, modify, default/type/nullability, column positioning, table rename, and index operations; and matching `SHOW CREATE TABLE` output.
- Added foreign-key schema validation and insert/update/delete enforcement with `CASCADE`, `SET NULL`, `RESTRICT`, and `NO ACTION` behavior for referenced rows.
- Added a fail-closed SQL support validator so unsupported operators, expressions, functions, query modifiers, table factors, and join forms return explicit errors instead of partial results.
- Added a deterministic 2,500-query differential corpus with a 95% compatibility floor, exact row/result parity tests, MySQL wire error-code checks, and ORM-shaped migration, prepared CRUD, relation, and introspection coverage for Diesel, Drizzle/Knex, Prisma, and SeaORM patterns.
- Added GitHub Actions coverage against MySQL 8.0.43. Real-MySQL parity is non-skippable in CI, and local compatibility tests provision the same image when Docker and the image are available.

### Changed

- Aligned expression evaluation with MySQL three-valued logic, numeric-prefix coercion, case-insensitive string equality, `DIV` and bitwise behavior, byte versus character string lengths, and NULL propagation through unary, comparison, `IN`, `BETWEEN`, and logical operators.
- Aligned `SELECT DISTINCT`, nested aggregate expressions, empty aggregate sets, scalar-subquery cardinality errors, qualified-name resolution, derived columns, ordering aliases, and set-result naming with MySQL behavior.
- Improved `SHOW COLUMNS`, `SHOW INDEX`, `SHOW CREATE TABLE`, and `information_schema` metadata for data types, nullability, ordinal positions, primary/unique/secondary keys, index prefix lengths, generated columns, and referential constraints.
- Aligned affected-row counts with MySQL: changed `ON DUPLICATE KEY UPDATE` rows report two, unchanged duplicate updates report zero, and replacements of existing rows report two.
- Changed declared schemas to be authoritative for reads: unknown tables, ambiguous references, and columns removed by `ALTER TABLE` now return errors instead of silently producing NULL values.
- Reworked the README around an accurate quick start, compatibility-profile comparison, verified SQL contract, local-data and search workflows, security warning, and explicit limitations.

### Fixed

- Fixed foreign-key insert validation deadlocking when child and parent tables occupied the same internal map shard.
- Fixed `ALTER TABLE RENAME COLUMN` and `CHANGE COLUMN` to move existing stored values to the new column name and keep primary, unique, index, and local foreign-key column metadata consistent.
- Fixed peer-aware default `RANGE` window frames and `CUME_DIST`/`PERCENT_RANK` result typing so fractional results are not truncated based on the first row.
- Fixed prepared binary `DATE`, `DATETIME`, and signed fractional `TIME` parameter handling and result encoding.

### Compatibility notes

- Recursive CTEs, `FULL JOIN`, stored programs, and transaction semantics remain outside the supported compatibility surface and now fail explicitly where parsed.

## 0.2.4 - Jun 30, 2026

- Added broader MySQL date/time scalar support, including `DATE_ADD`/`ADDDATE`, `DATE_SUB`/`SUBDATE`, `TIMESTAMPADD`, `TIMESTAMPDIFF`, `DATEDIFF`, `ADDTIME`, `SUBTIME`, `TIMEDIFF`, `EXTRACT`, current/UTC date-time functions, date/time part functions, and expanded `DATE_FORMAT` tokens.
- Added common JSON scalar support for `JSON_EXTRACT`, `JSON_UNQUOTE`, `JSON_OBJECT`, `JSON_ARRAY`, `JSON_CONTAINS`, `JSON_SET`, and `JSON_REMOVE`.
- Added more string and numeric scalar functions, including `LEFT`, `RIGHT`, `LPAD`, `RPAD`, `LOCATE`, `INSTR`, `POSITION`, `REVERSE`, `REPEAT`, `ASCII`/`ORD`, `GREATEST`, `LEAST`, `SIGN`, `SQRT`, `LOG`, `EXP`, `TRUNCATE`, and function-form `MOD`.
- Improved `CAST`/`CONVERT` handling for date, time, datetime, JSON, and signed numeric conversions.
- Improved aggregate compatibility for `GROUP_CONCAT(... ORDER BY ... SEPARATOR ...)` and multi-expression `COUNT(DISTINCT ...)`.
- Split SQL evaluator helpers into focused date/time, JSON, scalar, and common helper modules.
- Expanded real-MySQL parity coverage for date/time, JSON, string, numeric, and aggregate function behavior.
- Switched the MySQL wire-protocol dependency to the vendored `msql-srv` copy under `vendor/msql-srv`.
- Patched the vendored `msql-srv` session loop to acknowledge `COM_CHANGE_USER` and avoid panicking on unsupported command parse failures.
- Fixed floating-point column coercion so integral inserts into `DOUBLE`, `FLOAT`, and `REAL` columns are stored and reported as floating-point values instead of integer metadata.
- Added regression coverage for MySQL date/time, JSON, string, numeric, conversion, and aggregate function evaluation.

## 0.2.3 - Jun 29, 2026

- Fixed `UPDATE ... JOIN` evaluation so `WHERE` clauses and assignment expressions use the joined row context, including table aliases.
- Fixed `UPDATE ... LEFT JOIN` handling so unmatched right-side rows are null-extended for predicates such as `joined_table.id IS NULL`.
- Fixed `INSERT ... SELECT` so source `ORDER BY` and `LIMIT` clauses are preserved.
- Added support for single-table `DELETE ... ORDER BY ... LIMIT`.
- Added explicit errors for unsupported `UPDATE ... FROM`, multi-table `DELETE`, `DELETE ... USING`, `DELETE` joins, and qualified correlated subqueries so they cannot be silently mis-evaluated.
- Improved `ON DUPLICATE KEY UPDATE` evaluation for expressions that mix existing row values with `VALUES(...)`.
- Added expanded regression and MySQL parity coverage for DML edge cases, including `UPDATE ... JOIN`, `UPDATE ... LEFT JOIN`, `INSERT ... SELECT` modifiers, limited deletes, duplicate-key update expressions, and unsupported syntax guards.

## 0.2.2 - Jun 27, 2026

- Refreshed the README branding and corrected the introductory project description.

## 0.2.1 - Jun 27, 2026

- Added SQL `RETURNING` support for `INSERT`, `UPDATE`, and `DELETE`, including projection expressions and aliases.
- Fixed `DELETE` predicate evaluation so subqueries do not deadlock while rows are being removed.

## 0.2.0 - Jun 25, 2026

- Added an always-on Meilisearch-compatible HTTP API on the debug HTTP port.
  - Meilisearch indexes map to MySqweel/MySQL tables.
  - Meilisearch documents map to stored rows.
  - The MySQL/table engine remains the source of truth.
- Added synchronous Tantivy-backed text search for the Meilisearch-compatible API.
  - Document and table mutations rebuild the derived search index before reporting task success.
  - Search falls back to row-scan compatibility for edge cases where Tantivy produces no candidates.
- Added Meilisearch-compatible index, document, search, multi-search, settings, task, key, stats, and swap-index endpoints.
- Added support for Meilisearch search options including filters, sort, pagination, `attributesToRetrieve`, `attributesToSearchOn`, `showRankingScore`, and `showRankingScoreDetails`.
- Added facet support for Meilisearch search responses, including `facetDistribution`, numeric `facetStats`, array facet values, and `facets: ["*"]`.
- Added a 90/10 Meilisearch compatibility pass for previously missing feature areas:
  - query-time synonym and typo-tolerance fallback matching
  - highlighting, cropping, and match-position metadata in search hits
  - `POST /indexes/:uid/facet-search`
  - synchronous in-memory dump status/download endpoints
  - webhook CRUD compatibility endpoints
  - permissive bearer/API-key handling for tenant-token-shaped local client requests
- Added task compatibility improvements:
  - write APIs return Meilisearch-shaped tasks
  - tasks include both `taskUid` and `uid`
  - task durations are serialized as strings
  - task listing supports `uids`, `types`, `statuses`, `indexUids`, ranges, pagination, `from`, and `next`
- Added official Meilisearch JavaScript client compatibility coverage via `tests/node/meili-js-client-compat.mjs` and `cargo test --test meili_js_client`.
- Added optional official Meilisearch Python client compatibility coverage via `tests/python/meili_client_compat.py` and `cargo test --test meili_python_client`.
- Added direct Meilisearch handler coverage for synonyms, typo tolerance, formatting, facet search, dumps, and webhooks.
- Added `npm run test:meili` and `requirements-dev.txt` for running SDK compatibility smoke tests outside Cargo.
- Fixed `sqwl serve` panic caused by nesting Tokio runtimes inside the synchronous server path.
- Fixed Meilisearch filter handling for multi-value `IN` and `NOT IN` expressions.
- Fixed Meilisearch ranking score metadata being stripped by `attributesToRetrieve`.
- Fixed fallback text search so `searchableAttributes` and `attributesToSearchOn` are respected consistently.
- Fixed primary key metadata reporting so `information_schema.key_column_usage` and related introspection stay consistent after `ALTER TABLE` operations.
- Fixed an issue with spawning connection sessions.
- Added more information_schema coverage to the backend with associated tests.

## 0.1.0 - May 31, 2026

### Added

- Added the `sqwl` binary with `serve`, `serve --repl`, `repl`, and `explain` commands.
- Added a lightweight maintenance REPL for status, drift reports, snapshots, index rebuilds, resets, SQL execution, help, and graceful `Ctrl+C` / `Ctrl+D` exit.
- Added MySQL wire-protocol support for local `mysql2`, Drizzle, and migration workflows.
- Added permissive schema behavior:
  - inserts can create missing tables and columns
  - repeated `CREATE TABLE` statements merge into existing metadata
  - reads return rows shaped to the latest known schema
  - stored rows are not rewritten just because the schema changed
- Added support for schema metadata from `CREATE TABLE`, `ALTER TABLE`, indexes, unique constraints, and advisory foreign keys.
- Added dynamic row materialization against the latest schema metadata.
- Added positional inserts that infer generated `column_1`, `column_2`, etc. columns when needed.
- Added configurable duplicate handling with `--unique-mode overwrite|enforce`.
- Added Lux-backed directory persistence with exclusive data-directory locking.
- Added debug HTTP endpoints for health, drift reporting, table inspection, snapshots, restore, and JSON table seeding.
- Added fault-injection flags for query delay and intermittent read/write failures.
- Added broader query coverage, including joins, grouping, aggregates, scalar expressions, common functions, simple derived tables, and uncorrelated subqueries.
- Added best-effort `information_schema` and MySQL metadata command support.
- Added compatibility smoke tests for `mysql2`, Drizzle, and real-MySQL parity checks.
- Added expanded MySQL parity coverage for functions, NULL predicates, prepared writes, defaults, arithmetic updates, and deletes.
- Added project logo usage in the README.
- Added focused SQL engine submodules under `src/sql/engine/`.

### Notes

- MySqweel does not provide ACID guarantees, transaction semantics, or full MySQL compatibility.
