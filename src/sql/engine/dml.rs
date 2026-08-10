use super::*;

type ParentRowChange = (Map<String, Value>, Map<String, Value>);

impl Engine {
    pub(super) fn insert_rows(&self, insert: sqlparser::ast::Insert) -> Result<QueryResult> {
        let table = object_name(&insert.table_name)?;
        let explicit_columns: Vec<String> = insert.columns.into_iter().map(|i| i.value).collect();
        let query = insert
            .source
            .ok_or_else(|| anyhow!("missing INSERT source"))?;
        let ignore = insert.ignore;
        let replace = insert.replace_into;
        let returning = insert.returning;
        let on_duplicate = match insert.on {
            Some(OnInsert::DuplicateKeyUpdate(assignments)) => assignments,
            Some(_) | None => Vec::new(),
        };

        let mut prepared_rows = Vec::new();

        match query.body.as_ref() {
            SetExpr::Values(v) => {
                let values = v.rows.clone();
                let columns = self.resolve_insert_columns(&table, explicit_columns, &values)?;
                for row in values {
                    let mut data = Map::new();
                    for (idx, expr) in row.into_iter().enumerate() {
                        if let Some(col) = columns.get(idx) {
                            let value = match &expr {
                                Expr::Identifier(identifier)
                                    if identifier.value.eq_ignore_ascii_case("DEFAULT") =>
                                {
                                    sql_default_value()
                                }
                                _ => self.eval_expr_ctx(&expr, &Map::new(), 0)?,
                            };
                            data.insert(col.clone(), value);
                        }
                    }
                    prepared_rows.push(data);
                }
            }
            SetExpr::Select(_) => {
                let select_result = self.select_query((*query).clone())?;
                let columns = if explicit_columns.is_empty() {
                    select_result.columns.clone()
                } else {
                    explicit_columns
                };
                for row in select_result.rows {
                    let mut data = Map::new();
                    for (idx, col) in columns.iter().enumerate() {
                        let value = select_result
                            .columns
                            .get(idx)
                            .and_then(|src_col| row.get(src_col).cloned())
                            .unwrap_or(Value::Null);
                        data.insert(col.clone(), value);
                    }
                    prepared_rows.push(data);
                }
            }
            _ => return Err(anyhow!("only VALUES and SELECT insert are supported")),
        }

        self.insert_prepared_rows(
            &table,
            prepared_rows,
            InsertRowsOptions {
                ignore,
                replace,
                on_duplicate: &on_duplicate,
                returning: returning.as_deref(),
            },
        )
    }

    pub(super) fn insert_prepared_rows(
        &self,
        table: &str,
        rows: Vec<Map<String, Value>>,
        options: InsertRowsOptions<'_>,
    ) -> Result<QueryResult> {
        let mut affected = 0_u64;
        let mut first_insert_id = 0_u64;
        let mut rows_to_persist: BTreeMap<String, StoredRow> = BTreeMap::new();
        let mut rows_to_delete: BTreeSet<String> = BTreeSet::new();
        let mut returned_rows = Vec::new();
        for mut data in rows {
            self.validate_generated_insert_values(table, &data)?;
            self.apply_defaults(table, &mut data)?;
            self.apply_generated_columns(table, &mut data)?;
            self.apply_schema_types(table, &mut data)?;
            // Validate before taking the table's DashMap write guard. A parent
            // table can hash to the same shard, and trying to read that shard
            // while holding the child guard deadlocks.
            self.validate_foreign_key_row(table, &data)?;

            let (row_id, generated_id) = self.resolve_row_id(table, &data)?;
            let generated_insert_id = generated_id.then(|| value_to_u64(&row_id)).flatten();
            if !data.contains_key("id") || data.get("id").is_some_and(is_defaultish) {
                data.insert("id".to_string(), row_id.clone());
            }

            let key = row_id.to_string();
            let mut table_rows = self.rows.entry(table.to_string()).or_default();
            let conflict_keys = self.find_conflict_keys(table, &key, &data, &table_rows);

            if !conflict_keys.is_empty() {
                if options.ignore {
                    continue;
                }

                if !options.on_duplicate.is_empty() {
                    let conflict_key = conflict_keys
                        .iter()
                        .next()
                        .ok_or_else(|| anyhow!("conflict row disappeared"))?;
                    let existing = table_rows
                        .get_mut(conflict_key)
                        .ok_or_else(|| anyhow!("conflict row disappeared"))?;
                    let original_data = existing.data.clone();
                    for assignment in options.on_duplicate {
                        let col = assignment_target_name(assignment);
                        let value =
                            eval_insert_update_value(&assignment.value, &existing.data, &data)?;
                        existing.data.insert(col, value);
                    }
                    returned_rows.push(existing.data.clone());
                    if existing.data != original_data {
                        existing.version += 1;
                        existing.updated_at = Utc::now();
                        rows_to_persist.insert(conflict_key.clone(), existing.clone());
                        // MySQL reports two affected rows when ON DUPLICATE KEY
                        // UPDATE changes an existing row (and zero for a no-op).
                        affected += 2;
                    }
                    continue;
                }

                if options.replace || !self.enforces_uniqueness() {
                    for conflict_key in conflict_keys {
                        if table_rows.remove(&conflict_key).is_some() {
                            if options.replace {
                                affected += 1;
                            }
                            rows_to_delete.insert(conflict_key.clone());
                            rows_to_persist.remove(&conflict_key);
                        }
                    }
                } else if conflict_keys.contains(&key) {
                    return Err(anyhow!("primary key conflict on {table}: {key}"));
                } else {
                    self.enforce_unique_if_needed(table, &row_id, &data, &table_rows)?;
                }
            } else {
                self.enforce_unique_if_needed(table, &row_id, &data, &table_rows)?;
            }

            let stored = StoredRow::new(table.to_string(), row_id, data);
            table_rows.insert(key.clone(), stored.clone());
            if first_insert_id == 0 {
                first_insert_id = generated_insert_id.unwrap_or(0);
            }
            returned_rows.push(stored.data.clone());
            rows_to_persist.insert(key, stored);
            affected += 1;
        }
        self.rebuild_indexes(table);
        self.persist_auto_inc()?;
        for key in rows_to_delete {
            self.delete_row_from_storage(table, &key)?;
        }
        for (key, row) in rows_to_persist {
            self.persist_row(table, &key, &row)?;
        }
        if first_insert_id != 0 {
            self.last_insert_id
                .store(first_insert_id, AtomicOrdering::Relaxed);
        }

        self.returning_result(
            table,
            options.returning,
            returned_rows,
            affected,
            first_insert_id,
        )
    }

