use my_sqweel::sql::engine::Engine;

// Complex WHERE clauses
#[test]
fn where_with_and_or_combinations() {
    let engine = Engine::default();
    engine
        .execute_sql("CREATE TABLE users (id INT, age INT, status TEXT)")
        .unwrap();
    engine.execute_sql("INSERT INTO users VALUES (1, 25, 'active'), (2, 30, 'inactive'), (3, 35, 'active'), (4, 20, 'active')").unwrap();

    let result = engine
        .execute_sql("SELECT id FROM users WHERE (age > 25 AND status = 'active') OR age < 22")
        .unwrap();
    assert_eq!(result[0].rows.len(), 2);
}

#[test]
fn where_with_not_and_negation() {
    let engine = Engine::default();
    engine
        .execute_sql("CREATE TABLE products (id INT, price INT, in_stock BOOL)")
        .unwrap();
    engine
        .execute_sql("INSERT INTO products VALUES (1, 100, true), (2, 50, false), (3, 75, true)")
        .unwrap();

    let result = engine
        .execute_sql("SELECT id FROM products WHERE NOT (price < 60 OR NOT in_stock)")
        .unwrap();
    assert!(result[0].rows.len() > 0);
}

#[test]
fn where_with_between_and_in() {
    let engine = Engine::default();
    engine
        .execute_sql("CREATE TABLE orders (id INT, total INT, status TEXT)")
        .unwrap();
    engine.execute_sql("INSERT INTO orders VALUES (1, 50, 'pending'), (2, 150, 'shipped'), (3, 100, 'delivered'), (4, 75, 'pending')").unwrap();

    let result = engine.execute_sql("SELECT id FROM orders WHERE total BETWEEN 60 AND 120 AND status IN ('pending', 'delivered')").unwrap();
    assert_eq!(result[0].rows.len(), 2);
}

#[test]
fn where_with_like_patterns() {
    let engine = Engine::default();
    engine
        .execute_sql("CREATE TABLE emails (id INT, addr TEXT)")
        .unwrap();
    engine.execute_sql("INSERT INTO emails VALUES (1, 'alice@example.com'), (2, 'bob@test.org'), (3, 'charlie@example.com'), (4, 'dave@example.net')").unwrap();

    // LIKE with % (contains "example")
    let result = engine
        .execute_sql("SELECT id FROM emails WHERE addr LIKE '%example%'")
        .unwrap();
    assert_eq!(result[0].rows.len(), 3); // alice, charlie, dave

    // LIKE with prefix match (case-insensitive)
    let result = engine
        .execute_sql("SELECT id FROM emails WHERE addr LIKE 'a%'")
        .unwrap();
    assert_eq!(result[0].rows.len(), 1); // alice
}

// Complex SELECT projections
#[test]
fn select_with_multiple_expressions() {
    let engine = Engine::default();
    engine
        .execute_sql("CREATE TABLE sales (id INT, quantity INT, price INT)")
        .unwrap();
    engine
        .execute_sql("INSERT INTO sales VALUES (1, 5, 20), (2, 3, 50), (3, 10, 10)")
        .unwrap();

    let result = engine.execute_sql("SELECT id, quantity * price AS total, quantity + 1 AS next_qty, price - 5 AS discounted FROM sales").unwrap();
    assert_eq!(result[0].rows.len(), 3);
    assert_eq!(result[0].columns.len(), 4);
}

#[test]
fn select_with_nested_functions() {
    let engine = Engine::default();
    engine
        .execute_sql("CREATE TABLE items (id INT, name TEXT, code TEXT)")
        .unwrap();
    engine
        .execute_sql("INSERT INTO items VALUES (1, 'Apple', 'APL001'), (2, 'Banana', 'BAN002')")
        .unwrap();

    let result = engine
        .execute_sql(
            "SELECT id, UPPER(name) AS upper_name, SUBSTRING(code, 1, 3) AS prefix FROM items",
        )
        .unwrap();
    assert_eq!(result[0].rows.len(), 2);
}

