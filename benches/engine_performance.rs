use std::hint::black_box;
use std::time::{Duration, Instant};

use my_sqweel::sql::engine::{Engine, EngineConfig};

const ROWS: usize = 4_000;
const ITERATIONS: usize = 20;

fn main() {
    let engine = Engine::new(EngineConfig::default());
    engine
        .execute_sql(
            "CREATE TABLE perf_rows (
                id BIGINT PRIMARY KEY,
                score DECIMAL(12, 3),
                label VARCHAR(32),
                group_id INT
            )",
        )
        .expect("create benchmark table");

    for chunk_start in (0..ROWS).step_by(250) {
        let values = (chunk_start..(chunk_start + 250).min(ROWS))
            .map(|id| {
                let score = ((ROWS - id) % 997) as f64 / 7.0;
                let label = format!("label_{:04}", id % 257);
                format!("({}, {:.3}, '{}', {})", id + 1, score, label, id % 31)
            })
            .collect::<Vec<_>>()
            .join(", ");
        engine
            .execute_sql(&format!("INSERT INTO perf_rows VALUES {values}"))
            .expect("insert benchmark rows");
    }

    let cases = [
        (
            "compound_order_limit",
            "SELECT id, score, label FROM perf_rows ORDER BY score DESC, label ASC, id LIMIT 100 OFFSET 100",
        ),
        (
            "filtered_order_limit",
            "SELECT id, score, label FROM perf_rows WHERE group_id = 7 ORDER BY label ASC, score DESC LIMIT 100",
        ),
        (
            "window_rank",
            "SELECT id, ROW_NUMBER() OVER (PARTITION BY group_id ORDER BY score DESC) AS row_number FROM perf_rows",
        ),
        (
            "function_projection",
            "SELECT CONCAT(label, '-', group_id), LOWER(label), LENGTH(label) FROM perf_rows",
        ),
        (
            "implicit_equi_join",
            "SELECT left_rows.id, right_rows.id FROM perf_rows AS left_rows, perf_rows AS right_rows WHERE left_rows.id = right_rows.id LIMIT 100",
        ),
        ("full_scan_projection", "SELECT id, label FROM perf_rows"),
        ("scalar_select", "SELECT 1 AS value"),
    ];

    for (name, sql) in cases {
        for _ in 0..3 {
            black_box(engine.execute_sql(sql).expect("benchmark warmup"));
        }

        let started = Instant::now();
        let mut rows = 0usize;
        for _ in 0..ITERATIONS {
            let result = engine.execute_sql(sql).expect("benchmark query");
            rows += result.first().map_or(0, |result| result.rows.len());
            black_box(result);
        }
        let elapsed = started.elapsed();
        print_result(name, elapsed, rows / ITERATIONS);
    }

    let prepared_sql =
        "SELECT id, label FROM perf_rows WHERE group_id = ? ORDER BY score DESC LIMIT 25";
    let prepared_params = [serde_json::Value::Number(serde_json::Number::from(7))];
    for _ in 0..3 {
        black_box(
            engine
                .execute_sql_with_params(prepared_sql, &prepared_params)
                .expect("prepared benchmark warmup"),
        );
    }
    let started = Instant::now();
    let mut rows = 0usize;
    for _ in 0..ITERATIONS {
        let result = engine
            .execute_sql_with_params(prepared_sql, &prepared_params)
            .expect("prepared benchmark query");
        rows += result.first().map_or(0, |result| result.rows.len());
        black_box(result);
    }
    print_result("prepared_select", started.elapsed(), rows / ITERATIONS);
}

fn print_result(name: &str, elapsed: Duration, rows: usize) {
    let queries_per_second = ITERATIONS as f64 / elapsed.as_secs_f64();
    println!(
        "{name}: {queries_per_second:.2} queries/s, {rows} rows/query, {:.2?} total",
        elapsed
    );
}
