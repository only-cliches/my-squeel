use std::collections::HashMap;
use std::io;
use std::net::TcpListener;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::Duration;

use msql_srv::{
    Column, ColumnFlags, ColumnType, ErrorKind, InitWriter, MysqlIntermediary, MysqlShim,
    ParamParser, ParamValue, QueryResultWriter, StatementMetaWriter, ValueInner,
};
use serde_json::{Map, Value};

use crate::sql::engine::{Engine, MysqlColumnType, QueryResult};

#[derive(Clone)]
pub struct WireServer {
    engine: Arc<Engine>,
}

impl WireServer {
    pub fn new(engine: Arc<Engine>) -> Self {
        Self { engine }
    }

    pub fn serve(&self, bind_addr: std::net::SocketAddr) -> io::Result<()> {
        let listener = TcpListener::bind(bind_addr)?;
        self.serve_listener(listener)
    }

    pub fn serve_listener(&self, listener: TcpListener) -> io::Result<()> {
        for stream in listener.incoming() {
            match stream {
                Ok(stream) => {
                    self.spawn_session(stream);
                }
                Err(err) => {
                    tracing::warn!(error = %err, "failed accepting mysql connection");
                }
            }
        }
        Ok(())
    }

    pub fn serve_listener_until(
        &self,
        listener: TcpListener,
        stop: Arc<AtomicBool>,
    ) -> io::Result<()> {
        listener.set_nonblocking(true)?;
        while !stop.load(Ordering::Relaxed) {
            match listener.accept() {
                Ok((stream, _)) => self.spawn_session(stream),
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(50));
                }
                Err(err) => {
                    tracing::warn!(error = %err, "failed accepting mysql connection");
                    std::thread::sleep(Duration::from_millis(50));
                }
            }
        }
        Ok(())
    }

    fn spawn_session(&self, stream: std::net::TcpStream) {
        let backend = Backend::new(self.engine.clone());
        std::thread::spawn(move || {
            if let Err(err) = stream.set_nonblocking(false) {
                tracing::warn!(error = %err, "failed setting mysql session stream to blocking mode");
                return;
            }
            if let Err(err) = MysqlIntermediary::run_on_tcp(backend, stream) {
                tracing::warn!(error = %err, "mysql session ended with error");
            }
        });
    }
}

struct Backend {
    engine: Arc<Engine>,
    next_stmt_id: AtomicU32,
    statements: HashMap<u32, PreparedStatement>,
    last_insert_id: u64,
    current_db: String,
    session_vars: HashMap<String, Value>,
}

struct PreparedStatement {
    sql: String,
    param_count: usize,
}

impl Backend {
    fn new(engine: Arc<Engine>) -> Self {
        Self {
            engine,
            next_stmt_id: AtomicU32::new(1),
            statements: HashMap::new(),
            last_insert_id: 0,
            current_db: "app".to_string(),
            session_vars: default_session_vars(),
        }
    }
}

impl<W: io::Read + io::Write> MysqlShim<W> for Backend {
    type Error = io::Error;

    fn on_prepare(&mut self, query: &str, info: StatementMetaWriter<'_, W>) -> io::Result<()> {
        let stmt_id = self.next_stmt_id.fetch_add(1, Ordering::Relaxed);
        let param_count = count_query_params(query);
        self.statements.insert(
            stmt_id,
            PreparedStatement {
                sql: query.to_string(),
                param_count,
            },
        );
        let params = parameter_columns(param_count);
        let columns = prepared_result_columns(&self.engine, query, param_count);
        info.reply(stmt_id, &params, &columns)
    }

    fn on_execute(
        &mut self,
        id: u32,
        params: ParamParser<'_>,
        results: QueryResultWriter<'_, W>,
    ) -> io::Result<()> {
        let Some(statement) = self.statements.get(&id) else {
            return results.completed(0, 0);
        };
        let statement_sql = statement.sql.clone();
        let params = params.into_iter().map(param_to_json).collect::<Vec<_>>();
        if params.len() != statement.param_count {
            tracing::debug!(
                expected = statement.param_count,
                actual = params.len(),
                "prepared parameter count mismatch"
            );
            return results.completed(0, 0);
        }
        let out = if is_last_insert_id_query(&statement_sql) {
            Ok(vec![last_insert_id_result(self.last_insert_id)])
        } else {
            self.engine.execute_sql_with_params(&statement_sql, &params)
        };
        write_query_items(out, results, &mut self.last_insert_id, Some(&statement_sql))
    }

    fn on_close(&mut self, stmt: u32) {
        self.statements.remove(&stmt);
    }

    fn on_init(&mut self, schema: &str, writer: InitWriter<'_, W>) -> io::Result<()> {
        if !schema.is_empty() {
            self.current_db = schema.to_string();
        }
        writer.ok()
    }

    fn on_query(&mut self, query: &str, results: QueryResultWriter<'_, W>) -> io::Result<()> {
        let out = if let Some(result) = self.execute_session_query(query) {
            Ok(vec![result])
        } else if is_last_insert_id_query(query) {
            Ok(vec![last_insert_id_result(self.last_insert_id)])
        } else {
            self.engine.execute_sql(query)
        };
        write_query_items(out, results, &mut self.last_insert_id, Some(query))
    }
}