    pub(super) fn resolve_insert_columns(
        &self,
        table: &str,
        explicit_columns: Vec<String>,
        values: &[Vec<Expr>],
    ) -> Result<Vec<String>> {
        if self.mysql_strict() && !self.schemas.contains_key(table) {
            return Err(anyhow!("unknown table: {table}"));
        }
        if !explicit_columns.is_empty() {
            if self.mysql_strict() {
                let schema = self
                    .schemas
                    .get(table)
                    .map(|schema| schema.clone())
                    .ok_or_else(|| anyhow!("unknown table: {table}"))?;
                let mut seen = BTreeSet::new();
                for column in &explicit_columns {
                    if !seen.insert(column.to_ascii_lowercase()) {
                        return Err(anyhow!("column '{column}' specified twice"));
                    }
                    if !schema
                        .columns
                        .keys()
                        .any(|known| known.eq_ignore_ascii_case(column))
                    {
                        return Err(anyhow!("unknown column: {column}"));
                    }
                }
                if values.iter().any(|row| row.len() != explicit_columns.len()) {
                    return Err(anyhow!("column count doesn't match value count"));
                }
            }
            self.ensure_schema_for_insert(table, &explicit_columns)?;
            return Ok(explicit_columns);
        }

        let width = values.iter().map(Vec::len).max().unwrap_or(0);
        if let Some(schema) = self.schemas.get(table).map(|schema| schema.clone()) {
            let mut columns = ordered_schema_columns(&schema);
            if self.mysql_strict()
                && values
                    .iter()
                    .any(|row| !row.is_empty() && row.len() != columns.len())
            {
                return Err(anyhow!("column count doesn't match value count"));
            }
            if width > columns.len() {
                if self.mysql_strict() {
                    return Err(anyhow!("column count doesn't match value count"));
                }
                let mut schema = schema;
                for idx in columns.len() + 1..=width {
                    let column = generated_position_column(idx);
                    add_schema_column(&mut schema, column.clone(), ColumnHint::default());
                    columns.push(column);
                }
                schema.updated_at = Some(Utc::now());
                self.schemas.insert(table.to_string(), schema);
                self.persist_schema(table)?;
            }
            return Ok(columns);
        }

        let columns = (1..=width)
            .map(generated_position_column)
            .collect::<Vec<_>>();
        self.ensure_schema_for_insert(table, &columns)?;
        Ok(columns)
    }

    pub(super) fn ensure_schema_for_insert(&self, table: &str, columns: &[String]) -> Result<()> {
        if self.schemas.contains_key(table) {
            return Ok(());
        }
        if self.mysql_strict() {
            return Err(anyhow!("unknown table: {table}"));
        }
        if columns.is_empty() {
            return Err(anyhow!(
                "cannot infer schema for {table}: INSERT must provide at least one value or named column"
            ));
        }

        let mut schema = TableSchemaHint {
            table: table.to_string(),
            updated_at: Some(Utc::now()),
            ..TableSchemaHint::default()
        };
        for column in columns {
            add_schema_column(&mut schema, column.clone(), ColumnHint::default());
        }
        self.schemas.insert(table.to_string(), schema);
        self.rows.entry(table.to_string()).or_default();
        self.persist_schema(table)
    }

