use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration as StdDuration, Instant};

use anyhow::{Result, anyhow};
use chrono::{
    DateTime, Datelike, Duration, Months, NaiveDate, NaiveDateTime, NaiveTime, Timelike, Utc,
};
use dashmap::DashMap;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Number, Value, json};
use sqlparser::ast::{
    Assignment, BinaryOperator, DateTimeField, Expr, FunctionArg, FunctionArgExpr,
    FunctionArgumentClause, FunctionArguments, GroupByExpr, Ident, JoinConstraint, JoinOperator,
    ObjectName, Offset, OnInsert, OrderByExpr, Query, Select, SelectItem, SetExpr, Statement,
    TableConstraint, TableFactor, TableWithJoins, Value as SqlValue,
};

use crate::model::StoredRow;
use crate::schema::{ColumnHint, ForeignKeyHint, IndexHint, TableSchemaHint};
use crate::storage::{LuxRedisStore, RedisStore};

mod compat;
mod ddl;
mod dml;
mod eval;
mod maintenance;
mod query;
mod storage_format;
mod support;
mod values;

use compat::*;
use ddl::*;
use eval::*;
use storage_format::*;
use support::*;
use values::*;

const STORAGE_NAMESPACE: &str = "my-sqweel";
const STORAGE_NAMESPACE_PATTERN: &str = "my-sqweel:*";
const STORAGE_AUTO_INC_KEY: &str = "my-sqweel:auto_inc";
const UNIQUE_SEPARATOR: char = '\u{1f}';
const FK_FIELD_SEPARATOR: char = '\u{1e}';
pub(crate) const JSON_NULL_SENTINEL: &str = "\0my_sqweel_json_null";

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UniqueMode {
    #[default]
    Overwrite,
    Enforce,
}

/// Controls whether MySqweel favors schema-drift convenience or MySQL's
/// fail-fast behavior. The default remains drift tolerant for backwards
/// compatibility; callers that need MySQL parity should select `MysqlStrict`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompatibilityProfile {
    #[default]
    Drift,
    MysqlStrict,
}

impl CompatibilityProfile {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Drift => "drift",
            Self::MysqlStrict => "mysql_strict",
        }
    }
}

impl UniqueMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Overwrite => "overwrite",
            Self::Enforce => "enforce",
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SeedMode {
    #[default]
    Append,
    Replace,
}

impl SeedMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Append => "append",
            Self::Replace => "replace",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EngineConfig {
    pub unique_mode: UniqueMode,
    #[serde(default)]
    pub compatibility_profile: CompatibilityProfile,
    pub failure_injection: FailureInjectionConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct FailureInjectionConfig {
    pub query_delay_ms: u64,
    pub fail_read_every: u64,
    pub fail_write_every: u64,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            unique_mode: UniqueMode::Overwrite,
            compatibility_profile: CompatibilityProfile::Drift,
            failure_injection: FailureInjectionConfig::default(),
        }
    }
}