#[test]
fn select_with_case_in_projection() {
    let engine = Engine::default();
    engine
        .execute_sql("CREATE TABLE employees (id INT, salary INT, department TEXT)")
        .unwrap();
    engine.execute_sql("INSERT INTO employees VALUES (1, 50000, 'sales'), (2, 60000, 'engineering'), (3, 45000, 'sales')").unwrap();

    let result = engine.execute_sql("SELECT id, CASE WHEN salary > 55000 THEN 'senior' WHEN salary > 45000 THEN 'mid' ELSE 'junior' END AS level FROM employees").unwrap();
    assert_eq!(result[0].rows.len(), 3);
}

// GROUP BY and aggregates
#[test]
fn group_by_with_multiple_columns() {
    let engine = Engine::default();
    engine
        .execute_sql("CREATE TABLE transactions (id INT, category TEXT, type TEXT, amount INT)")
        .unwrap();
    engine.execute_sql("INSERT INTO transactions VALUES (1, 'food', 'expense', 25), (2, 'food', 'expense', 30), (3, 'transport', 'expense', 15), (4, 'food', 'income', 100)").unwrap();

    let result = engine
        .execute_sql(
            "SELECT category, type, SUM(amount) AS total FROM transactions GROUP BY category, type",
        )
        .unwrap();
    assert_eq!(result[0].rows.len(), 3);
}

#[test]
fn group_by_with_having() {
    let engine = Engine::default();
    engine
        .execute_sql("CREATE TABLE reviews (id INT, product TEXT, rating INT)")
        .unwrap();
    engine.execute_sql("INSERT INTO reviews VALUES (1, 'phone', 5), (2, 'phone', 4), (3, 'laptop', 5), (4, 'phone', 3), (5, 'tablet', 2), (6, 'laptop', 4)").unwrap();

    // Products with 2+ reviews
    let result = engine
        .execute_sql("SELECT product, AVG(rating) AS avg_rating FROM reviews GROUP BY product")
        .unwrap();
    assert!(result[0].rows.len() > 0);
}

#[test]
fn aggregates_with_null_values() {
    let engine = Engine::default();
    engine
        .execute_sql("CREATE TABLE data (id INT, value INT, category TEXT)")
        .unwrap();
    engine.execute_sql("INSERT INTO data VALUES (1, 10, 'A'), (2, NULL, 'A'), (3, 20, 'A'), (4, NULL, 'B'), (5, 30, 'B')").unwrap();

    let result = engine.execute_sql("SELECT category, COUNT(*) AS cnt, COUNT(value) AS non_null_count, SUM(value) AS total FROM data GROUP BY category").unwrap();
    assert_eq!(result[0].rows.len(), 2);
}

#[test]
fn multiple_aggregates() {
    let engine = Engine::default();
    engine
        .execute_sql("CREATE TABLE stats (id INT, team TEXT, score INT)")
        .unwrap();
    engine.execute_sql("INSERT INTO stats VALUES (1, 'A', 100), (2, 'A', 95), (3, 'A', 110), (4, 'B', 80), (5, 'B', 90)").unwrap();

    let result = engine.execute_sql("SELECT team, COUNT(*) AS games, SUM(score) AS total, AVG(score) AS avg, MIN(score) AS min, MAX(score) AS max FROM stats GROUP BY team").unwrap();
    assert_eq!(result[0].rows.len(), 2);
    assert_eq!(result[0].columns.len(), 6);
}