    pub(super) fn ensure_schema_for_seed(
        &self,
        table: &str,
        rows: &[Map<String, Value>],
    ) -> Result<()> {
        let columns = seed_row_columns(rows);
        if columns.is_empty() {
            if self.schemas.contains_key(table) {
                return Ok(());
            }
            return Err(anyhow!(
                "cannot infer schema for {table}: seed rows must include at least one column"
            ));
        }

        let existed = self.schemas.contains_key(table);
        let mut schema = self
            .schemas
            .get(table)
            .map(|schema| schema.clone())
            .unwrap_or_else(|| TableSchemaHint {
                table: table.to_string(),
                ..TableSchemaHint::default()
            });

        let mut changed = !existed;
        for column in columns {
            if !schema.columns.contains_key(&column) {
                add_schema_column(&mut schema, column, ColumnHint::default());
                changed = true;
            }
        }

        if changed {
            schema.updated_at = Some(Utc::now());
            self.schemas.insert(table.to_string(), schema);
            self.rows.entry(table.to_string()).or_default();
            self.persist_schema(table)?;
        }

        Ok(())
    }
    pub(super) fn update_rows(
        &self,
        table: TableWithJoins,
        assignments: Vec<Assignment>,
        from: Option<TableWithJoins>,
        selection: Option<Expr>,
        returning: Option<Vec<SelectItem>>,
    ) -> Result<QueryResult> {
        if from.is_some() {
            return Err(anyhow!("UPDATE ... FROM is not supported yet"));
        }

        let table_name = table_factor_name(&table.relation)?;
        if !self.schemas.contains_key(&table_name) {
            return Err(anyhow!("unknown table: {table_name}"));
        }
        let mut updated = 0_u64;
        let current_rows = self
            .rows
            .get(&table_name)
            .map(|rows| rows.clone())
            .unwrap_or_default();
        let mut next_rows = current_rows.clone();
        let mut changed_rows: BTreeMap<String, StoredRow> = BTreeMap::new();
        let mut deleted_keys: BTreeSet<String> = BTreeSet::new();
        let mut returned_rows = Vec::new();
        let mut parent_updates = Vec::new();

        for (old_key, current_row) in &current_rows {
            if !self.enforces_uniqueness() && !next_rows.contains_key(old_key) {
                continue;
            }
            let Some(match_context) =
                self.update_match_context(&table, &table_name, current_row, selection.as_ref())?
            else {
                continue;
            };

            let mut updated_data = current_row.data.clone();
            for assignment in &assignments {
                let col = assignment_target_name(assignment);
                if self
                    .schemas
                    .get(&table_name)
                    .and_then(|schema| schema.columns.get(&col).cloned())
                    .is_some_and(|hint| hint.generated.is_some())
                {
                    return Err(anyhow!(
                        "the value specified for generated column '{col}' is not allowed"
                    ));
                }
                if self.mysql_strict()
                    && !self
                        .schemas
                        .get(&table_name)
                        .is_some_and(|schema| schema.columns.contains_key(&col))
                {
                    return Err(anyhow!("unknown column: {col}"));
                }
                let value_context = self.update_assignment_context(
                    &table.relation,
                    &table_name,
                    &updated_data,
                    &match_context,
                )?;
                let value = self.eval_expr_ctx(&assignment.value, &value_context, 0)?;
                updated_data.insert(col, value);
            }
            self.apply_defaults(&table_name, &mut updated_data)?;
            self.apply_generated_columns(&table_name, &mut updated_data)?;
            self.apply_schema_types(&table_name, &mut updated_data)?;
            self.validate_foreign_key_row(&table_name, &updated_data)?;

            let (row_id, new_key) =
                self.updated_row_identity(&table_name, current_row, &updated_data);

            let mut updated_row = current_row.clone();
            updated_row.id = row_id;
            updated_row.data = updated_data;
            updated_row.version += 1;
            updated_row.updated_at = Utc::now();
            if current_row.data != updated_row.data {
                parent_updates.push((current_row.data.clone(), updated_row.data.clone()));
            }

            next_rows.remove(old_key);
            if new_key != *old_key {
                deleted_keys.insert(old_key.clone());
                changed_rows.remove(old_key);
            }

            if !self.enforces_uniqueness() {
                let conflict_keys =
                    self.find_conflict_keys(&table_name, &new_key, &updated_row.data, &next_rows);
                for conflict_key in conflict_keys {
                    if next_rows.remove(&conflict_key).is_some() {
                        deleted_keys.insert(conflict_key.clone());
                        changed_rows.remove(&conflict_key);
                    }
                }
            } else if next_rows.contains_key(&new_key) {
                return Err(anyhow!("primary key conflict on {table_name}: {new_key}"));
            }

            next_rows.insert(new_key.clone(), updated_row.clone());
            returned_rows.push(updated_row.data.clone());
            changed_rows.insert(new_key, updated_row);
            updated += 1;
        }

        self.validate_unique_constraints(&table_name, &next_rows)?;
        self.apply_parent_update_actions(&table_name, &parent_updates)?;
        self.rows.insert(table_name.clone(), next_rows);
        self.rebuild_indexes(&table_name);
        for key in deleted_keys {
            self.delete_row_from_storage(&table_name, &key)?;
        }
        for (pk, row) in changed_rows {
            self.persist_row(&table_name, &pk, &row)?;
        }

        self.returning_result(&table_name, returning.as_deref(), returned_rows, updated, 0)
    }

    fn update_match_context(
        &self,
        table: &TableWithJoins,
        table_name: &str,
        row: &StoredRow,
        selection: Option<&Expr>,
    ) -> Result<Option<Map<String, Value>>> {
        let contexts = self.update_join_contexts(table, table_name, row)?;
        for context in contexts {
            if self.matches_selection_ctx(selection, &context, 0)? {
                return Ok(Some(context));
            }
        }
        Ok(None)
    }

    fn update_join_contexts(
        &self,
        table: &TableWithJoins,
        table_name: &str,
        row: &StoredRow,
    ) -> Result<Vec<Map<String, Value>>> {
        let (_, left_alias) = table_factor_name_and_alias(&table.relation)?;
        let left_data = self.current_schema_row(table_name, &row.data);
        let mut left_map = left_data.clone();
        add_qualified_columns(&mut left_map, table_name, &left_data);
        if let Some(alias) = &left_alias {
            add_qualified_columns(&mut left_map, alias, &left_data);
        }

        let mut current = vec![left_map];
        for join in &table.joins {
            let (right_table, right_alias) = table_factor_name_and_alias(&join.relation)?;
            let right_rows = self
                .rows
                .get(&right_table)
                .map(|rows| rows.clone())
                .unwrap_or_default();
            let mut next = Vec::new();

            for candidate in &current {
                let mut matched = false;
                for right_row in right_rows.values() {
                    let right_data = self.current_schema_row(&right_table, &right_row.data);
                    let mut combined = candidate.clone();
                    add_qualified_columns(&mut combined, &right_table, &right_data);
                    if let Some(alias) = &right_alias {
                        add_qualified_columns(&mut combined, alias, &right_data);
                    }
                    for (key, value) in &right_data {
                        combined.entry(key.clone()).or_insert_with(|| value.clone());
                    }
                    if self.join_matches_ctx(&join.join_operator, &combined)? {
                        matched = true;
                        next.push(combined);
                    }
                }

                if !matched && matches!(join.join_operator, JoinOperator::LeftOuter(_)) {
                    let right_nulls = self.current_schema_null_row(&right_table);
                    let mut combined = candidate.clone();
                    add_qualified_columns(&mut combined, &right_table, &right_nulls);
                    if let Some(alias) = &right_alias {
                        add_qualified_columns(&mut combined, alias, &right_nulls);
                    }
                    for (key, value) in &right_nulls {
                        combined.entry(key.clone()).or_insert_with(|| value.clone());
                    }
                    next.push(combined);
                }
            }

            current = next;
        }

        Ok(current)
    }

    fn update_assignment_context(
        &self,
        relation: &TableFactor,
        table_name: &str,
        updated_data: &Map<String, Value>,
        match_context: &Map<String, Value>,
    ) -> Result<Map<String, Value>> {
        let (_, alias) = table_factor_name_and_alias(relation)?;
        let mut context = match_context.clone();
        let base_data = self.current_schema_row(table_name, updated_data);
        for (key, value) in &base_data {
            context.insert(key.clone(), value.clone());
        }
        add_qualified_columns(&mut context, table_name, &base_data);
        if let Some(alias) = alias {
            add_qualified_columns(&mut context, &alias, &base_data);
        }
        Ok(context)
    }

