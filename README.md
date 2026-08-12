<p align="center">
  <img src="logo.png" alt="MySqweel logo" width="280">
</p>

# MySqweel

<p align="center">
  <strong>Streamlined, embeddable MySQL for applications, testing, and QA.</strong>
</p>

<p align="center">
  <a href="https://github.com/only-cliches/my-sqweel/actions/workflows/ci.yml"><img src="https://github.com/only-cliches/my-sqweel/actions/workflows/ci.yml/badge.svg" alt="CI status"></a>
</p>

MySqweel is a lightweight reimplementation of core MySQL behavior. Embed the engine directly in a
Rust application like SQLite, or expose it through the MySQL wire protocol for existing clients,
ORMs, and migration tools. State stays easy to infer, inspect, seed, snapshot, reset, and
deliberately break.

Choose the default drift-tolerant profile for rapid iteration, or enable the strict profile when
compatibility matters more than convenience. The server process can also expose a debug API and a
Meilisearch-shaped search surface.

> **Compatibility boundary.** MySqweel is designed for workloads where transactions and atomicity
> are not critical. It does not provide ACID transactions, replication, access control, secure
> multi-tenant isolation, or complete MySQL compatibility.

## At a glance

| Surface | Default | Purpose |
| --- | --- | --- |
| Embedded Rust engine | In process | SQLite-like SQL execution without network or HTTP listeners |
| MySQL wire protocol | `127.0.0.1:3307` | Application, ORM, migration, and MySQL-client connections |
| Debug and search HTTP | `127.0.0.1:3407` | Drift inspection, seeding, snapshots, and local search |
| Storage | In memory | Disposable state; optional locked Lux-backed directory persistence |
| Compatibility profiles | Drift tolerant / MySQL strict | Choose convenience or fail-fast schema behavior |
| MySQL verification | MySQL 8.0.43 | Differential corpus and exact parity suites |
| Upstream MariaDB MTR verification | MariaDB 10.11.7 | 2 gated files / 21 scoped files / 305 SQL statements on ARM64 |

## Where it fits

MySqweel is useful for:

- embedding streamlined SQL storage directly in Rust applications
- early application development while the schema is changing
- local integration tests that need a disposable MySQL endpoint
- test harnesses, QA environments, and deterministic fixtures
- ORM, query-builder, migration, and seed-script development
- realistic UI flows without a full production-shaped stack
- schema-drift inspection and fixture management
- retry, loading, idempotency, and error-path testing
- local text, facet, and vector-search development
- demos, teaching, and experiments

Use real MySQL when your workload depends on transactions, atomic multi-statement writes,
permissions, replication, optimizer fidelity, security boundaries, high-concurrency durability,
scale, or compliance guarantees.

## Embed it

The engine can run entirely in process. This starts no TCP or HTTP listener:

```rust
use my_sqweel::sql::engine::{Engine, EngineConfig};

fn main() -> anyhow::Result<()> {
    let db = Engine::new(EngineConfig::mysql_strict());
    db.execute_sql("CREATE TABLE users (id INT PRIMARY KEY, name TEXT)")?;
    db.execute_sql("INSERT INTO users VALUES (1, 'Ada')")?;

    let results = db.execute_sql("SELECT id, name FROM users")?;
    println!("{:?}", results[0].rows);
    Ok(())
}
```

Use `Engine::default()` for drift-tolerant in-memory storage, or
`Engine::open_with_data_dir(...)` for directory-backed persistence.

## Quick start

### 1. Install from a checkout

You need a recent stable Rust toolchain and Cargo. A MySQL CLI is optional but useful for the
examples below.

```sh
git clone https://github.com/only-cliches/my-sqweel.git
cd my-sqweel
cargo install --path .
```

The installed binary is `sqwl`. You can also run it from the checkout with:

```sh
cargo run --bin sqwl -- serve
```

### 2. Start the server

```sh
sqwl serve
```

The MySQL and HTTP listeners bind to loopback by default:

```text
MySQL wire:   127.0.0.1:3307
Debug/search: 127.0.0.1:3407
```

### 3. Connect

```sh
mysql --protocol=TCP -h 127.0.0.1 -P 3307 -u root app
```

No password is required for the local connection. Try a normal schema and query flow:

```sql
CREATE TABLE users (
  id BIGINT PRIMARY KEY AUTO_INCREMENT,
  email VARCHAR(255) NOT NULL UNIQUE,
  display_name VARCHAR(100)
);

INSERT INTO users (email, display_name)
VALUES
  ('ada@example.test', 'Ada'),
  ('grace@example.test', 'Grace');

SELECT id, email, display_name
FROM users
ORDER BY id;
```