impl Backend {
    fn execute_session_query(&mut self, query: &str) -> Option<QueryResult> {
        let trimmed = query.trim().trim_end_matches(';').trim();
        let upper = trimmed.to_ascii_uppercase();
        if upper.starts_with("USE ") {
            self.current_db = trimmed[4..].trim().trim_matches('`').to_string();
            return Some(QueryResult::default());
        }
        if upper.starts_with("SET ") {
            self.apply_set_statement(&trimmed[4..]);
            return Some(QueryResult::default());
        }
        if upper.starts_with("SELECT ") {
            return self.select_session_values(trimmed);
        }
        None
    }

    fn apply_set_statement(&mut self, assignments: &str) {
        for assignment in split_sql_args_wire(assignments) {
            let Some((name, value)) = assignment.split_once('=') else {
                continue;
            };
            let name = normalize_session_var_name(name);
            self.session_vars
                .insert(name, parse_session_value(value.trim()));
        }
    }

    fn select_session_values(&self, sql: &str) -> Option<QueryResult> {
        let Ok(statements) = crate::sql::parse(sql) else {
            return None;
        };
        let Some(sqlparser::ast::Statement::Query(query)) = statements.into_iter().next() else {
            return None;
        };
        let sqlparser::ast::SetExpr::Select(select) = *query.body else {
            return None;
        };
        if !select.from.is_empty() {
            return None;
        }

        let mut columns = Vec::new();
        let mut row = Map::new();
        for item in select.projection {
            let (column, value) = self.session_projection_value(&item)?;
            columns.push(column.clone());
            row.insert(column, value);
        }
        Some(QueryResult {
            rows_affected: 0,
            last_insert_id: 0,
            columns,
            column_metadata: vec![],
            rows: vec![row],
        })
    }

    fn session_projection_value(
        &self,
        item: &sqlparser::ast::SelectItem,
    ) -> Option<(String, Value)> {
        let (expr, alias) = match item {
            sqlparser::ast::SelectItem::UnnamedExpr(expr) => (expr, None),
            sqlparser::ast::SelectItem::ExprWithAlias { expr, alias } => {
                (expr, Some(alias.value.clone()))
            }
            _ => return None,
        };
        let expr_text = expr.to_string();
        let normalized = expr_text
            .chars()
            .filter(|ch| !ch.is_whitespace() && *ch != '`')
            .collect::<String>();
        let normalized_upper = normalized.to_ascii_uppercase();
        let value = if normalized_upper == "DATABASE()" || normalized_upper == "SCHEMA()" {
            Value::String(self.current_db.clone())
        } else if normalized.starts_with("@@") {
            let name = normalize_session_var_name(&normalized);
            self.session_vars
                .get(&name)
                .cloned()
                .unwrap_or_else(|| Value::String(String::new()))
        } else {
            return None;
        };
        Some((alias.unwrap_or(expr_text), value))
    }
}

fn write_query_items<W: io::Read + io::Write>(
    items: anyhow::Result<Vec<QueryResult>>,
    results: QueryResultWriter<'_, W>,
    session_last_insert_id: &mut u64,
    query: Option<&str>,
) -> io::Result<()> {
    match items {
        Ok(items) => {
            let out = items.into_iter().last().unwrap_or_default();
            if out.last_insert_id != 0 {
                *session_last_insert_id = out.last_insert_id;
            }
            write_result(results, out, query)
        }
        Err(err) => {
            tracing::debug!(error = %err, "query execution error");
            let message = err.to_string();
            results.error(mysql_error_kind(&message), message.as_bytes())
        }
    }
}

fn mysql_error_kind(message: &str) -> ErrorKind {
    let message = message.to_ascii_lowercase();
    if message.contains("already exists") {
        ErrorKind::ER_TABLE_EXISTS_ERROR
    } else if message.contains("unknown table") || message.contains("missing table") {
        ErrorKind::ER_NO_SUCH_TABLE
    } else if message.contains("unknown column") {
        ErrorKind::ER_BAD_FIELD_ERROR
    } else if message.contains("duplicate column") || message.contains("specified twice") {
        ErrorKind::ER_DUP_FIELDNAME
    } else if message.contains("primary key conflict")
        || message.contains("unique constraint violation")
        || message.contains("duplicate entry")
    {
        ErrorKind::ER_DUP_ENTRY
    } else if message.contains("cannot be null") {
        ErrorKind::ER_BAD_NULL_ERROR
    } else if message.contains("does not have a default") {
        ErrorKind::ER_NO_DEFAULT_FOR_FIELD
    } else if message.contains("column count doesn't match") {
        ErrorKind::ER_WRONG_VALUE_COUNT_ON_ROW
    } else if message.contains("referenced row") {
        ErrorKind::ER_ROW_IS_REFERENCED_2
    } else if message.contains("foreign key constraint fails") {
        ErrorKind::ER_NO_REFERENCED_ROW_2
    } else if message.contains("data too long") {
        ErrorKind::ER_DATA_TOO_LONG
    } else if message.contains("out of range") {
        ErrorKind::ER_WARN_DATA_OUT_OF_RANGE
    } else if message.contains("incorrect integer") || message.contains("incorrect decimal") {
        ErrorKind::ER_TRUNCATED_WRONG_VALUE_FOR_FIELD
    } else if message.contains("incorrect date")
        || message.contains("incorrect datetime")
        || message.contains("incorrect time")
    {
        ErrorKind::ER_TRUNCATED_WRONG_VALUE
    } else if message.contains("sql parser error") || message.contains("parse") {
        ErrorKind::ER_PARSE_ERROR
    } else {
        ErrorKind::ER_NOT_SUPPORTED_YET
    }
}