    pub(super) fn delete_rows(&self, delete: sqlparser::ast::Delete) -> Result<QueryResult> {
        let from = match &delete.from {
            sqlparser::ast::FromTable::WithFromKeyword(v)
            | sqlparser::ast::FromTable::WithoutKeyword(v) => v,
        };
        if !delete.tables.is_empty()
            || delete.using.is_some()
            || from.iter().any(|table| !table.joins.is_empty())
        {
            return self.delete_joined_rows(delete);
        }
        let returning = delete.returning;
        if from.len() != 1 {
            return Err(anyhow!("DELETE supports exactly one target table"));
        }
        let root = from
            .first()
            .ok_or_else(|| anyhow!("missing DELETE target table"))?;
        if !root.joins.is_empty() {
            return Err(anyhow!("DELETE with joins is not supported yet"));
        }
        let table_name = table_factor_name(&root.relation)?;

        let mut deleted = 0_u64;
        let mut deleted_keys = Vec::new();
        let mut returned_rows = Vec::new();
        let current_rows = self
            .rows
            .get(&table_name)
            .map(|rows| rows.clone())
            .unwrap_or_default();
        let (_, table_alias) = table_factor_name_and_alias(&root.relation)?;
        let mut candidates = Vec::new();
        for (k, row) in &current_rows {
            let base_view = self.current_schema_row(&table_name, &row.data);
            let mut view = base_view.clone();
            add_qualified_columns(&mut view, &table_name, &base_view);
            if let Some(alias) = &table_alias {
                add_qualified_columns(&mut view, alias, &base_view);
            }
            if self.matches_selection_ctx(delete.selection.as_ref(), &view, 0)? {
                candidates.push((k.clone(), row.clone(), view));
            }
        }
        let schema = self.schemas.get(&table_name).map(|schema| schema.clone());
        sort_delete_candidates(&mut candidates, &delete.order_by, schema.as_ref())?;
        if let Some(limit) = &delete.limit {
            candidates.truncate(expr_to_usize(limit)?);
        }

        if !candidates.is_empty() {
            let candidate_keys = candidates
                .iter()
                .map(|(key, _, _)| key.clone())
                .collect::<BTreeSet<_>>();
            self.apply_parent_delete_actions(&table_name, &candidate_keys)?;
            let mut next_rows = current_rows;
            for (key, _, _) in candidates {
                if let Some(row) = next_rows.remove(&key) {
                    returned_rows.push(row.data);
                    deleted_keys.push(key);
                    deleted += 1;
                }
            }
            self.rows.insert(table_name.clone(), next_rows);
        }
        self.rebuild_indexes(&table_name);
        for key in deleted_keys {
            self.delete_row_from_storage(&table_name, &key)?;
        }

        self.returning_result(&table_name, returning.as_deref(), returned_rows, deleted, 0)
    }

    fn delete_joined_rows(&self, delete: sqlparser::ast::Delete) -> Result<QueryResult> {
        if !delete.order_by.is_empty() || delete.limit.is_some() {
            return Err(anyhow!(
                "ORDER BY and LIMIT are not supported for multi-table DELETE"
            ));
        }
        let from = match &delete.from {
            sqlparser::ast::FromTable::WithFromKeyword(v)
            | sqlparser::ast::FromTable::WithoutKeyword(v) => v,
        };
        let sources = delete.using.as_deref().unwrap_or(from);
        if sources.is_empty() {
            return Err(anyhow!("missing DELETE source table"));
        }

        let aliases = delete_source_aliases(sources)?;
        let targets = if !delete.tables.is_empty() {
            delete
                .tables
                .iter()
                .map(|target| {
                    let target_name = object_name(target)?;
                    aliases
                        .get(&target_name.to_ascii_lowercase())
                        .cloned()
                        .ok_or_else(|| anyhow!("unknown table: {target_name}"))
                })
                .collect::<Result<Vec<_>>>()?
        } else {
            from.iter()
                .map(|target| delete_target_from_factor(&target.relation))
                .collect::<Result<Vec<_>>>()?
        };

        let mut contexts = vec![Map::new()];
        for source in sources {
            let source_rows = self.joined_table_factor_rows(source, None)?;
            let mut next = Vec::new();
            for context in &contexts {
                for source_row in &source_rows {
                    next.push(merge_delete_context(context, source_row));
                }
            }
            contexts = next;
        }
        let mut matching_contexts = Vec::new();
        for context in contexts {
            if self.matches_selection_ctx(delete.selection.as_ref(), &context, 0)? {
                matching_contexts.push(context);
            }
        }
        let contexts = matching_contexts;

        let mut keys_by_table = BTreeMap::<String, BTreeSet<String>>::new();
        for target in &targets {
            let schema = self
                .schemas
                .get(&target.table)
                .map(|schema| schema.clone())
                .ok_or_else(|| anyhow!("unknown table: {}", target.table))?;
            let primary_key = schema
                .primary_key
                .first()
                .cloned()
                .or_else(|| schema.columns.contains_key("id").then(|| "id".to_string()))
                .ok_or_else(|| {
                    anyhow!(
                        "multi-table DELETE requires a primary key on {}",
                        target.table
                    )
                })?;
            for context in &contexts {
                let qualified = format!("{}.{}", target.qualifier, primary_key);
                if let Some(value) = context
                    .get(&qualified)
                    .or_else(|| context.get(&format!("{}.{}", target.table, primary_key)))
                    && value != &Value::Null
                {
                    keys_by_table
                        .entry(target.table.clone())
                        .or_default()
                        .insert(value.to_string());
                }
            }
        }

        let mut rows_affected = 0_u64;
        let mut returned_rows = Vec::new();
        for target in &targets {
            let Some(keys) = keys_by_table.remove(&target.table) else {
                continue;
            };
            self.apply_parent_delete_actions(&target.table, &keys)?;
            let mut table_rows = self
                .rows
                .get(&target.table)
                .map(|rows| rows.clone())
                .unwrap_or_default();
            for key in keys {
                if let Some(row) = table_rows.remove(&key) {
                    rows_affected += 1;
                    if targets.len() == 1 {
                        returned_rows.push(row.data);
                    }
                    self.delete_row_from_storage(&target.table, &key)?;
                }
            }
            self.rows.insert(target.table.clone(), table_rows);
            self.rebuild_indexes(&target.table);
        }

        if targets.len() == 1 {
            self.returning_result(
                &targets[0].table,
                delete.returning.as_deref(),
                returned_rows,
                rows_affected,
                0,
            )
        } else {
            if delete.returning.is_some() {
                return Err(anyhow!("RETURNING is ambiguous for multi-table DELETE"));
            }
            Ok(QueryResult {
                rows_affected,
                ..QueryResult::default()
            })
        }
    }