impl EngineConfig {
    /// A parity-oriented configuration that rejects schema drift and enforces
    /// declared uniqueness in the same places MySQL does.
    pub fn mysql_strict() -> Self {
        Self {
            unique_mode: UniqueMode::Enforce,
            compatibility_profile: CompatibilityProfile::MysqlStrict,
            ..Self::default()
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct QueryResult {
    pub rows_affected: u64,
    pub last_insert_id: u64,
    pub columns: Vec<String>,
    pub column_metadata: Vec<ColumnMetadata>,
    pub rows: Vec<Map<String, Value>>,
}

/// Options controlling the amount of data copied into query completion events.
/// Result payloads are disabled by default because a query can return a large
/// number of rows.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct QueryEventOptions {
    pub include_results: bool,
}

pub type QueryId = u64;

impl QueryEventOptions {
    pub const fn metadata_only() -> Self {
        Self {
            include_results: false,
        }
    }

    pub const fn with_results() -> Self {
        Self {
            include_results: true,
        }
    }
}

#[derive(Debug, Clone)]
pub struct QueryReceivedEvent {
    pub query_id: QueryId,
    pub query: String,
}

#[derive(Debug, Clone)]
pub struct QueryCompletedEvent {
    pub query_id: QueryId,
    pub duration: StdDuration,
    pub result_set_count: usize,
    /// Total number of rows across all result sets.
    pub result_set_size: usize,
    pub results: Option<Vec<QueryResult>>,
    pub error: Option<String>,
}

#[derive(Debug, Clone)]
pub enum QueryEvent {
    Received(QueryReceivedEvent),
    Completed(QueryCompletedEvent),
}

/// A blocking stream of query lifecycle events.
pub struct QueryEventStream {
    receiver: Receiver<QueryEvent>,
}

impl QueryEventStream {
    pub fn recv(&self) -> Result<QueryEvent, mpsc::RecvError> {
        self.receiver.recv()
    }

    pub fn try_recv(&self) -> Result<QueryEvent, mpsc::TryRecvError> {
        self.receiver.try_recv()
    }

    pub fn recv_timeout(&self, timeout: StdDuration) -> Result<QueryEvent, mpsc::RecvTimeoutError> {
        self.receiver.recv_timeout(timeout)
    }
}

impl Iterator for QueryEventStream {
    type Item = QueryEvent;

    fn next(&mut self) -> Option<Self::Item> {
        self.receiver.recv().ok()
    }
}

struct QueryEventSubscriber {
    sender: Sender<QueryEvent>,
    include_results: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MysqlColumnType {
    Null,
    TinyInt,
    SmallInt,
    Integer,
    BigInt,
    Float,
    Double,
    Decimal,
    Date,
    Time,
    DateTime,
    Timestamp,
    Year,
    Char,
    #[default]
    VarChar,
    Text,
    Binary,
    VarBinary,
    Blob,
    Json,
    Bit,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ColumnMetadata {
    pub name: String,
    #[serde(default)]
    pub table: String,
    #[serde(default)]
    pub column_type: MysqlColumnType,
    #[serde(default = "default_true")]
    pub nullable: bool,
    #[serde(default)]
    pub unsigned: bool,
    #[serde(default)]
    pub decimals: u8,
    pub character_set: Option<String>,
    pub collation: Option<String>,
}

fn default_true() -> bool {
    true
}

impl ColumnMetadata {
    pub fn from_declared(
        name: impl Into<String>,
        table: impl Into<String>,
        hint: &ColumnHint,
    ) -> Self {
        let declared = hint.sql_type.as_deref().unwrap_or("VARCHAR");
        let upper = declared.to_ascii_uppercase();
        Self {
            name: name.into(),
            table: table.into(),
            column_type: mysql_column_type_from_declared(&upper),
            nullable: hint.nullable.unwrap_or(true) && !hint.primary_key,
            unsigned: upper.contains("UNSIGNED"),
            decimals: declared_type_scale(&upper),
            character_set: is_character_type(&upper).then(|| "utf8mb4".to_string()),
            collation: is_character_type(&upper).then(|| "utf8mb4_general_ci".to_string()),
        }
    }

    pub fn from_value(name: impl Into<String>, value: Option<&Value>) -> Self {
        let name = name.into();
        let column_type = match value {
            None | Some(Value::Null) => MysqlColumnType::VarChar,
            Some(Value::Bool(_)) => MysqlColumnType::TinyInt,
            Some(Value::Number(number)) if number.is_i64() || number.is_u64() => {
                MysqlColumnType::BigInt
            }
            Some(Value::Number(_)) => MysqlColumnType::Double,
            Some(Value::Array(_) | Value::Object(_)) => MysqlColumnType::Json,
            Some(Value::String(_)) => MysqlColumnType::VarChar,
        };
        Self {
            name,
            column_type,
            nullable: true,
            ..Self::default()
        }
    }
}

fn is_character_type(upper: &str) -> bool {
    ["CHAR", "TEXT", "ENUM", "SET"]
        .iter()
        .any(|kind| upper.contains(kind))
}

fn declared_type_scale(upper: &str) -> u8 {
    if !(upper.starts_with("DECIMAL") || upper.starts_with("NUMERIC")) {
        return 0;
    }
    upper
        .split_once('(')
        .and_then(|(_, tail)| tail.split_once(',').map(|(_, scale)| scale))
        .and_then(|scale| scale.trim_end_matches(')').trim().parse().ok())
        .unwrap_or(0)
}

fn mysql_column_type_from_declared(upper: &str) -> MysqlColumnType {
    if upper.starts_with("TINYINT") || upper.starts_with("BOOL") {
        MysqlColumnType::TinyInt
    } else if upper.starts_with("SMALLINT") {
        MysqlColumnType::SmallInt
    } else if upper.starts_with("MEDIUMINT") || upper.starts_with("INT") {
        MysqlColumnType::Integer
    } else if upper.starts_with("BIGINT") {
        MysqlColumnType::BigInt
    } else if upper.starts_with("FLOAT") {
        MysqlColumnType::Float
    } else if upper.starts_with("DOUBLE") || upper.starts_with("REAL") {
        MysqlColumnType::Double
    } else if upper.starts_with("DECIMAL") || upper.starts_with("NUMERIC") {
        MysqlColumnType::Decimal
    } else if upper.starts_with("TIMESTAMP") {
        MysqlColumnType::Timestamp
    } else if upper.starts_with("DATETIME") {
        MysqlColumnType::DateTime
    } else if upper.starts_with("DATE") {
        MysqlColumnType::Date
    } else if upper.starts_with("TIME") {
        MysqlColumnType::Time
    } else if upper.starts_with("YEAR") {
        MysqlColumnType::Year
    } else if upper.starts_with("VARBINARY") {
        MysqlColumnType::VarBinary
    } else if upper.starts_with("BINARY") {
        MysqlColumnType::Binary
    } else if upper.starts_with("BIT") {
        MysqlColumnType::Bit
    } else if upper.contains("BLOB") {
        MysqlColumnType::Blob
    } else if upper.starts_with("JSON") {
        MysqlColumnType::Json
    } else if upper.contains("TEXT") {
        MysqlColumnType::Text
    } else if upper.starts_with("CHAR") {
        MysqlColumnType::Char
    } else {
        MysqlColumnType::VarChar
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Snapshot {
    pub version: u32,
    pub created_at: String,
    pub schemas: BTreeMap<String, TableSchemaHint>,
    pub rows: BTreeMap<String, BTreeMap<String, StoredRow>>,
    pub auto_inc: BTreeMap<String, i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SeedReport {
    pub table: String,
    pub mode: SeedMode,
    pub rows_seeded: u64,
    pub rows_affected: u64,
    pub last_insert_id: u64,
}

struct InsertRowsOptions<'a> {
    ignore: bool,
    replace: bool,
    on_duplicate: &'a [Assignment],
    returning: Option<&'a [SelectItem]>,
}

pub struct Engine {
    cfg: EngineConfig,
    storage: Arc<dyn RedisStore>,
    schemas: DashMap<String, TableSchemaHint>,
    rows: DashMap<String, BTreeMap<String, StoredRow>>,
    auto_inc: DashMap<String, i64>,
    indexes: DashMap<String, BTreeMap<String, BTreeMap<String, BTreeSet<String>>>>,
    last_insert_id: AtomicU64,
    next_query_id: AtomicU64,
    read_query_count: AtomicU64,
    write_query_count: AtomicU64,
    query_event_subscribers: Mutex<Vec<QueryEventSubscriber>>,
}

impl Default for Engine {
    fn default() -> Self {
        Self::new(EngineConfig::default())
    }
}

impl Engine {
    pub fn new(cfg: EngineConfig) -> Self {
        Self::open_with_data_dir(cfg, None).expect("failed to start embedded Lux storage")
    }

    pub fn open_with_data_dir(cfg: EngineConfig, data_dir: Option<&str>) -> Result<Self> {
        let storage = Arc::new(LuxRedisStore::open(data_dir)?);
        Self::with_storage(cfg, storage)
    }

    fn with_storage(cfg: EngineConfig, storage: Arc<dyn RedisStore>) -> Result<Self> {
        let engine = Self {
            cfg,
            storage,
            schemas: DashMap::default(),
            rows: DashMap::default(),
            auto_inc: DashMap::default(),
            indexes: DashMap::default(),
            last_insert_id: AtomicU64::new(0),
            next_query_id: AtomicU64::new(1),
            read_query_count: AtomicU64::new(0),
            write_query_count: AtomicU64::new(0),
            query_event_subscribers: Mutex::new(Vec::new()),
        };
        engine.load_from_storage()?;
        Ok(engine)
    }

    pub fn compatibility_profile(&self) -> CompatibilityProfile {
        self.cfg.compatibility_profile
    }

    /// Subscribe to query lifecycle events. Each subscription has its own
    /// result-payload policy and receives events for queries executed after
    /// the subscription is created.
    pub fn subscribe_query_events(&self, options: QueryEventOptions) -> QueryEventStream {
        let (sender, receiver) = mpsc::channel();
        self.query_event_subscribers
            .lock()
            .push(QueryEventSubscriber {
                sender,
                include_results: options.include_results,
            });
        QueryEventStream { receiver }
    }

    pub(super) fn mysql_strict(&self) -> bool {
        self.cfg.compatibility_profile == CompatibilityProfile::MysqlStrict
    }

    pub(super) fn enforces_uniqueness(&self) -> bool {
        self.mysql_strict() || self.cfg.unique_mode == UniqueMode::Enforce
    }

    pub fn execute_sql(&self, sql: &str) -> Result<Vec<QueryResult>> {
        self.execute_sql_internal(sql, sql, true, true)
    }

    /// Execute a statement without converting the internal JSON-null marker
    /// to SQL `NULL`. The MySQL wire layer needs that distinction so a JSON
    /// literal `null` is sent as the bytes `null`, while an actual SQL NULL is
    /// sent as a protocol NULL.
    pub(crate) fn execute_sql_for_wire(&self, sql: &str) -> Result<Vec<QueryResult>> {
        self.execute_sql_internal(sql, sql, false, true)
    }

    fn execute_sql_internal(
        &self,
        event_sql: &str,
        execution_sql: &str,
        normalize_json_nulls: bool,
        emit_events: bool,
    ) -> Result<Vec<QueryResult>> {
        let query_id =
            emit_events.then(|| self.next_query_id.fetch_add(1, AtomicOrdering::Relaxed));
        let started = Instant::now();
        if let Some(query_id) = query_id {
            self.publish_query_event(QueryEvent::Received(QueryReceivedEvent {
                query_id,
                query: event_sql.to_string(),
            }));
        }

        tracing::debug!(sql = execution_sql, "sql.execute");
        let mut out = Vec::new();
        let outcome: Result<Vec<QueryResult>> = (|| {
            for raw in split_sql_statements(execution_sql) {
                if raw.is_empty() {
                    continue;
                }
                self.maybe_inject_failure(&raw)?;
                if let Some(result) = self.execute_compat_statement(&raw)? {
                    out.push(result);
                    continue;
                }
                for statement in super::parse(&raw)? {
                    validate_statement_support(&statement)?;
                    let mut result = self.execute_statement_unobserved(statement)?;
                    if normalize_json_nulls {
                        for row in &mut result.rows {
                            for value in row.values_mut() {
                                *value = eval::public_json_value(value);
                            }
                        }
                    }
                    out.push(result);
                }
            }
            Ok(out)
        })();

        if let Some(query_id) = query_id {
            match &outcome {
                Ok(results) => {
                    self.publish_query_completed(query_id, started.elapsed(), Some(results), None)
                }
                Err(error) => self.publish_query_completed(
                    query_id,
                    started.elapsed(),
                    None,
                    Some(error.to_string()),
                ),
            }
        }
        outcome
    }

    fn publish_query_event(&self, event: QueryEvent) {
        let mut subscribers = self.query_event_subscribers.lock();
        subscribers.retain(|subscriber| subscriber.sender.send(event.clone()).is_ok());
    }

    fn publish_query_completed(
        &self,
        query_id: u64,
        duration: StdDuration,
        results: Option<&[QueryResult]>,
        error: Option<String>,
    ) {
        let result_set_count = results.map_or(0, <[QueryResult]>::len);
        let result_set_size = results.map_or(0, |results| {
            results.iter().map(|result| result.rows.len()).sum()
        });
        let mut subscribers = self.query_event_subscribers.lock();
        subscribers.retain(|subscriber| {
            let event_results = if subscriber.include_results {
                results.map(|results| results.iter().cloned().map(public_query_result).collect())
            } else {
                None
            };
            subscriber
                .sender
                .send(QueryEvent::Completed(QueryCompletedEvent {
                    query_id,
                    duration,
                    result_set_count,
                    result_set_size,
                    results: event_results,
                    error: error.clone(),
                }))
                .is_ok()
        });
    }

    fn query_failed_before_execution(
        &self,
        query: &str,
        error: anyhow::Error,
    ) -> Result<Vec<QueryResult>> {
        let query_id = self.next_query_id.fetch_add(1, AtomicOrdering::Relaxed);
        let started = Instant::now();
        self.publish_query_event(QueryEvent::Received(QueryReceivedEvent {
            query_id,
            query: query.to_string(),
        }));
        self.publish_query_completed(query_id, started.elapsed(), None, Some(error.to_string()));
        Err(error)
    }

    fn maybe_inject_failure(&self, sql: &str) -> Result<()> {
        let cfg = &self.cfg.failure_injection;
        if cfg.query_delay_ms > 0 {
            std::thread::sleep(std::time::Duration::from_millis(cfg.query_delay_ms));
        }

        if is_read_sql(sql) {
            if cfg.fail_read_every > 0 {
                let count = self.read_query_count.fetch_add(1, AtomicOrdering::Relaxed) + 1;
                if count.is_multiple_of(cfg.fail_read_every) {
                    return Err(anyhow!(
                        "simulated read failure (--fail-read-every={})",
                        cfg.fail_read_every
                    ));
                }
            }
        } else if cfg.fail_write_every > 0 {
            let count = self.write_query_count.fetch_add(1, AtomicOrdering::Relaxed) + 1;
            if count.is_multiple_of(cfg.fail_write_every) {
                return Err(anyhow!(
                    "simulated write failure (--fail-write-every={})",
                    cfg.fail_write_every
                ));
            }
        }

        Ok(())
    }

    pub fn execute_sql_with_params(&self, sql: &str, params: &[Value]) -> Result<Vec<QueryResult>> {
        let execution_sql = match substitute_params(sql, params) {
            Ok(execution_sql) => execution_sql,
            Err(error) => return self.query_failed_before_execution(sql, error),
        };
        self.execute_sql_internal(sql, &execution_sql, true, true)
    }

    pub(crate) fn execute_sql_with_params_for_wire(
        &self,
        sql: &str,
        params: &[Value],
    ) -> Result<Vec<QueryResult>> {
        let execution_sql = match substitute_params(sql, params) {
            Ok(execution_sql) => execution_sql,
            Err(error) => return self.query_failed_before_execution(sql, error),
        };
        self.execute_sql_internal(sql, &execution_sql, false, true)
    }

    pub(crate) fn execute_sql_with_params_without_events(
        &self,
        sql: &str,
        params: &[Value],
    ) -> Result<Vec<QueryResult>> {
        let execution_sql = substitute_params(sql, params)?;
        self.execute_sql_internal(sql, &execution_sql, true, false)
    }

    pub fn execute_statement(&self, stmt: Statement) -> Result<QueryResult> {
        let query = stmt.to_string();
        let query_id = self.next_query_id.fetch_add(1, AtomicOrdering::Relaxed);
        let started = Instant::now();
        self.publish_query_event(QueryEvent::Received(QueryReceivedEvent { query_id, query }));
        let outcome = self.execute_statement_unobserved(stmt);
        match &outcome {
            Ok(result) => self.publish_query_completed(
                query_id,
                started.elapsed(),
                Some(std::slice::from_ref(result)),
                None,
            ),
            Err(error) => self.publish_query_completed(
                query_id,
                started.elapsed(),
                None,
                Some(error.to_string()),
            ),
        }
        outcome
    }

    fn execute_statement_unobserved(&self, stmt: Statement) -> Result<QueryResult> {
        match stmt {
            Statement::CreateTable(create) => self.create_table(
                create.name,
                create.columns,
                create.constraints,
                create.if_not_exists,
                create.temporary,
            ),
            Statement::AlterTable {
                name,
                operations,
                if_exists,
                ..
            } => self.alter_table(name, operations, if_exists),
            Statement::CreateIndex(create) => self.create_index_from_sql(&create.to_string()),
            Statement::Insert(insert) => self.insert_rows(insert),
            Statement::Query(query) => self.select_query(*query),
            Statement::Update {
                table,
                assignments,
                from,
                selection,
                returning,
                ..
            } => self.update_rows(table, assignments, from, selection, returning),
            Statement::Delete(delete) => self.delete_rows(delete),
            Statement::Drop {
                object_type: sqlparser::ast::ObjectType::Table,
                names,
                if_exists,
                ..
            } => self.drop_table(names, if_exists),
            Statement::Drop {
                object_type: sqlparser::ast::ObjectType::Index,
                names,
                ..
            } => self.drop_index(names),
            Statement::Truncate { table_names, .. } => self.truncate_tables(table_names),
            Statement::StartTransaction { .. }
            | Statement::Commit { .. }
            | Statement::Rollback { .. }
            | Statement::Use { .. }
            | Statement::ShowVariable { .. }
            | Statement::SetVariable { .. }
            | Statement::ShowVariables { .. }
            | Statement::ShowStatus { .. } => Ok(QueryResult::default()),
            Statement::ShowTables { .. } => Ok(self.show_tables()),
            _ => Err(anyhow!("statement not supported yet")),
        }
    }

    fn execute_compat_statement(&self, sql: &str) -> Result<Option<QueryResult>> {
        let trimmed = sql.trim().trim_end_matches(';').trim();
        if trimmed.is_empty() {
            return Ok(Some(QueryResult::default()));
        }
        let upper = trimmed.to_ascii_uppercase();

        if upper.starts_with("CREATE DATABASE") || upper.starts_with("DROP DATABASE") {
            return Ok(Some(QueryResult::default()));
        }
        if let Some((table, index)) = parse_alter_table_drop_index(trimmed) {
            return Ok(Some(self.drop_index_from_table(&table, &index)?));
        }
        if upper.starts_with("SHOW DATABASES") || upper.starts_with("SHOW SCHEMAS") {
            return Ok(Some(show_databases_result()));
        }
        if let Some(table) = parse_show_columns_table(trimmed) {
            return Ok(Some(self.show_columns(&table)));
        }
        if let Some(table) = parse_describe_table(trimmed) {
            return Ok(Some(self.show_columns(&table)));
        }
        if let Some(table) = parse_show_index_table(trimmed) {
            return Ok(Some(self.show_index(&table)));
        }
        if let Some(table) = parse_show_create_table(trimmed) {
            return Ok(Some(self.show_create_table(&table)));
        }
        if let Some((from, to)) = parse_rename_table(trimmed) {
            return Ok(Some(self.rename_table(&from, &to)?));
        }
        if upper.starts_with("SELECT ")
            && let Some(result) = select_system_variables(trimmed)
        {
            return Ok(Some(result));
        }

        Ok(None)
    }
}

fn public_query_result(mut result: QueryResult) -> QueryResult {
    for row in &mut result.rows {
        for value in row.values_mut() {
            *value = eval::public_json_value(value);
        }
    }
    result
}

pub type SharedEngine = Arc<Engine>;

#[cfg(test)]
mod tests;