fn write_result<W: io::Read + io::Write>(
    results: QueryResultWriter<'_, W>,
    out: QueryResult,
    query: Option<&str>,
) -> io::Result<()> {
    let mut columns = out.columns;
    if columns.is_empty()
        && let Some(row) = out.rows.first()
    {
        columns = row.keys().cloned().collect();
    }

    if columns.is_empty() {
        return results.completed(out.rows_affected, out.last_insert_id);
    }

    let mut decimal_columns = query.map(mysql_decimal_columns).unwrap_or_default();
    for metadata in &out.column_metadata {
        if metadata.column_type == MysqlColumnType::Decimal {
            decimal_columns
                .entry(metadata.name.clone())
                .or_insert(metadata.decimals as usize);
        }
    }
    let defs: Vec<Column> = columns
        .iter()
        .enumerate()
        .map(|(index, name)| {
            let metadata = out.column_metadata.get(index);
            let mut colflags = ColumnFlags::empty();
            if metadata.is_some_and(|metadata| !metadata.nullable) {
                colflags.insert(ColumnFlags::NOT_NULL_FLAG);
            }
            if metadata.is_some_and(|metadata| metadata.unsigned) {
                colflags.insert(ColumnFlags::UNSIGNED_FLAG);
            }
            if metadata.is_some_and(|metadata| {
                matches!(
                    metadata.column_type,
                    MysqlColumnType::Binary
                        | MysqlColumnType::VarBinary
                        | MysqlColumnType::Blob
                        | MysqlColumnType::Bit
                )
            }) {
                colflags.insert(ColumnFlags::BINARY_FLAG);
            }
            Column {
                table: metadata
                    .map(|metadata| metadata.table.clone())
                    .unwrap_or_default(),
                column: name.clone(),
                coltype: metadata
                    .map(|metadata| wire_column_type(metadata.column_type))
                    .unwrap_or_else(|| {
                        if decimal_columns.contains_key(name) {
                            ColumnType::MYSQL_TYPE_NEWDECIMAL
                        } else {
                            column_type_for(&out.rows, name)
                        }
                    }),
                colflags,
            }
        })
        .collect();

    if let Err(error) = validate_wire_rows(&out.rows, &columns, &defs) {
        let message = error.to_string();
        return results.error(mysql_error_kind(&message), message.as_bytes());
    }

    let mut rw = results.start(&defs)?;
    for row in out.rows {
        write_row(&mut rw, &row, &columns, &defs, &decimal_columns)?;
        rw.end_row()?;
    }
    rw.finish()
}

fn wire_column_type(column_type: MysqlColumnType) -> ColumnType {
    match column_type {
        MysqlColumnType::Null => ColumnType::MYSQL_TYPE_NULL,
        MysqlColumnType::TinyInt => ColumnType::MYSQL_TYPE_TINY,
        MysqlColumnType::SmallInt => ColumnType::MYSQL_TYPE_SHORT,
        MysqlColumnType::Integer => ColumnType::MYSQL_TYPE_LONG,
        MysqlColumnType::BigInt => ColumnType::MYSQL_TYPE_LONGLONG,
        MysqlColumnType::Float => ColumnType::MYSQL_TYPE_FLOAT,
        MysqlColumnType::Double => ColumnType::MYSQL_TYPE_DOUBLE,
        MysqlColumnType::Decimal => ColumnType::MYSQL_TYPE_NEWDECIMAL,
        MysqlColumnType::Date => ColumnType::MYSQL_TYPE_DATE,
        MysqlColumnType::Time => ColumnType::MYSQL_TYPE_TIME,
        MysqlColumnType::DateTime => ColumnType::MYSQL_TYPE_DATETIME,
        MysqlColumnType::Timestamp => ColumnType::MYSQL_TYPE_TIMESTAMP,
        MysqlColumnType::Year => ColumnType::MYSQL_TYPE_YEAR,
        MysqlColumnType::Char | MysqlColumnType::Binary => ColumnType::MYSQL_TYPE_STRING,
        MysqlColumnType::VarChar | MysqlColumnType::VarBinary => ColumnType::MYSQL_TYPE_VAR_STRING,
        MysqlColumnType::Text | MysqlColumnType::Blob => ColumnType::MYSQL_TYPE_BLOB,
        MysqlColumnType::Json => ColumnType::MYSQL_TYPE_JSON,
        MysqlColumnType::Bit => ColumnType::MYSQL_TYPE_BIT,
    }
}

fn is_last_insert_id_query(query: &str) -> bool {
    let Ok(statements) = crate::sql::parse(query) else {
        return false;
    };
    let Some(sqlparser::ast::Statement::Query(query)) = statements.into_iter().next() else {
        return false;
    };
    let sqlparser::ast::SetExpr::Select(select) = *query.body else {
        return false;
    };
    if !select.from.is_empty() || select.projection.len() != 1 {
        return false;
    }

    let expr = match &select.projection[0] {
        sqlparser::ast::SelectItem::UnnamedExpr(expr) => expr,
        sqlparser::ast::SelectItem::ExprWithAlias { expr, .. } => expr,
        _ => return false,
    };
    expr.to_string()
        .chars()
        .filter(|ch| !ch.is_whitespace() && *ch != '`')
        .collect::<String>()
        .eq_ignore_ascii_case("LAST_INSERT_ID()")
}