    fn returning_result(
        &self,
        table: &str,
        returning: Option<&[SelectItem]>,
        rows: Vec<Map<String, Value>>,
        rows_affected: u64,
        last_insert_id: u64,
    ) -> Result<QueryResult> {
        let Some(projection) = returning else {
            return Ok(QueryResult {
                rows_affected,
                last_insert_id,
                columns: vec![],
                column_metadata: vec![],
                rows: vec![],
            });
        };

        let rows = rows
            .into_iter()
            .map(|row| {
                let row = self.current_schema_row(table, &row);
                self.project_row_ctx(projection, &row, last_insert_id)
            })
            .collect::<Result<Vec<_>>>()?;

        Ok(QueryResult {
            rows_affected,
            last_insert_id,
            columns: self.returning_columns(table, projection, rows.first()),
            column_metadata: vec![],
            rows,
        })
    }

    fn returning_columns(
        &self,
        table: &str,
        projection: &[SelectItem],
        first_row: Option<&Map<String, Value>>,
    ) -> Vec<String> {
        if projection
            .iter()
            .any(|item| matches!(item, SelectItem::Wildcard(_)))
        {
            if let Some(schema) = self.schemas.get(table).map(|schema| schema.clone()) {
                return ordered_schema_columns(&schema);
            }
            return first_row
                .map(|row| row.keys().cloned().collect())
                .unwrap_or_default();
        }

        infer_projection_columns(projection)
    }
    pub(super) fn apply_defaults(&self, table: &str, data: &mut Map<String, Value>) -> Result<()> {
        let Some(schema) = self.schemas.get(table).map(|schema| schema.clone()) else {
            return Ok(());
        };
        for (column, hint) in &schema.columns {
            let explicitly_default = data.get(column).is_some_and(is_default_keyword);
            if data.contains_key(column) && !explicitly_default {
                continue;
            }
            if let Some(default) = &hint.default {
                data.insert(column.clone(), eval_default_value(default)?);
            } else if explicitly_default {
                // MySQL treats DEFAULT on a nullable column without a declared
                // default as its implicit NULL default. Removing it for a
                // required/auto-increment column lets the normal missing-value
                // validation or identifier generation take over. An explicit
                // SQL NULL is intentionally not considered DEFAULT here.
                data.remove(column);
                if hint.nullable != Some(false) && !hint.auto_increment {
                    data.insert(column.clone(), Value::Null);
                }
            }
        }
        Ok(())
    }

    fn validate_generated_insert_values(
        &self,
        table: &str,
        data: &Map<String, Value>,
    ) -> Result<()> {
        let Some(schema) = self.schemas.get(table) else {
            return Ok(());
        };
        for (column, hint) in &schema.columns {
            if hint.generated.is_some()
                && data.get(column).is_some_and(|value| !is_defaultish(value))
            {
                return Err(anyhow!(
                    "the value specified for generated column '{column}' is not allowed"
                ));
            }
        }
        Ok(())
    }

    pub(super) fn apply_generated_columns(
        &self,
        table: &str,
        data: &mut Map<String, Value>,
    ) -> Result<()> {
        let Some(schema) = self.schemas.get(table).map(|schema| schema.clone()) else {
            return Ok(());
        };
        for column in ordered_schema_columns(&schema) {
            let Some(expression) = schema
                .columns
                .get(&column)
                .and_then(|hint| hint.generated.as_deref())
            else {
                continue;
            };
            let expression = parse_scalar_expr(expression)
                .ok_or_else(|| anyhow!("invalid generated column expression: {expression}"))?;
            let value = self.eval_expr_ctx(&expression, data, 0)?;
            data.insert(column, value);
        }
        Ok(())
    }

    pub(super) fn apply_schema_types(
        &self,
        table: &str,
        data: &mut Map<String, Value>,
    ) -> Result<()> {
        let Some(schema) = self.schemas.get(table).map(|schema| schema.clone()) else {
            if self.mysql_strict() {
                return Err(anyhow!("unknown table: {table}"));
            }
            return Ok(());
        };
        if self.mysql_strict() {
            for column in data.keys() {
                if !schema
                    .columns
                    .keys()
                    .any(|known| known.eq_ignore_ascii_case(column))
                {
                    return Err(anyhow!("unknown column: {column}"));
                }
            }
        }
        for (column, hint) in &schema.columns {
            if let Some(value) = data.get(column).cloned() {
                if value == Value::Null && hint.nullable == Some(false) && !hint.auto_increment {
                    return Err(anyhow!("column '{column}' cannot be null"));
                }
                if self.mysql_strict() {
                    validate_mysql_column_value(column, &value, hint)?;
                }
                data.insert(column.clone(), coerce_value_for_column(value, hint));
            } else if hint.nullable == Some(false) && hint.default.is_none() && !hint.auto_increment
            {
                return Err(anyhow!("column '{column}' does not have a default value"));
            }
        }
        Ok(())
    }

    fn validate_foreign_key_row(&self, table: &str, data: &Map<String, Value>) -> Result<()> {
        let Some(schema) = self.schemas.get(table).map(|schema| schema.clone()) else {
            return Ok(());
        };
        for foreign_key in &schema.foreign_keys {
            let local_values = foreign_key
                .columns
                .iter()
                .map(|column| data.get(column).cloned().unwrap_or(Value::Null))
                .collect::<Vec<_>>();
            if local_values.iter().any(|value| value == &Value::Null) {
                continue;
            }
            let parent_rows = self
                .rows
                .get(&foreign_key.referenced_table)
                .map(|rows| rows.clone())
                .unwrap_or_default();
            let matched = parent_rows.values().any(|parent| {
                let parent = self.current_schema_row(&foreign_key.referenced_table, &parent.data);
                foreign_key
                    .referenced_columns
                    .iter()
                    .zip(&local_values)
                    .all(|(column, local)| {
                        parent
                            .get(column)
                            .is_some_and(|referenced| mysql_eq(referenced, local))
                    })
            });
            if !matched {
                return Err(anyhow!(
                    "foreign key constraint fails: {}",
                    foreign_key.name
                ));
            }
        }
        Ok(())
    }