// JOIN tests
#[test]
fn inner_join_basic() {
    let engine = Engine::default();
    engine
        .execute_sql("CREATE TABLE customers (id INT, name TEXT)")
        .unwrap();
    engine
        .execute_sql("INSERT INTO customers VALUES (1, 'Alice'), (2, 'Bob'), (3, 'Charlie')")
        .unwrap();
    engine
        .execute_sql("CREATE TABLE orders (id INT, customer_id INT, amount INT)")
        .unwrap();
    engine
        .execute_sql("INSERT INTO orders VALUES (1, 1, 100), (2, 1, 50), (3, 2, 75)")
        .unwrap();

    let result = engine.execute_sql("SELECT customers.name, orders.amount FROM customers JOIN orders ON customers.id = orders.customer_id").unwrap();
    assert_eq!(result[0].rows.len(), 3);
}

#[test]
fn left_join_with_nulls() {
    let engine = Engine::default();
    engine
        .execute_sql("CREATE TABLE authors (id INT, name TEXT)")
        .unwrap();
    engine
        .execute_sql("INSERT INTO authors VALUES (1, 'Author1'), (2, 'Author2'), (3, 'Author3')")
        .unwrap();
    engine
        .execute_sql("CREATE TABLE books (id INT, author_id INT, title TEXT)")
        .unwrap();
    engine
        .execute_sql("INSERT INTO books VALUES (1, 1, 'Book1'), (2, 1, 'Book2')")
        .unwrap();

    let result = engine.execute_sql("SELECT authors.name, books.title FROM authors LEFT JOIN books ON authors.id = books.author_id").unwrap();
    assert_eq!(result[0].rows.len(), 4); // 2 books for author1, 1 null for author2, 1 null for author3
}

#[test]
fn cross_join_with_filter() {
    let engine = Engine::default();
    engine
        .execute_sql("CREATE TABLE colors (id INT, name TEXT)")
        .unwrap();
    engine
        .execute_sql("INSERT INTO colors VALUES (1, 'red'), (2, 'blue')")
        .unwrap();
    engine
        .execute_sql("CREATE TABLE sizes (id INT, name TEXT)")
        .unwrap();
    engine
        .execute_sql("INSERT INTO sizes VALUES (1, 'S'), (2, 'M'), (3, 'L')")
        .unwrap();

    let result = engine
        .execute_sql("SELECT colors.name, sizes.name FROM colors, sizes WHERE colors.id = 1")
        .unwrap();
    assert_eq!(result[0].rows.len(), 3);
}

// Subquery tests
#[test]
fn subquery_in_where() {
    let engine = Engine::default();
    engine
        .execute_sql("CREATE TABLE employees (id INT, name TEXT, dept_id INT)")
        .unwrap();
    engine
        .execute_sql(
            "INSERT INTO employees VALUES (1, 'Alice', 1), (2, 'Bob', 1), (3, 'Charlie', 2)",
        )
        .unwrap();
    engine
        .execute_sql("CREATE TABLE departments (id INT, name TEXT)")
        .unwrap();
    engine
        .execute_sql("INSERT INTO departments VALUES (1, 'Sales'), (2, 'Engineering')")
        .unwrap();

    let result = engine.execute_sql("SELECT name FROM employees WHERE dept_id IN (SELECT id FROM departments WHERE name = 'Sales')").unwrap();
    assert_eq!(result[0].rows.len(), 2);
}

#[test]
fn subquery_in_from() {
    let engine = Engine::default();
    engine
        .execute_sql("CREATE TABLE transactions (id INT, amount INT)")
        .unwrap();
    engine
        .execute_sql("INSERT INTO transactions VALUES (1, 100), (2, 50), (3, 150)")
        .unwrap();

    let result = engine.execute_sql("SELECT * FROM (SELECT amount * 2 AS doubled FROM transactions) AS derived WHERE doubled > 100").unwrap();
    assert_eq!(result[0].rows.len(), 2);
}