fn last_insert_id_result(value: u64) -> QueryResult {
    let column = "LAST_INSERT_ID()".to_string();
    let mut row = Map::new();
    row.insert(column.clone(), serde_json::Number::from(value).into());
    QueryResult {
        rows_affected: 0,
        last_insert_id: 0,
        columns: vec![column],
        column_metadata: vec![],
        rows: vec![row],
    }
}

fn default_session_vars() -> HashMap<String, Value> {
    [
        ("autocommit", serde_json::json!(1)),
        ("sql_mode", serde_json::json!("")),
        ("time_zone", serde_json::json!("+00:00")),
        ("version", serde_json::json!("8.0.0-my-sqweel")),
        ("version_comment", serde_json::json!("MySqweel")),
        (
            "transaction_isolation",
            serde_json::json!("REPEATABLE-READ"),
        ),
        ("tx_isolation", serde_json::json!("REPEATABLE-READ")),
        ("character_set_client", serde_json::json!("utf8mb4")),
        ("character_set_connection", serde_json::json!("utf8mb4")),
        ("character_set_results", serde_json::json!("utf8mb4")),
        (
            "collation_connection",
            serde_json::json!("utf8mb4_general_ci"),
        ),
        ("max_allowed_packet", serde_json::json!(67108864)),
    ]
    .into_iter()
    .map(|(key, value)| (key.to_string(), value))
    .collect()
}

fn normalize_session_var_name(name: &str) -> String {
    name.trim()
        .trim_start_matches("@@")
        .trim_start_matches("SESSION.")
        .trim_start_matches("session.")
        .trim_start_matches("GLOBAL.")
        .trim_start_matches("global.")
        .trim_matches('`')
        .to_ascii_lowercase()
}

fn parse_session_value(value: &str) -> Value {
    let value = value.trim();
    if value.eq_ignore_ascii_case("NULL") {
        return Value::Null;
    }
    if value.eq_ignore_ascii_case("TRUE") {
        return Value::Bool(true);
    }
    if value.eq_ignore_ascii_case("FALSE") {
        return Value::Bool(false);
    }
    if let Ok(value) = value.parse::<i64>() {
        return Value::Number(value.into());
    }
    Value::String(
        value
            .trim_matches('\'')
            .trim_matches('"')
            .replace("''", "'"),
    )
}

fn split_sql_args_wire(args: &str) -> Vec<String> {
    if args.trim().is_empty() {
        return Vec::new();
    }

    let mut out = Vec::new();
    let mut current = String::new();
    let mut depth = 0_i32;
    let mut in_single = false;
    let mut in_double = false;
    let mut chars = args.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '\'' if !in_double => {
                current.push(ch);
                if in_single && chars.peek() == Some(&'\'') {
                    current.push(chars.next().expect("peeked quote"));
                } else {
                    in_single = !in_single;
                }
            }
            '"' if !in_single => {
                in_double = !in_double;
                current.push(ch);
            }
            '(' if !in_single && !in_double => {
                depth += 1;
                current.push(ch);
            }
            ')' if !in_single && !in_double => {
                depth -= 1;
                current.push(ch);
            }
            ',' if depth == 0 && !in_single && !in_double => {
                out.push(current.trim().to_string());
                current.clear();
            }
            _ => current.push(ch),
        }
    }
    if !current.trim().is_empty() {
        out.push(current.trim().to_string());
    }
    out
}

fn write_row<W: io::Read + io::Write>(
    rw: &mut msql_srv::RowWriter<'_, W>,
    row: &Map<String, Value>,
    columns: &[String],
    definitions: &[Column],
    decimal_columns: &HashMap<String, usize>,
) -> io::Result<()> {
    for (index, key) in columns.iter().enumerate() {
        let definition = &definitions[index];
        let value = row.get(key).cloned().unwrap_or(Value::Null);
        match value {
            Value::Null => rw.write_col(Option::<String>::None)?,
            Value::Number(number) if definition.coltype == ColumnType::MYSQL_TYPE_NEWDECIMAL => {
                let value = number.as_f64().unwrap_or_default();
                let scale = decimal_columns.get(key).copied().unwrap_or(0);
                rw.write_col(format!("{value:.scale$}"))?;
            }
            Value::String(value) if definition.coltype == ColumnType::MYSQL_TYPE_NEWDECIMAL => {
                let scale = decimal_columns.get(key).copied().unwrap_or(0);
                let number = value.parse::<f64>().unwrap_or_default();
                rw.write_col(format!("{number:.scale$}"))?;
            }
            Value::Bool(value) => {
                write_numeric_column(rw, i64::from(value), definition)?;
            }
            Value::Number(number) => {
                if is_integral_column(definition.coltype) {
                    if definition.colflags.contains(ColumnFlags::UNSIGNED_FLAG)
                        && let Some(value) = number.as_u64()
                    {
                        write_unsigned_numeric_column(rw, value, definition)?;
                    } else {
                        let value = number
                            .as_i64()
                            .unwrap_or_else(|| number.as_f64().unwrap_or_default() as i64);
                        write_numeric_column(rw, value, definition)?;
                    }
                } else if definition.coltype == ColumnType::MYSQL_TYPE_FLOAT {
                    rw.write_col(number.as_f64().unwrap_or_default() as f32)?;
                } else if definition.coltype == ColumnType::MYSQL_TYPE_DOUBLE {
                    rw.write_col(number.as_f64().unwrap_or_default())?;
                } else {
                    rw.write_col(number.to_string())?;
                }
            }
            Value::String(value) => write_string_column(rw, &value, definition)?,
            other => rw.write_col(other.to_string())?,
        }
    }
    Ok(())
}