    fn apply_parent_delete_actions(
        &self,
        parent_table: &str,
        parent_keys: &BTreeSet<String>,
    ) -> Result<()> {
        let mut visited = BTreeSet::new();
        self.apply_parent_delete_actions_inner(parent_table, parent_keys, &mut visited)
    }

    fn apply_parent_delete_actions_inner(
        &self,
        parent_table: &str,
        parent_keys: &BTreeSet<String>,
        visited: &mut BTreeSet<(String, String)>,
    ) -> Result<()> {
        let parent_rows = self
            .rows
            .get(parent_table)
            .map(|rows| rows.clone())
            .unwrap_or_default();
        let parent_values = parent_keys
            .iter()
            .filter_map(|key| {
                parent_rows
                    .get(key)
                    .map(|row| (key.clone(), row.data.clone()))
            })
            .collect::<Vec<_>>();
        if parent_values.is_empty() {
            return Ok(());
        }

        let referencing = self
            .schemas
            .iter()
            .flat_map(|schema| {
                let table = schema.table.clone();
                schema
                    .foreign_keys
                    .iter()
                    .filter(|foreign_key| {
                        foreign_key
                            .referenced_table
                            .eq_ignore_ascii_case(parent_table)
                    })
                    .cloned()
                    .map(move |foreign_key| (table.clone(), foreign_key))
                    .collect::<Vec<_>>()
            })
            .collect::<Vec<_>>();

        for (child_table, foreign_key) in referencing {
            let child_rows = self
                .rows
                .get(&child_table)
                .map(|rows| rows.clone())
                .unwrap_or_default();
            let mut matching = BTreeSet::new();
            for (child_key, child) in &child_rows {
                if child_table.eq_ignore_ascii_case(parent_table) && parent_keys.contains(child_key)
                {
                    continue;
                }
                if parent_values.iter().any(|(_, parent)| {
                    foreign_key_row_matches(
                        &child.data,
                        &foreign_key.columns,
                        parent,
                        &foreign_key.referenced_columns,
                    )
                }) {
                    matching.insert(child_key.clone());
                }
            }
            if matching.is_empty() {
                continue;
            }

            match foreign_key
                .on_delete
                .as_deref()
                .unwrap_or("RESTRICT")
                .to_ascii_uppercase()
                .as_str()
            {
                "CASCADE" => {
                    let fresh = matching
                        .iter()
                        .filter(|key| visited.insert((child_table.clone(), (*key).clone())))
                        .cloned()
                        .collect::<BTreeSet<_>>();
                    self.apply_parent_delete_actions_inner(&child_table, &fresh, visited)?;
                    let mut next_rows = child_rows;
                    for key in fresh {
                        if next_rows.remove(&key).is_some() {
                            self.delete_row_from_storage(&child_table, &key)?;
                        }
                    }
                    self.rows.insert(child_table.clone(), next_rows);
                    self.rebuild_indexes(&child_table);
                }
                "SET NULL" => {
                    let mut next_rows = child_rows;
                    for key in matching {
                        if let Some(row) = next_rows.get_mut(&key) {
                            for column in &foreign_key.columns {
                                row.data.insert(column.clone(), Value::Null);
                            }
                            row.version += 1;
                            row.updated_at = Utc::now();
                            self.persist_row(&child_table, &key, row)?;
                        }
                    }
                    self.rows.insert(child_table.clone(), next_rows);
                    self.rebuild_indexes(&child_table);
                }
                "RESTRICT" | "NO ACTION" => {
                    return Err(anyhow!(
                        "cannot delete or update a referenced row: foreign key constraint fails ({})",
                        foreign_key.name
                    ));
                }
                action => return Err(anyhow!("unsupported ON DELETE action: {action}")),
            }
        }
        Ok(())
    }

    fn apply_parent_update_actions(
        &self,
        parent_table: &str,
        changes: &[ParentRowChange],
    ) -> Result<()> {
        if changes.is_empty() {
            return Ok(());
        }
        let referencing = self
            .schemas
            .iter()
            .flat_map(|schema| {
                let table = schema.table.clone();
                schema
                    .foreign_keys
                    .iter()
                    .filter(|foreign_key| {
                        foreign_key
                            .referenced_table
                            .eq_ignore_ascii_case(parent_table)
                    })
                    .cloned()
                    .map(move |foreign_key| (table.clone(), foreign_key))
                    .collect::<Vec<_>>()
            })
            .collect::<Vec<_>>();

        let mut actions = Vec::<(String, String, ForeignKeyHint, Vec<Value>)>::new();
        for (child_table, foreign_key) in referencing {
            if child_table.eq_ignore_ascii_case(parent_table) {
                continue;
            }
            let child_rows = self
                .rows
                .get(&child_table)
                .map(|rows| rows.clone())
                .unwrap_or_default();
            for (old_parent, new_parent) in changes {
                let old_values = foreign_key
                    .referenced_columns
                    .iter()
                    .map(|column| old_parent.get(column).cloned().unwrap_or(Value::Null))
                    .collect::<Vec<_>>();
                let new_values = foreign_key
                    .referenced_columns
                    .iter()
                    .map(|column| new_parent.get(column).cloned().unwrap_or(Value::Null))
                    .collect::<Vec<_>>();
                if old_values == new_values {
                    continue;
                }
                let matching = child_rows
                    .iter()
                    .filter(|(_, child)| {
                        foreign_key
                            .columns
                            .iter()
                            .zip(&old_values)
                            .all(|(column, old)| {
                                child.data.get(column).is_some_and(|value| {
                                    value != &Value::Null && mysql_eq(value, old)
                                })
                            })
                    })
                    .map(|(key, _)| key.clone())
                    .collect::<Vec<_>>();
                if matching.is_empty() {
                    continue;
                }
                let action = foreign_key
                    .on_update
                    .as_deref()
                    .unwrap_or("RESTRICT")
                    .to_ascii_uppercase();
                if matches!(action.as_str(), "RESTRICT" | "NO ACTION") {
                    return Err(anyhow!(
                        "cannot delete or update a referenced row: foreign key constraint fails ({})",
                        foreign_key.name
                    ));
                }
                let values = match action.as_str() {
                    "CASCADE" => new_values,
                    "SET NULL" => vec![Value::Null; foreign_key.columns.len()],
                    _ => return Err(anyhow!("unsupported ON UPDATE action: {action}")),
                };
                for key in matching {
                    actions.push((
                        child_table.clone(),
                        key,
                        foreign_key.clone(),
                        values.clone(),
                    ));
                }
            }
        }

        let mut by_table = BTreeMap::<String, BTreeMap<String, StoredRow>>::new();
        for (table, key, foreign_key, values) in actions {
            let rows = by_table.entry(table.clone()).or_insert_with(|| {
                self.rows
                    .get(&table)
                    .map(|rows| rows.clone())
                    .unwrap_or_default()
            });
            if let Some(row) = rows.get_mut(&key) {
                for (column, value) in foreign_key.columns.iter().zip(values) {
                    row.data.insert(column.clone(), value);
                }
                row.version += 1;
                row.updated_at = Utc::now();
            }
        }
        for (table, rows) in by_table {
            for (key, row) in &rows {
                self.persist_row(&table, key, row)?;
            }
            self.rows.insert(table.clone(), rows);
            self.rebuild_indexes(&table);
        }
        Ok(())
    }