Point an application at the same endpoint:

```sh
export DATABASE_URL="mysql://root@127.0.0.1:3307/app"
```

For fail-fast compatibility work, start with the strict profile instead:

```sh
sqwl --mysql-strict serve
```

### Embed the engine and observe query metrics

Library users can subscribe to query lifecycle events. Completion events include logical rows and
cells read, including rows examined but rejected by a predicate, plus physical row and cell writes:

```rust
use my_sqweel::sql::engine::{Engine, QueryEvent, QueryEventOptions};

let engine = Engine::default();
let events = engine.subscribe_query_events(QueryEventOptions::metadata_only());
engine.execute_sql("SELECT id FROM users WHERE email LIKE '%example.test'")?;

let _received = events.recv()?;
if let QueryEvent::Completed(event) = events.recv()? {
    println!("rows read: {}", event.metrics.rows_read);
    println!("cells read: {}", event.metrics.cells_read);
}
```

These are logical execution metrics, not storage-I/O counters. Repeated join or subquery
examinations count repeatedly, and multi-statement API calls report aggregate totals.

## Choose a compatibility profile

MySqweel has two intentionally different compatibility profiles.

| Behavior | Drift tolerant (default) | MySQL strict (`--mysql-strict`) |
| --- | --- | --- |
| Missing tables or columns during writes | Infer and extend schema hints | Return an error |
| Repeated `CREATE TABLE` | Merge new hints into known metadata | Use MySQL-style exists behavior |
| Declared types, ranges, lengths, nulls, and defaults | Best-effort coercion | Validate and reject invalid values |
| Unique conflicts | Overwrite by default; configurable | Enforce uniqueness |
| Foreign keys | Enforce declared relationships and actions | Enforce relationships with MySQL-style errors |
| Best use | Embedded apps, prototypes, fixtures, changing DTOs | Application integration, ORMs, migrations, and compatibility tests |

Strict mode also returns common MySQL wire error numbers for missing tables and columns, duplicate
entries, null/default violations, invalid values, length/range errors, and foreign-key failures.

Strict mode narrows accidental differences; it does not turn MySqweel into MySQL. Both profiles use
the same supported SQL surface, and neither implements transaction semantics.

### Unique conflicts without full strict mode

The drift profile can enforce unique keys while retaining schema inference:

```sh
sqwl --unique-mode enforce serve
```

The default is `--unique-mode overwrite`, which is convenient for repeatable seeds. The strict
profile always enforces unique keys.

## CLI reference

```text
sqwl [options] serve [--repl]
sqwl [options] repl
sqwl explain <sql>
sqwl help
```

Global options in the table below must appear before the subcommand.

| Option | Purpose |
| --- | --- |
| `--bind <addr>` | MySQL bind address; default `127.0.0.1:3307` |
| `--debug-bind <addr>` | Debug/search HTTP bind; default is the MySQL port plus 100 |
| `--data-dir <dir>` | Enable locked Lux-backed directory persistence |
| `--allow-remote` | Permit non-loopback MySQL and HTTP bindings |
| `--mysql-strict` | Reject schema drift and use MySQL-style validation errors |
| `--unique-mode <mode>` | Choose `overwrite` or `enforce`; default `overwrite` |
| `--query-delay-ms <n>` | Add fixed latency to each SQL statement |
| `--fail-read-every <n>` | Fail every Nth read statement |
| `--fail-write-every <n>` | Fail every Nth write statement |
| `--snapshot-dir <path>` | REPL snapshot directory; default `.my-sqweel/snapshots` |
| `--log-filter <filter>` | Tracing filter; default `my_sqweel=info` |

The debug/search API is always enabled for `sqwl serve`. `--allow-remote` can expose unauthenticated,
state-mutating HTTP endpoints; never bind MySqweel to an untrusted network.

### Explain SQL without executing it

```sh
sqwl explain "SELECT id, email FROM users WHERE email = 'ada@example.test'"
```

Example output:

```json
{
  "count": 1,
  "statements": [
    {
      "kind": "query",
      "tables": ["users"],
      "normalized": "SELECT id, email FROM users WHERE email = 'ada@example.test'"
    }
  ]
}
```

## Local-data workflows

### Durable local state

Without `--data-dir`, state lives in memory and disappears with the process. To reuse a local
database between runs:

```sh
sqwl --data-dir .my-sqweel/data serve
```

The embedded Lux store locks the directory so two MySqweel processes cannot open it concurrently.
Directory persistence does not add transaction or atomic multi-statement guarantees.

### Maintenance REPL