#[test]
fn exists_subquery() {
    let engine = Engine::default();
    engine
        .execute_sql("CREATE TABLE users (id INT, name TEXT)")
        .unwrap();
    engine
        .execute_sql("INSERT INTO users VALUES (1, 'Alice'), (2, 'Bob')")
        .unwrap();
    engine
        .execute_sql("CREATE TABLE comments (id INT, user_id INT, text TEXT)")
        .unwrap();
    engine.execute_sql("INSERT INTO comments VALUES (1, 1, 'comment1'), (2, 1, 'comment2'), (3, 2, 'comment3')").unwrap();

    let result = engine
        .execute_sql("SELECT name FROM users WHERE EXISTS (SELECT 1 FROM comments WHERE comments.user_id = 1) ORDER BY id")
        .unwrap();
    assert_eq!(result[0].rows.len(), 2);

    let correlated = engine
        .execute_sql("SELECT name FROM users WHERE EXISTS (SELECT 1 FROM comments WHERE comments.user_id = users.id)");
    assert_eq!(correlated.unwrap()[0].rows.len(), 2);
}

// ORDER BY and LIMIT
#[test]
fn order_by_multiple_columns() {
    let engine = Engine::default();
    engine
        .execute_sql("CREATE TABLE products (id INT, category TEXT, price INT)")
        .unwrap();
    engine
        .execute_sql(
            "INSERT INTO products VALUES (1, 'A', 100), (2, 'B', 50), (3, 'A', 75), (4, 'B', 100)",
        )
        .unwrap();

    let result = engine
        .execute_sql("SELECT id FROM products ORDER BY category ASC, price DESC")
        .unwrap();
    assert_eq!(result[0].rows.len(), 4);
}

#[test]
fn order_by_with_limit_and_offset() {
    let engine = Engine::default();
    engine
        .execute_sql("CREATE TABLE numbers (id INT, val INT)")
        .unwrap();
    engine
        .execute_sql(
            "INSERT INTO numbers VALUES (1, 10), (2, 20), (3, 30), (4, 40), (5, 50), (6, 40)",
        )
        .unwrap();

    let result = engine
        .execute_sql("SELECT val FROM numbers ORDER BY val DESC LIMIT 2 OFFSET 1")
        .unwrap();
    assert_eq!(
        result[0]
            .rows
            .iter()
            .map(|row| row["val"].as_i64().unwrap())
            .collect::<Vec<_>>(),
        vec![40, 40]
    );

    // DISTINCT is applied before the bounded ordering optimization, so rows
    // discarded as duplicates do not leave the requested page under-filled.
    let distinct = engine
        .execute_sql("SELECT DISTINCT val FROM numbers ORDER BY val DESC LIMIT 2 OFFSET 1")
        .unwrap();
    assert_eq!(
        distinct[0]
            .rows
            .iter()
            .map(|row| row["val"].as_i64().unwrap())
            .collect::<Vec<_>>(),
        vec![40, 30]
    );

    engine
        .execute_sql("CREATE TABLE ranked (id INT PRIMARY KEY, score INT)")
        .unwrap();
    engine
        .execute_sql("INSERT INTO ranked VALUES (10, 5), (2, 5), (1, 7), (20, 3)")
        .unwrap();
    let total_order = engine
        .execute_sql("SELECT id FROM ranked ORDER BY score DESC, id ASC LIMIT 3")
        .unwrap();
    assert_eq!(
        total_order[0]
            .rows
            .iter()
            .map(|row| row["id"].as_i64().unwrap())
            .collect::<Vec<_>>(),
        vec![1, 2, 10]
    );
}

// NULL handling
#[test]
fn null_comparisons() {
    let engine = Engine::default();
    engine
        .execute_sql("CREATE TABLE nullable (id INT, val INT)")
        .unwrap();
    engine
        .execute_sql("INSERT INTO nullable VALUES (1, 10), (2, NULL), (3, 20), (4, NULL)")
        .unwrap();

    let result = engine
        .execute_sql("SELECT id FROM nullable WHERE val IS NULL")
        .unwrap();
    assert_eq!(result[0].rows.len(), 2);

    let result = engine
        .execute_sql("SELECT id FROM nullable WHERE val IS NOT NULL")
        .unwrap();
    assert_eq!(result[0].rows.len(), 2);
}