    pub(super) fn resolve_row_id(
        &self,
        table: &str,
        data: &Map<String, Value>,
    ) -> Result<(Value, bool)> {
        let pk_col = self
            .schemas
            .get(table)
            .and_then(|schema| schema.primary_key.first().cloned())
            .or_else(|| {
                if data.contains_key("id") {
                    Some("id".to_string())
                } else {
                    None
                }
            });

        if let Some(pk_col) = pk_col {
            let maybe_auto_inc = self
                .schemas
                .get(table)
                .and_then(|schema| schema.columns.get(&pk_col).cloned())
                .map(|c| c.auto_increment)
                .unwrap_or(false);

            if let Some(v) = data.get(&pk_col) {
                if maybe_auto_inc && is_defaultish(v) {
                    let next = self.next_auto_inc(table, &pk_col);
                    return Ok((Value::Number(Number::from(next)), true));
                }
                return Ok((v.clone(), false));
            }

            if maybe_auto_inc || pk_col == "id" {
                let next = self.next_auto_inc(table, &pk_col);
                return Ok((Value::Number(Number::from(next)), true));
            }
        }

        Ok((Value::String(uuid::Uuid::new_v4().to_string()), false))
    }

    pub(super) fn updated_row_identity(
        &self,
        table: &str,
        current_row: &StoredRow,
        data: &Map<String, Value>,
    ) -> (Value, String) {
        let pk_col = self
            .schemas
            .get(table)
            .and_then(|schema| schema.primary_key.first().cloned())
            .or_else(|| data.contains_key("id").then(|| "id".to_string()));

        let id = pk_col
            .as_deref()
            .and_then(|column| data.get(column).cloned())
            .unwrap_or_else(|| current_row.id.clone());
        let key = id.to_string();
        (id, key)
    }

    pub(super) fn enforce_unique_if_needed(
        &self,
        table: &str,
        row_id: &Value,
        data: &Map<String, Value>,
        table_rows: &BTreeMap<String, StoredRow>,
    ) -> Result<()> {
        if !self.enforces_uniqueness() {
            return Ok(());
        }
        let Some(schema) = self.schemas.get(table).map(|s| s.clone()) else {
            return Ok(());
        };
        if schema.unique.is_empty() {
            return Ok(());
        }

        for unique_cols in &schema.unique {
            let Some(key) = schema_unique_key(&schema, data, unique_cols) else {
                continue;
            };
            for existing in table_rows.values() {
                if &existing.id == row_id {
                    continue;
                }
                if schema_unique_key(&schema, &existing.data, unique_cols).as_ref() == Some(&key) {
                    return Err(anyhow!(
                        "unique constraint violation on {table}({})",
                        unique_cols.join(",")
                    ));
                }
            }
        }
        Ok(())
    }

    pub(super) fn validate_unique_constraints(
        &self,
        table: &str,
        table_rows: &BTreeMap<String, StoredRow>,
    ) -> Result<()> {
        if !self.enforces_uniqueness() {
            return Ok(());
        }
        let Some(schema) = self.schemas.get(table).map(|s| s.clone()) else {
            return Ok(());
        };

        for unique_cols in &schema.unique {
            let mut seen: BTreeMap<String, String> = BTreeMap::new();
            for (pk, row) in table_rows {
                let Some(key) = schema_unique_key(&schema, &row.data, unique_cols) else {
                    continue;
                };
                if seen.insert(key, pk.clone()).is_some() {
                    return Err(anyhow!(
                        "unique constraint violation on {table}({})",
                        unique_cols.join(",")
                    ));
                }
            }
        }
        Ok(())
    }

    pub(super) fn find_conflict_keys(
        &self,
        table: &str,
        row_key: &str,
        data: &Map<String, Value>,
        table_rows: &BTreeMap<String, StoredRow>,
    ) -> BTreeSet<String> {
        let mut conflicts = BTreeSet::new();
        if table_rows.contains_key(row_key) {
            conflicts.insert(row_key.to_string());
        }

        if let Some(schema) = self.schemas.get(table).map(|s| s.clone()) {
            for unique_cols in &schema.unique {
                let Some(incoming) = schema_unique_key(&schema, data, unique_cols) else {
                    continue;
                };
                for (existing_key, existing) in table_rows {
                    if schema_unique_key(&schema, &existing.data, unique_cols).as_ref()
                        == Some(&incoming)
                    {
                        conflicts.insert(existing_key.clone());
                    }
                }
            }
        }

        conflicts
    }