Run the server and maintenance shell together:

```sh
sqwl serve --repl
```

Or open only the REPL, optionally against an existing data directory:

```sh
sqwl --data-dir .my-sqweel/data repl
```

Common commands:

```text
status
drift check
drift report
snapshot save <name>
snapshot restore <name>
snapshot list
index rebuild [--all|<table>]
reset [table]
explain <sql>
sql <sql>
help
quit
```

### Inspect schema drift

The drift report compares declared schema hints with stored rows:

```sh
curl http://127.0.0.1:3407/_drift/report
```

It reports known tables, row counts, declared columns, missing row fields, extra fields, and
duplicate values for unique constraints.

### Seed JSON directly

```sh
curl -X POST http://127.0.0.1:3407/_drift/tables/users/seed \
  -H 'content-type: application/json' \
  -d '{
    "mode": "replace",
    "rows": [
      {
        "email": "ada@example.test",
        "display_name": "Ada Lovelace",
        "role": "admin"
      },
      {
        "email": "grace@example.test",
        "display_name": "Grace Hopper",
        "role": "engineer"
      }
    ]
  }'
```

In the drift profile, the seed endpoint can infer a missing table and columns from the payload.

### Save and restore snapshots

The REPL stores named snapshots under `--snapshot-dir`:

```text
snapshot save before-auth-refactor
reset users
snapshot restore before-auth-refactor
```

The HTTP API can also return or restore complete engine snapshots:

```sh
curl -X POST http://127.0.0.1:3407/_drift/snapshot
```

### Inject failures

Add latency or deterministic read/write failures:

```sh
sqwl \
  --query-delay-ms 100 \
  --fail-read-every 10 \
  --fail-write-every 7 \
  serve
```

This is useful for testing retries, loading states, error handling, idempotency, and unhappy-path
user experiences.

## MySQL compatibility

MySqweel implements a practical, tested MySQL subset. Unsupported syntax returns an explicit error
instead of being silently evaluated as `NULL`, `FALSE`, or a partial result.

### Verification contract

- A deterministic 2,500-query corpus compares column names and normalized values with MySQL 8.0.43.
- The current corpus result is 2,500/2,500 exact matches; CI requires 100%.
- Broader parity tests require exact results for every claimed DDL, DML, metadata, and query shape.
- Wire tests verify common MySQL error numbers and typed prepared-statement behavior.
- ORM-shaped tests cover migration, CRUD, relation, and introspection patterns used by Diesel,
  Drizzle/Knex, Prisma, and SeaORM.
- The [MariaDB MTR workflow](.github/workflows/mariadb-mtr-discovery.yml) inventories the ARM64
  MariaDB 10.11.7 MTR distribution, filters complete external-server candidates, and rotates
  100-file batches through MariaDB and MySqweel. It also audits the focused
  [`tests/mariadb-mtr-scope.txt`](tests/mariadb-mtr-scope.txt) set covering 21 files and 305 SQL
  statements across DDL, DML, aggregates, subqueries, date/time, windows, and JSON.

The percentage describes this versioned corpus, not the entire MySQL grammar. Every reported edge
case should become a regression case before its implementation is changed.

The MariaDB MTR inventory is not itself a compatibility score: static candidates still have to
pass against both MariaDB and MySqweel. A discovered file becomes eligible for the strict CI gate
only after that dual-engine pass and a compatibility-boundary review. The strict manifest records
the exact upstream test and expected-result hashes in
[`tests/mariadb-mtr-allowlist.txt`](tests/mariadb-mtr-allowlist.txt). The broader focused scope
is intentionally non-gating until its complete files pass against both engines.

### Schema, DDL, and metadata

- `CREATE TABLE` and `CREATE TEMPORARY TABLE`
- primary, unique, secondary, prefix, and foreign-key metadata
- virtual and stored generated columns
- `ALTER TABLE` add, drop, rename, change, and modify column forms
- column defaults, types, nullability, `FIRST`, and `AFTER`
- `CREATE INDEX`, prefix indexes, `DROP INDEX`, and `ALTER TABLE ... DROP INDEX`
- `DROP TABLE`, `TRUNCATE TABLE`, and `RENAME TABLE`
- foreign-key validation and `CASCADE`, `SET NULL`, `RESTRICT`, and `NO ACTION`
- `SHOW TABLES`, `SHOW COLUMNS`, `SHOW INDEX`, `SHOW CREATE TABLE`, and `DESCRIBE`
- common `information_schema` views used by clients and ORMs

### Writes