fn write_unsigned_numeric_column<W: io::Read + io::Write>(
    rw: &mut msql_srv::RowWriter<'_, W>,
    value: u64,
    definition: &Column,
) -> io::Result<()> {
    match definition.coltype {
        ColumnType::MYSQL_TYPE_TINY => rw.write_col(value as u8),
        ColumnType::MYSQL_TYPE_SHORT | ColumnType::MYSQL_TYPE_YEAR => rw.write_col(value as u16),
        ColumnType::MYSQL_TYPE_LONG | ColumnType::MYSQL_TYPE_INT24 => rw.write_col(value as u32),
        ColumnType::MYSQL_TYPE_LONGLONG => rw.write_col(value),
        _ => rw.write_col(value.to_string()),
    }
}

fn is_integral_column(column_type: ColumnType) -> bool {
    matches!(
        column_type,
        ColumnType::MYSQL_TYPE_TINY
            | ColumnType::MYSQL_TYPE_SHORT
            | ColumnType::MYSQL_TYPE_LONG
            | ColumnType::MYSQL_TYPE_INT24
            | ColumnType::MYSQL_TYPE_LONGLONG
            | ColumnType::MYSQL_TYPE_YEAR
    )
}

fn write_numeric_column<W: io::Read + io::Write>(
    rw: &mut msql_srv::RowWriter<'_, W>,
    value: i64,
    definition: &Column,
) -> io::Result<()> {
    let unsigned = definition.colflags.contains(ColumnFlags::UNSIGNED_FLAG);
    match (definition.coltype, unsigned) {
        (ColumnType::MYSQL_TYPE_TINY, false) => rw.write_col(value as i8),
        (ColumnType::MYSQL_TYPE_TINY, true) => rw.write_col(value as u8),
        (ColumnType::MYSQL_TYPE_SHORT | ColumnType::MYSQL_TYPE_YEAR, false) => {
            rw.write_col(value as i16)
        }
        (ColumnType::MYSQL_TYPE_SHORT | ColumnType::MYSQL_TYPE_YEAR, true) => {
            rw.write_col(value as u16)
        }
        (ColumnType::MYSQL_TYPE_LONG | ColumnType::MYSQL_TYPE_INT24, false) => {
            rw.write_col(value as i32)
        }
        (ColumnType::MYSQL_TYPE_LONG | ColumnType::MYSQL_TYPE_INT24, true) => {
            rw.write_col(value as u32)
        }
        (ColumnType::MYSQL_TYPE_LONGLONG, false) => rw.write_col(value),
        (ColumnType::MYSQL_TYPE_LONGLONG, true) => rw.write_col(value as u64),
        _ => rw.write_col(value.to_string()),
    }
}

fn write_string_column<W: io::Read + io::Write>(
    rw: &mut msql_srv::RowWriter<'_, W>,
    value: &str,
    definition: &Column,
) -> io::Result<()> {
    match definition.coltype {
        ColumnType::MYSQL_TYPE_DATE => chrono::NaiveDate::parse_from_str(value, "%Y-%m-%d")
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
            .and_then(|value| rw.write_col(value)),
        ColumnType::MYSQL_TYPE_DATETIME | ColumnType::MYSQL_TYPE_TIMESTAMP => {
            parse_mysql_datetime_value(value)
                .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid datetime value"))
                .and_then(|value| rw.write_col(value))
        }
        ColumnType::MYSQL_TYPE_TIME => {
            parse_mysql_time_value(value).and_then(|value| rw.write_col(value))
        }
        column_type if is_integral_column(column_type) => value
            .parse::<i64>()
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
            .and_then(|value| write_numeric_column(rw, value, definition)),
        ColumnType::MYSQL_TYPE_FLOAT => value
            .parse::<f32>()
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
            .and_then(|value| rw.write_col(value)),
        ColumnType::MYSQL_TYPE_DOUBLE => value
            .parse::<f64>()
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
            .and_then(|value| rw.write_col(value)),
        _ => rw.write_col(value),
    }
}

fn parse_mysql_datetime_value(value: &str) -> Option<chrono::NaiveDateTime> {
    let decoded = serde_json::from_str::<String>(value).ok();
    let value = decoded.as_deref().unwrap_or(value);
    [
        "%Y-%m-%d %H:%M:%S%.f",
        "%Y-%m-%d %H:%M:%S",
        "%Y-%m-%dT%H:%M:%S%.f",
        "%Y-%m-%dT%H:%M:%S",
    ]
    .into_iter()
    .find_map(|format| chrono::NaiveDateTime::parse_from_str(value, format).ok())
    .or_else(|| {
        chrono::DateTime::parse_from_rfc3339(value)
            .ok()
            .map(|value| value.naive_utc())
    })
}