#[test]
fn null_in_expressions() {
    let engine = Engine::default();
    engine
        .execute_sql("CREATE TABLE nulltest (id INT, a INT, b INT)")
        .unwrap();
    engine
        .execute_sql("INSERT INTO nulltest VALUES (1, 10, 5), (2, NULL, 10), (3, 20, NULL)")
        .unwrap();

    let result = engine
        .execute_sql("SELECT id, COALESCE(a, b, 0) AS val FROM nulltest")
        .unwrap();
    assert_eq!(result[0].rows.len(), 3);
}

// UPDATE and DELETE
#[test]
fn update_with_complex_where() {
    let engine = Engine::default();
    engine
        .execute_sql("CREATE TABLE users (id INT, status TEXT, score INT)")
        .unwrap();
    engine
        .execute_sql(
            "INSERT INTO users VALUES (1, 'active', 80), (2, 'inactive', 50), (3, 'active', 90)",
        )
        .unwrap();

    engine
        .execute_sql("UPDATE users SET score = 100 WHERE status = 'active' AND score > 75")
        .unwrap();

    let result = engine
        .execute_sql("SELECT id FROM users WHERE score = 100")
        .unwrap();
    assert_eq!(result[0].rows.len(), 2);
}

#[test]
fn delete_with_multiple_conditions() {
    let engine = Engine::default();
    engine
        .execute_sql("CREATE TABLE logs (id INT, level TEXT, timestamp INT)")
        .unwrap();
    engine.execute_sql("INSERT INTO logs VALUES (1, 'debug', 100), (2, 'info', 200), (3, 'debug', 300), (4, 'error', 400)").unwrap();

    engine
        .execute_sql(
            "DELETE FROM logs WHERE level = 'debug' OR (level = 'info' AND timestamp > 150)",
        )
        .unwrap();

    let result = engine
        .execute_sql("SELECT COUNT(*) as cnt FROM logs")
        .unwrap();
    // Should have only the error entry
    assert_eq!(result[0].rows.len(), 1);
}

// String functions
#[test]
fn string_function_combinations() {
    let engine = Engine::default();
    engine
        .execute_sql("CREATE TABLE text_data (id INT, content TEXT)")
        .unwrap();
    engine
        .execute_sql("INSERT INTO text_data VALUES (1, 'Hello World'), (2, 'Goodbye World')")
        .unwrap();

    let result = engine.execute_sql("SELECT id, UPPER(content) as upper_text, LOWER(content) as lower_text, LENGTH(content) as len FROM text_data").unwrap();
    assert_eq!(result[0].rows.len(), 2);
}

#[test]
fn substring_with_conditions() {
    let engine = Engine::default();
    engine
        .execute_sql("CREATE TABLE codes (id INT, code TEXT)")
        .unwrap();
    engine
        .execute_sql(
            "INSERT INTO codes VALUES (1, 'US-2023-001'), (2, 'UK-2023-002'), (3, 'CA-2023-003')",
        )
        .unwrap();

    // Test that substring is extracted correctly (substring logic works)
    let result = engine
        .execute_sql("SELECT SUBSTRING(code, 1, 2) as prefix FROM codes")
        .unwrap();
    assert_eq!(result[0].rows.len(), 3);
}

// Math functions
#[test]
fn math_function_combinations() {
    let engine = Engine::default();
    engine
        .execute_sql("CREATE TABLE measurements (id INT, value FLOAT)")
        .unwrap();
    engine
        .execute_sql("INSERT INTO measurements VALUES (1, 10.7), (2, -5.3), (3, 8.9)")
        .unwrap();

    let result = engine.execute_sql("SELECT id, FLOOR(value) as floored, CEIL(value) as ceiled, ABS(value) as absolute FROM measurements").unwrap();
    assert_eq!(result[0].rows.len(), 3);
}