- `INSERT ... VALUES` and `INSERT ... SELECT`
- `INSERT IGNORE`, `REPLACE`, and `ON DUPLICATE KEY UPDATE`
- `UPDATE`, including common joined-update forms
- single-table deletes with ordering/limits and MySQL multi-table delete forms
- `RETURNING` for inserts, updates, and deletes
- auto-increment keys, defaults, generated values, type coercion, and affected-row counts

### Queries

- `SELECT`, `DISTINCT`, aliases, qualified wildcards, and expression projections
- `WHERE`, `GROUP BY`, `HAVING`, `ORDER BY`, `LIMIT`, and `OFFSET`
- aggregate, scalar, date/time, JSON, string, numeric, and conversion functions
- broad JSON document functions, JSON aggregates, wildcard paths, arrow extraction, and basic `JSON_TABLE` projections
- `INNER`, `LEFT`, `RIGHT`, `CROSS`, `NATURAL`, `ON`, and `USING` joins
- derived tables and nonrecursive CTEs with column aliases
- scalar and `EXISTS`/`IN` subqueries
- `UNION`, `INTERSECT`, and `EXCEPT`, including `ALL`/`DISTINCT` variants
- named/inline windows, common `ROWS` frames, and peer-aware `RANGE` behavior
- `ROW_NUMBER`, `RANK`, `DENSE_RANK`, `PERCENT_RANK`, `CUME_DIST`, `NTILE`, `LAG`, `LEAD`,
  `FIRST_VALUE`, `LAST_VALUE`, `NTH_VALUE`, and aggregate windows
- MySQL-style three-valued logic, numeric-prefix coercion, and byte/character length behavior

See [CHANGELOG.md](CHANGELOG.md) for the detailed function and compatibility history.

### Wire and client behavior

- prepared statements and positional parameters
- declared/inferred result types and nullability
- signed/unsigned numeric widths, decimal scale, and source-table metadata
- typed `DATE`, `DATETIME`, `TIMESTAMP`, and signed fractional `TIME` values
- JSON and binary result metadata
- `LAST_INSERT_ID()`, `DATABASE()`, `SCHEMA()`, and common session variables
- charset, collation, and MySQL system-metadata stubs

### Explicit limits

The following are outside the supported compatibility surface:

- transaction semantics, isolation, savepoints, and row locking
- recursive CTEs
- `FULL JOIN`
- stored procedures, stored functions, triggers, and events
- replication, users, grants, and production authentication
- exact optimizer, index-planning, collation, and locking behavior
- the remainder of the MySQL grammar not listed above
- `JSON_VALUE` optional `RETURNING`/`ON EMPTY`/`ON ERROR` clauses (the pinned sqlparser version rejects those forms before execution)
- nested `JSON_TABLE` column expansion and MySQL binary-JSON storage byte-for-byte accounting
- the complete JSON Schema keyword vocabulary; the embedded validator currently covers the common structural/type constraints

Use real MySQL for tests that depend on any of these behaviors.

## Meilisearch-shaped local search

The debug HTTP listener also provides a local API shaped like Meilisearch. SQL tables remain the
source of truth; document mutations update table rows and rebuild the derived Tantivy search index.

Create an index and add documents:

```sh
curl -X POST http://127.0.0.1:3407/indexes \
  -H 'content-type: application/json' \
  -d '{ "uid": "books", "primaryKey": "id" }'

curl -X POST http://127.0.0.1:3407/indexes/books/documents \
  -H 'content-type: application/json' \
  -d '{
    "documents": [
      {
        "id": "1",
        "title": "Dune",
        "genre": "sci-fi",
        "rating": 10,
        "description": "Desert planet politics, spice, prophecy, and power."
      },
      {
        "id": "2",
        "title": "Foundation",
        "genre": "sci-fi",
        "rating": 8,
        "description": "Mathematics, empire, and a long plan."
      }
    ]
  }'
```

Search:

```sh
curl -X POST http://127.0.0.1:3407/indexes/books/search \
  -H 'content-type: application/json' \
  -d '{
    "q": "desert spice",
    "filter": "genre = \"sci-fi\"",
    "sort": ["rating:desc"],
    "attributesToRetrieve": ["id", "title", "rating"],
    "showRankingScore": true
  }'
```

The development compatibility surface includes document CRUD, filters, sorting, facets, facet
search, multi-search, settings, task-shaped responses, stats, dumps, webhooks, and API-key stubs.
Official Meilisearch JavaScript-client flows are covered by tests; Python client coverage is
available when its optional dependency is installed.

This is not a complete Meilisearch implementation. Authentication is permissive/stubbed, task
execution is local, and relevance is not guaranteed to match a production Meilisearch server.