fn validate_wire_rows(
    rows: &[Map<String, Value>],
    columns: &[String],
    definitions: &[Column],
) -> io::Result<()> {
    for row in rows {
        for (index, key) in columns.iter().enumerate() {
            let definition = &definitions[index];
            let value = row.get(key).unwrap_or(&Value::Null);
            match value {
                Value::Null if definition.colflags.contains(ColumnFlags::NOT_NULL_FLAG) => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("column '{key}' cannot be null"),
                    ));
                }
                Value::String(value) => validate_wire_string(value, definition)?,
                _ => {}
            }
        }
    }
    Ok(())
}

fn validate_wire_string(value: &str, definition: &Column) -> io::Result<()> {
    let valid = match definition.coltype {
        ColumnType::MYSQL_TYPE_DATE => chrono::NaiveDate::parse_from_str(value, "%Y-%m-%d").is_ok(),
        ColumnType::MYSQL_TYPE_DATETIME | ColumnType::MYSQL_TYPE_TIMESTAMP => {
            parse_mysql_datetime_value(value).is_some()
        }
        ColumnType::MYSQL_TYPE_TIME => parse_mysql_time_value(value).is_ok(),
        column_type if is_integral_column(column_type) => value.parse::<i64>().is_ok(),
        ColumnType::MYSQL_TYPE_FLOAT | ColumnType::MYSQL_TYPE_DOUBLE => {
            value.parse::<f64>().is_ok()
        }
        _ => true,
    };
    if valid {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "incorrect datetime or numeric value in result row",
        ))
    }
}

#[derive(Debug)]
struct MysqlTimeValue {
    negative: bool,
    days: u32,
    hours: u8,
    minutes: u8,
    seconds: u8,
    micros: u32,
}

impl std::fmt::Display for MysqlTimeValue {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let sign = if self.negative { "-" } else { "" };
        let hours = u64::from(self.days) * 24 + u64::from(self.hours);
        if self.micros == 0 {
            write!(
                formatter,
                "{sign}{hours:02}:{:02}:{:02}",
                self.minutes, self.seconds
            )
        } else {
            write!(
                formatter,
                "{sign}{hours:02}:{:02}:{:02}.{:06}",
                self.minutes, self.seconds, self.micros
            )
        }
    }
}

impl msql_srv::ToMysqlValue for MysqlTimeValue {
    fn to_mysql_text<W: io::Write>(&self, writer: &mut W) -> io::Result<()> {
        msql_srv::ToMysqlValue::to_mysql_text(&self.to_string(), writer)
    }

    fn to_mysql_bin<W: io::Write>(&self, writer: &mut W, column: &Column) -> io::Result<()> {
        if column.coltype != ColumnType::MYSQL_TYPE_TIME {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "time value used with a non-TIME column",
            ));
        }
        if self.days == 0
            && self.hours == 0
            && self.minutes == 0
            && self.seconds == 0
            && self.micros == 0
        {
            return writer.write_all(&[0]);
        }

        writer.write_all(&[if self.micros == 0 { 8 } else { 12 }])?;
        writer.write_all(&[u8::from(self.negative)])?;
        writer.write_all(&self.days.to_le_bytes())?;
        writer.write_all(&[self.hours, self.minutes, self.seconds])?;
        if self.micros != 0 {
            writer.write_all(&self.micros.to_le_bytes())?;
        }
        Ok(())
    }
}

fn parse_mysql_time_value(value: &str) -> io::Result<MysqlTimeValue> {
    let (negative, value) = value
        .strip_prefix('-')
        .map(|value| (true, value))
        .or_else(|| value.strip_prefix('+').map(|value| (false, value)))
        .unwrap_or((false, value));
    let mut parts = value.split(':');
    let hours = parts.next().and_then(|value| value.parse::<u64>().ok());
    let minutes = parts.next().and_then(|value| value.parse::<u8>().ok());
    let seconds = parts.next();
    if parts.next().is_some() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid TIME value",
        ));
    }
    let (seconds, fraction) = seconds
        .map(|value| value.split_once('.').unwrap_or((value, "")))
        .unwrap_or(("", ""));
    let seconds = seconds.parse::<u8>().ok();
    let micros = if fraction.is_empty() {
        Some(0)
    } else if fraction.len() <= 6 && fraction.bytes().all(|byte| byte.is_ascii_digit()) {
        format!("{fraction:0<6}").parse::<u32>().ok()
    } else {
        None
    };
    let (Some(hours), Some(minutes), Some(seconds), Some(micros)) =
        (hours, minutes, seconds, micros)
    else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid TIME value",
        ));
    };
    if hours > 838 || minutes > 59 || seconds > 59 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid TIME value",
        ));
    }
    Ok(MysqlTimeValue {
        negative,
        days: (hours / 24) as u32,
        hours: (hours % 24) as u8,
        minutes,
        seconds,
        micros,
    })
}

fn column_type_for(rows: &[Map<String, Value>], column: &str) -> ColumnType {
    rows.iter()
        .filter_map(|row| row.get(column))
        .find_map(|value| match value {
            Value::Bool(_) => Some(ColumnType::MYSQL_TYPE_TINY),
            Value::Number(number) if number.is_i64() || number.is_u64() => {
                Some(ColumnType::MYSQL_TYPE_LONGLONG)
            }
            Value::Number(_) => Some(ColumnType::MYSQL_TYPE_DOUBLE),
            Value::Null => None,
            _ => Some(ColumnType::MYSQL_TYPE_STRING),
        })
        .unwrap_or(ColumnType::MYSQL_TYPE_STRING)
}