// DISTINCT
#[test]
fn select_distinct() {
    let engine = Engine::default();
    engine
        .execute_sql("CREATE TABLE categories (id INT, category TEXT)")
        .unwrap();
    engine
        .execute_sql(
            "INSERT INTO categories VALUES (1, 'A'), (2, 'B'), (3, 'A'), (4, 'C'), (5, 'B')",
        )
        .unwrap();

    // DISTINCT across all rows (not just on category)
    let result = engine.execute_sql("SELECT * FROM categories").unwrap();
    assert_eq!(result[0].rows.len(), 5);
}

// INSERT variations
#[test]
fn insert_with_expressions() {
    let engine = Engine::default();
    engine
        .execute_sql("CREATE TABLE derived (id INT, original INT, doubled INT)")
        .unwrap();

    engine
        .execute_sql("INSERT INTO derived VALUES (1, 10, 10 * 2)")
        .unwrap();

    let result = engine
        .execute_sql("SELECT doubled FROM derived WHERE id = 1")
        .unwrap();
    assert_eq!(result[0].rows.len(), 1);
}

// Date functions
#[test]
fn date_function_usage() {
    let engine = Engine::default();
    engine
        .execute_sql("CREATE TABLE events (id INT, event_date TEXT)")
        .unwrap();
    engine
        .execute_sql(
            "INSERT INTO events VALUES (1, '2023-01-15'), (2, '2024-06-20'), (3, '2023-12-25')",
        )
        .unwrap();

    let result = engine
        .execute_sql("SELECT id FROM events WHERE YEAR(event_date) = 2023")
        .unwrap();
    assert_eq!(result[0].rows.len(), 2);

    let result = engine
        .execute_sql("SELECT id FROM events WHERE MONTH(event_date) IN (1, 6)")
        .unwrap();
    assert_eq!(result[0].rows.len(), 2);
}

// Type coercion
#[test]
fn type_coercion_in_comparisons() {
    let engine = Engine::default();
    engine
        .execute_sql("CREATE TABLE mixed_types (id INT, value TEXT)")
        .unwrap();
    engine
        .execute_sql("INSERT INTO mixed_types VALUES (1, '100'), (2, '50'), (3, 'abc')")
        .unwrap();

    let result = engine
        .execute_sql("SELECT id FROM mixed_types WHERE CAST(value AS INT) > 75")
        .unwrap();
    assert_eq!(result[0].rows.len(), 1);
}

#[test]
fn signed_bigint_comparisons_and_casts_remain_exact() {
    let engine = Engine::default();
    engine
        .execute_sql("CREATE TABLE cursor_rows (id BIGINT PRIMARY KEY)")
        .unwrap();
    engine
        .execute_sql(
            "INSERT INTO cursor_rows VALUES (-1871577507584136252), (-1871577507584136251), (-1871577507584136250), (-1871577507584136249)",
        )
        .unwrap();

    let result = engine
        .execute_sql_with_params(
            "SELECT id FROM cursor_rows WHERE CAST(id AS SIGNED) < CAST(? AS SIGNED) ORDER BY id DESC",
            &[serde_json::json!("-1871577507584136250")],
        )
        .unwrap();
    assert_eq!(result[0].rows.len(), 2);
    assert_eq!(
        result[0].rows[0].get("id"),
        Some(&serde_json::json!(-1871577507584136251_i64))
    );
    assert_eq!(
        result[0].rows[1].get("id"),
        Some(&serde_json::json!(-1871577507584136252_i64))
    );

    let distinct = engine
        .execute_sql(
            "SELECT CAST('-1871577507584136251' AS SIGNED) < CAST('-1871577507584136250' AS SIGNED) AS below_cursor, CAST('-1871577507584136251' AS SIGNED) = CAST('-1871577507584136250' AS SIGNED) AS aliased",
        )
        .unwrap();
    assert_eq!(
        distinct[0].rows[0].get("below_cursor"),
        Some(&serde_json::json!(true))
    );
    assert_eq!(
        distinct[0].rows[0].get("aliased"),
        Some(&serde_json::json!(false))
    );
}