### Vector search

Declare a vector column:

```sql
CREATE TABLE books (
  id TEXT PRIMARY KEY,
  title TEXT,
  embedding VECTOR(3)
);
```

Add vectors through the document endpoint, then search with a query vector:

```sh
curl -X POST http://127.0.0.1:3407/indexes/books/search \
  -H 'content-type: application/json' \
  -d '{
    "vector": [0.95, 0.05, 0.2],
    "vectorField": "embedding",
    "showRankingScore": true
  }'
```

Local vector ranking uses cosine similarity.

## HTTP endpoint map

| Endpoint | Purpose |
| --- | --- |
| `GET /health` | Basic process health |
| `GET /version` | Version payload |
| `GET /_drift/health` | Drift API health |
| `GET /_drift/report` | Schema-drift report |
| `GET /_drift/tables` | Known tables |
| `GET /_drift/tables/{table}/rows` | Inspect table rows |
| `POST /_drift/tables/{table}/seed` | Seed JSON rows |
| `POST /_drift/snapshot` | Export an engine snapshot |
| `POST /_drift/restore` | Restore an engine snapshot |
| `GET/POST /indexes` | List or create search indexes |
| `POST /indexes/{uid}/documents` | Add or update documents |
| `POST /indexes/{uid}/search` | Search an index |
| `POST /indexes/{uid}/facet-search` | Search facet values |
| `POST /multi-search` | Run multiple searches |
| `GET /tasks` | List task-shaped responses |
| `GET /stats` | Instance statistics |

Additional Meilisearch-shaped routes cover per-index settings and stats, document fetch/delete,
index swaps, dumps, webhooks, and keys.

## Development

Run the complete local suite:

```sh
cargo test --all-targets --locked
```

When Docker and a local MySQL-compatible image are available, compatibility tests provision and
remove their own comparison server. To require comparison or use an existing MySQL instance:

```sh
MYSQL_COMPARE_URL=mysql://root:password@127.0.0.1:3306/test \
MYSQL_PARITY_REQUIRED=1 \
cargo test --all-targets --locked
```

On an Ubuntu 24.04 ARM64 runner, reproduce the upstream MariaDB MTR comparison with the pinned
Ubuntu MariaDB packages:

```sh
eval "$(tools/prepare_mariadb_mtr.sh .cache/mariadb-mtr --print-env)"
export MARIADB_COMPARE_URL=mysql://root:password@127.0.0.1:3306/test
cargo build --locked --bin sqwl
python3 tools/mariadb_mtr_compat.py \
  --target both \
  --suite-root "$MARIADB_MTR_ROOT" \
  --allowlist tests/mariadb-mtr-allowlist.txt \
  --baseline-url "$MARIADB_COMPARE_URL" \
  --mysqltest-bin "$MYSQLTEST_BIN" \
  --client-bindir "$MYSQL_CLIENT_BINDIR" \
  --mtr-runner "$MTR_RUNNER" \
  --safe-process-bin "$MTR_SAFE_PROCESS" \
  --mtr-layout mariadb \
  --baseline-label MariaDB \
  --mysqweel-bin target/debug/sqwl \
  --report-dir artifacts/mariadb-mtr \
  --baseline-version 10.11.7 \
  --source-revision 10.11.7-2ubuntu2 \
  --minimum-percent 100
```

Formatting and linting:

```sh
cargo fmt --all --check
cargo clippy --all-targets --all-features
```

Run from the checkout with debug logs:

```sh
cargo run --bin sqwl -- --log-filter my_sqweel=debug serve --repl
```

When fixing a compatibility mismatch, add the smallest reproducing query to the differential
corpus or parity suite first. A useful report includes the schema, fixture rows, query, MySQL
version, expected result, and MySqweel result.

## Project layout

```text
src/bin/sqwl.rs                    CLI entrypoint
src/lib.rs                         CLI, REPL, snapshots, and SQL explain
src/server/mysql_wire.rs           MySQL wire protocol and typed results
src/server/debug_http.rs           Drift and Meilisearch-shaped HTTP APIs
src/sql/mod.rs                     MySQL-dialect parsing
src/sql/engine/                    SQL execution and compatibility validation
src/schema/mod.rs                  Schema-hint model
src/model.rs                       Stored-row model
src/storage/mod.rs                 Embedded Lux-backed storage adapter
tests/mysql_compatibility_corpus.rs Differential MySQL query corpus
tests/mysql_parity.rs              Exact real-MySQL parity suite
tests/orm_compatibility.rs         ORM-shaped wire and migration coverage
```

## License

[MIT](LICENSE)