fn parameter_columns(count: usize) -> Vec<Column> {
    (0..count)
        .map(|idx| Column {
            table: "".to_string(),
            column: format!("param{}", idx + 1),
            coltype: ColumnType::MYSQL_TYPE_STRING,
            colflags: ColumnFlags::empty(),
        })
        .collect()
}

fn prepared_result_columns(engine: &Engine, query: &str, param_count: usize) -> Vec<Column> {
    let decimal_columns = mysql_decimal_columns(query);
    let Ok(statements) = crate::sql::parse(query) else {
        return Vec::new();
    };

    let Some(sqlparser::ast::Statement::Query(parsed_query)) = statements.into_iter().next() else {
        return Vec::new();
    };

    // Zero is accepted by LIMIT/OFFSET placeholders and generally produces an
    // empty SELECT while still allowing the engine to derive schema metadata.
    if let Ok(mut results) = engine.execute_sql_with_params(
        query,
        &vec![Value::Number(serde_json::Number::from(0)); param_count],
    ) && let Some(result) = results.pop()
        && !result.column_metadata.is_empty()
    {
        return result
            .columns
            .iter()
            .enumerate()
            .map(|(index, name)| {
                let metadata = result.column_metadata.get(index);
                let mut flags = ColumnFlags::empty();
                if metadata.is_some_and(|metadata| !metadata.nullable) {
                    flags.insert(ColumnFlags::NOT_NULL_FLAG);
                }
                if metadata.is_some_and(|metadata| metadata.unsigned) {
                    flags.insert(ColumnFlags::UNSIGNED_FLAG);
                }
                Column {
                    table: metadata
                        .map(|metadata| metadata.table.clone())
                        .unwrap_or_default(),
                    column: name.clone(),
                    coltype: metadata
                        .map(|metadata| wire_column_type(metadata.column_type))
                        .unwrap_or(ColumnType::MYSQL_TYPE_VAR_STRING),
                    colflags: flags,
                }
            })
            .collect();
    }

    let sqlparser::ast::SetExpr::Select(select) = *parsed_query.body else {
        return Vec::new();
    };

    select
        .projection
        .iter()
        .filter_map(|item| {
            let column = match item {
                sqlparser::ast::SelectItem::UnnamedExpr(expr) => match expr {
                    sqlparser::ast::Expr::Identifier(ident) => ident.value.clone(),
                    sqlparser::ast::Expr::CompoundIdentifier(parts) => parts
                        .iter()
                        .map(|part| part.value.clone())
                        .collect::<Vec<_>>()
                        .join("."),
                    other => other.to_string(),
                },
                sqlparser::ast::SelectItem::ExprWithAlias { alias, .. } => alias.value.clone(),
                _ => return None,
            };

            Some(Column {
                table: "".to_string(),
                coltype: if decimal_columns.contains_key(&column) {
                    ColumnType::MYSQL_TYPE_NEWDECIMAL
                } else {
                    ColumnType::MYSQL_TYPE_STRING
                },
                column,
                colflags: ColumnFlags::empty(),
            })
        })
        .collect()
}

fn mysql_decimal_columns(query: &str) -> HashMap<String, usize> {
    let Ok(statements) = crate::sql::parse(query) else {
        return HashMap::new();
    };
    let Some(sqlparser::ast::Statement::Query(query)) = statements.into_iter().next() else {
        return HashMap::new();
    };
    let sqlparser::ast::SetExpr::Select(select) = *query.body else {
        return HashMap::new();
    };

    select
        .projection
        .iter()
        .filter_map(|item| {
            let (expr, column) = match item {
                sqlparser::ast::SelectItem::UnnamedExpr(expr) => (expr, expr.to_string()),
                sqlparser::ast::SelectItem::ExprWithAlias { expr, alias } => {
                    (expr, alias.value.clone())
                }
                _ => return None,
            };
            mysql_decimal_scale(expr).map(|scale| (column, scale))
        })
        .collect()
}

fn mysql_decimal_scale(expr: &sqlparser::ast::Expr) -> Option<usize> {
    use sqlparser::ast::{Expr, FunctionArg, FunctionArgExpr, FunctionArguments, Value};

    let Expr::Function(function) = expr else {
        return None;
    };
    let name = function.name.0.last()?.value.to_ascii_uppercase();
    if name == "AVG" {
        return Some(4);
    }
    if !matches!(name.as_str(), "ROUND" | "TRUNCATE") {
        return None;
    }
    let FunctionArguments::List(arguments) = &function.args else {
        return None;
    };
    let Some(argument) = arguments.args.get(1) else {
        return Some(0);
    };
    let argument = match argument {
        FunctionArg::Named { arg, .. }
        | FunctionArg::ExprNamed { arg, .. }
        | FunctionArg::Unnamed(arg) => arg,
    };
    let FunctionArgExpr::Expr(Expr::Value(Value::Number(scale, _))) = argument else {
        return None;
    };
    Some(scale.parse::<i32>().ok()?.max(0) as usize)
}