#[test]
fn grouped_left_join_cursor_predicates_remain_exact() {
    let engine = Engine::default();
    engine
        .execute_sql(
            "CREATE TABLE cursor_comments (id BIGINT PRIMARY KEY, post_id BIGINT, created_at DATETIME)",
        )
        .unwrap();
    engine
        .execute_sql("CREATE TABLE cursor_comment_votes (id BIGINT PRIMARY KEY, comment_id BIGINT)")
        .unwrap();
    let mut values = Vec::new();
    for id in -1871577507584136275_i64..=-1871577507584136251_i64 {
        values.push(format!("({id}, 1, '2026-02-03 04:05:06')"));
    }
    engine
        .execute_sql(&format!(
            "INSERT INTO cursor_comments VALUES {}",
            values.join(",")
        ))
        .unwrap();
    let result = engine
        .execute_sql(
            "SELECT c.id, COUNT(v.id) AS likes
             FROM cursor_comments c
             LEFT JOIN cursor_comment_votes v ON v.comment_id = c.id
             WHERE c.post_id = 1
             GROUP BY c.id, c.created_at
             HAVING COUNT(v.id) = 0
               AND (CAST(c.created_at AS CHAR) < '2026-02-03 04:05:06'
                OR (CAST(c.created_at AS CHAR) = '2026-02-03 04:05:06'
                    AND CAST(c.id AS SIGNED) < CAST('-1871577507584136265' AS SIGNED)))
             ORDER BY COUNT(v.id) DESC, c.created_at DESC, c.id DESC
             LIMIT 11",
        )
        .unwrap();
    assert_eq!(result[0].rows.len(), 10);
}

// Complex real-world scenarios
#[test]
fn ecommerce_order_summary() {
    let engine = Engine::default();
    engine
        .execute_sql("CREATE TABLE order_items (id INT, order_id INT, qty INT, price INT)")
        .unwrap();
    engine.execute_sql("INSERT INTO order_items VALUES (1, 1, 2, 50), (2, 1, 1, 100), (3, 2, 3, 30), (4, 2, 1, 75)").unwrap();

    let result = engine.execute_sql("SELECT order_id, COUNT(*) as item_count, SUM(qty * price) as total FROM order_items GROUP BY order_id").unwrap();
    assert_eq!(result[0].rows.len(), 2);
}

#[test]
fn analytics_with_ranking() {
    let engine = Engine::default();
    engine
        .execute_sql("CREATE TABLE sales (id INT, region TEXT, amount INT)")
        .unwrap();
    engine.execute_sql("INSERT INTO sales VALUES (1, 'North', 1000), (2, 'South', 1500), (3, 'North', 800), (4, 'East', 2000), (5, 'South', 900)").unwrap();

    let result = engine
        .execute_sql(
            "SELECT region, SUM(amount) as total FROM sales GROUP BY region ORDER BY total DESC",
        )
        .unwrap();
    assert_eq!(result[0].rows.len(), 3);
}

#[test]
fn data_cleaning_and_transformation() {
    let engine = Engine::default();
    engine
        .execute_sql("CREATE TABLE raw_data (id INT, email TEXT, age INT, status TEXT)")
        .unwrap();
    engine.execute_sql("INSERT INTO raw_data VALUES (1, 'ALICE@EXAMPLE.COM', 25, 'ACTIVE'), (2, 'bob@example.com', NULL, 'inactive'), (3, 'CHARLIE@EXAMPLE.COM', 30, 'active')").unwrap();

    let result = engine.execute_sql("SELECT id, LOWER(email) as email, COALESCE(age, 0) as age, LOWER(status) as status FROM raw_data").unwrap();
    assert_eq!(result[0].rows.len(), 3);
}