    pub(super) fn next_auto_inc(&self, table: &str, column: &str) -> i64 {
        let key = format!("{table}:{column}");
        let mut slot = self.auto_inc.entry(key).or_insert(0);
        *slot += 1;
        *slot
    }

    pub(super) fn clear_auto_inc(&self, table: &str) {
        let prefix = format!("{table}:");
        let keys: Vec<String> = self
            .auto_inc
            .iter()
            .filter_map(|it| it.key().starts_with(&prefix).then(|| it.key().clone()))
            .collect();
        for key in keys {
            self.auto_inc.remove(&key);
        }
    }

    pub(super) fn rebuild_indexes_all(&self) {
        let tables: Vec<String> = self.rows.iter().map(|r| r.key().clone()).collect();
        for t in tables {
            self.rebuild_indexes(&t);
        }
    }

    pub(super) fn rebuild_indexes(&self, table: &str) {
        let Some(schema) = self.schemas.get(table).map(|s| s.clone()) else {
            return;
        };
        let Some(rows) = self.rows.get(table).map(|r| r.clone()) else {
            return;
        };
        let mut table_index: BTreeMap<String, BTreeMap<String, BTreeSet<String>>> = BTreeMap::new();

        for index in &schema.indexes {
            if index.columns.len() != 1 {
                continue;
            }
            let col = index.columns[0].clone();
            let mut map: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
            for (pk, row) in &rows {
                let view = self.current_schema_row(table, &row.data);
                let key = view.get(&col).cloned().unwrap_or(Value::Null).to_string();
                map.entry(key).or_default().insert(pk.clone());
            }
            table_index.insert(col, map);
        }
        self.indexes.insert(table.to_string(), table_index);
    }
}

#[derive(Clone)]
struct DeleteTarget {
    table: String,
    qualifier: String,
}

fn delete_source_aliases(sources: &[TableWithJoins]) -> Result<BTreeMap<String, DeleteTarget>> {
    let mut aliases = BTreeMap::new();
    for source in sources {
        add_delete_factor_alias(&source.relation, &mut aliases)?;
        for join in &source.joins {
            add_delete_factor_alias(&join.relation, &mut aliases)?;
        }
    }
    Ok(aliases)
}

fn add_delete_factor_alias(
    factor: &TableFactor,
    aliases: &mut BTreeMap<String, DeleteTarget>,
) -> Result<()> {
    let TableFactor::Table { name, alias, .. } = factor else {
        return Ok(());
    };
    let table = object_name(name)?;
    let qualifier = alias
        .as_ref()
        .map(|alias| alias.name.value.clone())
        .unwrap_or_else(|| table.clone());
    let target = DeleteTarget {
        table: table.clone(),
        qualifier: qualifier.clone(),
    };
    aliases.insert(table.to_ascii_lowercase(), target.clone());
    aliases.insert(qualifier.to_ascii_lowercase(), target);
    Ok(())
}

fn delete_target_from_factor(factor: &TableFactor) -> Result<DeleteTarget> {
    let TableFactor::Table { name, alias, .. } = factor else {
        return Err(anyhow!("DELETE target must be a base table"));
    };
    let table = object_name(name)?;
    let qualifier = alias
        .as_ref()
        .map(|alias| alias.name.value.clone())
        .unwrap_or_else(|| table.clone());
    Ok(DeleteTarget { table, qualifier })
}

fn merge_delete_context(
    left: &Map<String, Value>,
    right: &Map<String, Value>,
) -> Map<String, Value> {
    let mut combined = left.clone();
    for (column, value) in right {
        combined
            .entry(column.clone())
            .or_insert_with(|| value.clone());
    }
    combined
}

fn foreign_key_row_matches(
    child: &Map<String, Value>,
    child_columns: &[String],
    parent: &Map<String, Value>,
    parent_columns: &[String],
) -> bool {
    if child_columns.len() != parent_columns.len() || child_columns.is_empty() {
        return false;
    }
    child_columns
        .iter()
        .zip(parent_columns)
        .all(|(child_column, parent_column)| {
            let child_value = child.get(child_column).unwrap_or(&Value::Null);
            let parent_value = parent.get(parent_column).unwrap_or(&Value::Null);
            child_value != &Value::Null
                && parent_value != &Value::Null
                && mysql_eq(child_value, parent_value)
        })
}

fn schema_unique_key(
    schema: &TableSchemaHint,
    data: &Map<String, Value>,
    columns: &[String],
) -> Option<String> {
    let prefix_lengths = schema
        .indexes
        .iter()
        .find(|index| index.unique && index.columns == columns)
        .map(|index| index.prefix_lengths.as_slice())
        .unwrap_or(&[]);
    unique_key_with_prefixes(data, columns, prefix_lengths)
}

fn sort_delete_candidates(
    candidates: &mut [(String, StoredRow, Map<String, Value>)],
    order_by: &[OrderByExpr],
    schema: Option<&TableSchemaHint>,
) -> Result<()> {
    for item in order_by {
        validate_order_expr(&item.expr)?;
    }

    candidates.sort_by(|(_, _, left), (_, _, right)| {
        for item in order_by {
            let left_value = expr_resolved_value(&item.expr, left).unwrap_or(Value::Null);
            let right_value = expr_resolved_value(&item.expr, right).unwrap_or(Value::Null);
            let hint = order_column_hint_from_schema(schema, &item.expr);
            let ordering = compare_order_values(&left_value, &right_value, hint.as_ref());
            if ordering != Ordering::Equal {
                return if item.asc.unwrap_or(true) {
                    ordering
                } else {
                    ordering.reverse()
                };
            }
        }
        Ordering::Equal
    });

    Ok(())
}

fn order_column_hint_from_schema(
    schema: Option<&TableSchemaHint>,
    expr: &Expr,
) -> Option<ColumnHint> {
    let column = match expr {
        Expr::Identifier(identifier) => identifier.value.as_str(),
        Expr::CompoundIdentifier(parts) => parts.last()?.value.as_str(),
        _ => return None,
    };
    schema?
        .columns
        .iter()
        .find_map(|(known, hint)| known.eq_ignore_ascii_case(column).then(|| hint.clone()))
}