fn count_query_params(query: &str) -> usize {
    let mut count = 0;
    let mut in_single = false;
    let mut in_double = false;
    let mut chars = query.chars().peekable();

    while let Some(ch) = chars.next() {
        match ch {
            '\'' if !in_double => in_single = !in_single,
            '"' if !in_single => in_double = !in_double,
            '\\' if in_single || in_double => {
                let _ = chars.next();
            }
            '?' if !in_single && !in_double => count += 1,
            _ => {}
        }
    }

    count
}

fn param_to_json(param: ParamValue<'_>) -> Value {
    match param.value.into_inner() {
        ValueInner::NULL => Value::Null,
        ValueInner::Bytes(bytes) => Value::String(String::from_utf8_lossy(bytes).to_string()),
        ValueInner::Int(value) => Value::Number(value.into()),
        ValueInner::UInt(value) => serde_json::Number::from(value).into(),
        ValueInner::Double(value) => serde_json::Number::from_f64(value)
            .map(Value::Number)
            .unwrap_or(Value::Null),
        ValueInner::Date(bytes) => Value::String(
            decode_mysql_date_parameter(bytes)
                .unwrap_or_else(|| String::from_utf8_lossy(bytes).to_string()),
        ),
        ValueInner::Time(bytes) => Value::String(
            decode_mysql_time_parameter(bytes)
                .unwrap_or_else(|| String::from_utf8_lossy(bytes).to_string()),
        ),
        ValueInner::Datetime(bytes) => Value::String(
            decode_mysql_datetime_parameter(bytes)
                .unwrap_or_else(|| String::from_utf8_lossy(bytes).to_string()),
        ),
    }
}

fn decode_mysql_date_parameter(bytes: &[u8]) -> Option<String> {
    if bytes.is_empty() {
        return Some("0000-00-00".to_string());
    }
    if bytes.len() != 4 {
        return None;
    }
    let year = u16::from_le_bytes([bytes[0], bytes[1]]);
    Some(format!("{year:04}-{:02}-{:02}", bytes[2], bytes[3]))
}

fn decode_mysql_datetime_parameter(bytes: &[u8]) -> Option<String> {
    if bytes.is_empty() {
        return Some("0000-00-00 00:00:00".to_string());
    }
    if !matches!(bytes.len(), 4 | 7 | 11) {
        return None;
    }
    let date = decode_mysql_date_parameter(&bytes[..4])?;
    if bytes.len() == 4 {
        return Some(format!("{date} 00:00:00"));
    }
    let base = format!("{date} {:02}:{:02}:{:02}", bytes[4], bytes[5], bytes[6]);
    if bytes.len() == 11 {
        let micros = u32::from_le_bytes([bytes[7], bytes[8], bytes[9], bytes[10]]);
        Some(format!("{base}.{micros:06}"))
    } else {
        Some(base)
    }
}

fn decode_mysql_time_parameter(bytes: &[u8]) -> Option<String> {
    if bytes.is_empty() {
        return Some("00:00:00".to_string());
    }
    if !matches!(bytes.len(), 8 | 12) {
        return None;
    }
    let negative = bytes[0] != 0;
    let days = u32::from_le_bytes([bytes[1], bytes[2], bytes[3], bytes[4]]);
    let hours = u64::from(days) * 24 + u64::from(bytes[5]);
    let sign = if negative { "-" } else { "" };
    let base = format!("{sign}{hours:02}:{:02}:{:02}", bytes[6], bytes[7]);
    if bytes.len() == 12 {
        let micros = u32::from_le_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]);
        Some(format!("{base}.{micros:06}"))
    } else {
        Some(base)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use msql_srv::{Column, ColumnFlags, ColumnType};
    use serde_json::{Map, json};

    use super::{Backend, parse_mysql_datetime_value, validate_wire_rows};
    use crate::sql::engine::Engine;

    #[test]
    fn session_select_shortcut_does_not_capture_table_queries() {
        let backend = Backend::new(Arc::new(Engine::default()));

        let session_only = backend
            .select_session_values("SELECT DATABASE() AS db")
            .expect("session-only select should be handled");
        assert_eq!(
            session_only.rows[0]
                .get("db")
                .and_then(|value| value.as_str()),
            Some("app")
        );

        assert!(
            backend
                .select_session_values("SELECT DATABASE() AS db, email FROM users")
                .is_none()
        );
    }

    #[test]
    fn datetime_wire_parser_accepts_mysql_and_rfc3339_values() {
        assert!(parse_mysql_datetime_value("2026-07-15 12:34:56.123456").is_some());
        assert!(parse_mysql_datetime_value("2026-07-15T12:34:56.123Z").is_some());
        assert!(parse_mysql_datetime_value("\"2026-07-15T12:34:56.123Z\"").is_some());
    }

    #[test]
    fn invalid_result_values_are_rejected_before_starting_a_row_writer() {
        let columns = vec!["createdAt".to_string()];
        let definitions = vec![Column {
            table: "events".to_string(),
            column: "createdAt".to_string(),
            coltype: ColumnType::MYSQL_TYPE_TIMESTAMP,
            colflags: ColumnFlags::NOT_NULL_FLAG,
        }];
        let mut row = Map::new();
        row.insert("createdAt".to_string(), json!("not-a-date"));

        let error = validate_wire_rows(&[row], &columns, &definitions).unwrap_err();
        assert!(error.to_string().contains("incorrect datetime"));
    }
}
