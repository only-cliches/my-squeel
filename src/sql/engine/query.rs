use super::*;

impl Engine {
    pub(super) fn select_query(&self, mut query: Query) -> Result<QueryResult> {
        if query.with.is_some() {
            query = inline_common_table_expressions(query)?;
        }
        let order_by = query
            .order_by
            .map(|order_by| order_by.exprs)
            .unwrap_or_default();
        let limit = query.limit;
        let offset = query.offset;

        let result_columns: Vec<String>;
        let result_metadata: Vec<ColumnMetadata>;
        let mut rows = match &*query.body {
            SetExpr::Select(select) => {
                return self.select_from(
                    select.clone(),
                    &order_by,
                    limit.as_ref(),
                    offset.as_ref(),
                );
            }
            SetExpr::SetOperation {
                op,
                left,
                right,
                set_quantifier,
            } => {
                let left_query = Query {
                    body: left.clone(),
                    order_by: None,
                    limit: None,
                    offset: None,
                    fetch: None,
                    locks: vec![],
                    with: None,
                    for_clause: None,
                    format_clause: None,
                    limit_by: vec![],
                    settings: None,
                };
                let right_query = Query {
                    body: right.clone(),
                    order_by: None,
                    limit: None,
                    offset: None,
                    fetch: None,
                    locks: vec![],
                    with: None,
                    for_clause: None,
                    format_clause: None,
                    limit_by: vec![],
                    settings: None,
                };
                let mut left_result = self.select_query(left_query)?;
                let right_result = self.select_query(right_query)?;
                if left_result.columns.len() != right_result.columns.len() {
                    return Err(anyhow!(
                        "set operation branches return different column counts"
                    ));
                }
                result_columns = left_result.columns.clone();
                result_metadata = left_result.column_metadata.clone();
                let right_rows = right_result
                    .rows
                    .into_iter()
                    .map(|row| remap_set_row(&row, &right_result.columns, &left_result.columns))
                    .collect::<Result<Vec<_>>>()?;

                match op {
                    sqlparser::ast::SetOperator::Union => {
                        let should_dedup = *set_quantifier != sqlparser::ast::SetQuantifier::All;
                        if should_dedup {
                            let mut seen = BTreeSet::new();
                            left_result
                                .rows
                                .retain(|row| seen.insert(encode_json_row(row)));
                            for row in right_rows {
                                let row_key = encode_json_row(&row);
                                if seen.insert(row_key) {
                                    left_result.rows.push(row);
                                }
                            }
                        } else {
                            left_result.rows.extend(right_rows);
                        }
                        left_result.rows
                    }
                    sqlparser::ast::SetOperator::Intersect => {
                        let all = *set_quantifier == sqlparser::ast::SetQuantifier::All;
                        set_intersection(left_result.rows, right_rows, all)
                    }
                    sqlparser::ast::SetOperator::Except => {
                        let all = *set_quantifier == sqlparser::ast::SetQuantifier::All;
                        set_difference(left_result.rows, right_rows, all)
                    }
                }
            }
            _ => return Err(anyhow!("only SELECT and UNION are supported")),
        };

        apply_ordering(&mut rows, &order_by)?;
        apply_limit_offset(&mut rows, limit.as_ref(), offset.as_ref())?;

        Ok(QueryResult {
            rows_affected: 0,
            last_insert_id: 0,
            columns: result_columns,
            column_metadata: result_metadata,
            rows,
        })
    }

    pub(super) fn select_from(
        &self,
        select: Box<Select>,
        order_by: &[OrderByExpr],
        limit: Option<&Expr>,
        offset: Option<&Offset>,
    ) -> Result<QueryResult> {
        self.validate_select_column_references(&select, order_by)?;

        if select.from.is_empty() {
            let last_insert_id = self.last_insert_id.load(AtomicOrdering::Relaxed);
            let mut rows = Vec::new();
            let row = Map::new();
            if self.matches_selection_ctx(select.selection.as_ref(), &row, last_insert_id)? {
                rows.push(row);
            }
            if let Some(result) = aggregate_select_result(
                &select,
                rows.clone(),
                order_by,
                limit,
                offset,
                last_insert_id,
            )? {
                return Ok(self.with_select_metadata(&select, result));
            }

            return self.finish_select_rows(&select, rows, order_by, limit, offset, last_insert_id);
        }

        if select.from.is_empty() {
            // Already handled above
            unreachable!()
        }

        let root = &select.from[0];

        if select.from.len() > 1 {
            // Handle implicit cross-join (comma-separated FROM)
            let mut joined = Vec::new();
            let (first_table, first_alias) = table_factor_name_and_alias(&root.relation)?;
            if !self.schemas.contains_key(&first_table) {
                return Err(anyhow!("unknown table: {first_table}"));
            }
            let first_rows = self
                .rows
                .get(&first_table)
                .map(|r| r.clone())
                .unwrap_or_default();

            let mut current = Vec::new();
            for first_row in first_rows.values() {
                let first_data = self.current_schema_row(&first_table, &first_row.data);
                let mut first_map = first_data.clone();
                add_qualified_columns(&mut first_map, &first_table, &first_data);
                if let Some(alias) = &first_alias {
                    add_qualified_columns(&mut first_map, alias, &first_data);
                }

                current.push(first_map);
            }

            // Cross join each subsequent table
            for from_table in &select.from[1..] {
                let (table_name, alias) = table_factor_name_and_alias(&from_table.relation)?;
                if !self.schemas.contains_key(&table_name) {
                    return Err(anyhow!("unknown table: {table_name}"));
                }
                let table_rows = self
                    .rows
                    .get(&table_name)
                    .map(|r| r.clone())
                    .unwrap_or_default();

                let mut next = Vec::new();
                for candidate in &current {
                    for row in table_rows.values() {
                        let table_data = self.current_schema_row(&table_name, &row.data);
                        let mut combined = candidate.clone();
                        add_qualified_columns(&mut combined, &table_name, &table_data);
                        if let Some(ref a) = alias {
                            add_qualified_columns(&mut combined, a, &table_data);
                        }
                        for (k, v) in &table_data {
                            combined.entry(k.clone()).or_insert_with(|| v.clone());
                        }
                        next.push(combined);
                    }
                }
                current = next;
            }

            for c in current {
                if self.matches_selection_ctx(select.selection.as_ref(), &c, 0)? {
                    joined.push(c);
                }
            }

            let last_insert_id = self.last_insert_id.load(AtomicOrdering::Relaxed);
            if let Some(result) = aggregate_select_result(
                &select,
                joined.clone(),
                order_by,
                limit,
                offset,
                last_insert_id,
            )? {
                return Ok(self.with_select_metadata(&select, result));
            }

            return self.finish_select_rows(
                &select,
                joined,
                order_by,
                limit,
                offset,
                last_insert_id,
            );
        }

        let root = &select.from[0];
        if matches!(root.relation, TableFactor::Derived { .. }) {
            let rows = self.select_derived_rows(&select, root)?;
            let last_insert_id = self.last_insert_id.load(AtomicOrdering::Relaxed);
            if let Some(result) = aggregate_select_result(
                &select,
                rows.clone(),
                order_by,
                limit,
                offset,
                last_insert_id,
            )? {
                return Ok(self.with_select_metadata(&select, result));
            }
            return self.finish_select_rows(&select, rows, order_by, limit, offset, last_insert_id);
        }
        let root_name_full = table_factor_name_full(&root.relation)?;
        if root_name_full.eq_ignore_ascii_case("information_schema.tables") {
            return self.select_information_schema_tables(&select);
        }
        if root_name_full.eq_ignore_ascii_case("information_schema.schemata") {
            return self.select_information_schema_schemata(&select);
        }
        if root_name_full.eq_ignore_ascii_case("information_schema.columns") {
            return self.select_information_schema_columns(&select);
        }
        if root_name_full.eq_ignore_ascii_case("information_schema.table_constraints") {
            return self.select_information_schema_table_constraints(&select);
        }
        if root_name_full.eq_ignore_ascii_case("information_schema.statistics") {
            return self.select_information_schema_statistics(&select);
        }
        if root_name_full.eq_ignore_ascii_case("information_schema.key_column_usage") {
            return self.select_information_schema_key_column_usage(&select);
        }
        if root_name_full.eq_ignore_ascii_case("information_schema.referential_constraints") {
            return self.select_information_schema_referential_constraints(&select);
        }
        if root_name_full.eq_ignore_ascii_case("information_schema.character_sets") {
            return self.select_information_schema_character_sets(&select);
        }
        if root_name_full.eq_ignore_ascii_case("information_schema.collations") {
            return self.select_information_schema_collations(&select);
        }
        if root_name_full.eq_ignore_ascii_case("information_schema.views") {
            return self.select_information_schema_views(&select);
        }
        if root_name_full.eq_ignore_ascii_case("information_schema.routines") {
            return self.select_information_schema_routines(&select);
        }
        if root_name_full.eq_ignore_ascii_case("information_schema.engines") {
            return self.select_information_schema_engines(&select);
        }
        if root_name_full.eq_ignore_ascii_case("information_schema.processlist") {
            return self.select_information_schema_processlist(&select);
        }
        if root_name_full.eq_ignore_ascii_case("information_schema.session_variables") {
            return self.select_information_schema_session_variables(&select);
        }
        if root_name_full.eq_ignore_ascii_case("information_schema.global_variables") {
            return self.select_information_schema_global_variables(&select);
        }
        if root_name_full.eq_ignore_ascii_case("information_schema.keywords") {
            return self.select_information_schema_keywords(&select);
        }
        if root_name_full.eq_ignore_ascii_case("information_schema.triggers") {
            return self.select_information_schema_triggers(&select);
        }
        if root_name_full.eq_ignore_ascii_case("information_schema.check_constraints") {
            return self.select_information_schema_check_constraints(&select);
        }
        if root_name_full.eq_ignore_ascii_case("information_schema.files") {
            return self.select_information_schema_files(&select);
        }

        let rows = if root.joins.is_empty() {
            self.select_single_table(&select, root)?
        } else {
            self.select_with_joins(&select, root)?
        };

        let last_insert_id = self.last_insert_id.load(AtomicOrdering::Relaxed);
        if let Some(result) = aggregate_select_result(
            &select,
            rows.clone(),
            order_by,
            limit,
            offset,
            last_insert_id,
        )? {
            return Ok(self.with_select_metadata(&select, result));
        }

        self.finish_select_rows(&select, rows, order_by, limit, offset, last_insert_id)
    }

    fn finish_select_rows(
        &self,
        select: &Select,
        mut rows: Vec<Map<String, Value>>,
        order_by: &[OrderByExpr],
        limit: Option<&Expr>,
        offset: Option<&Offset>,
        last_insert_id: u64,
    ) -> Result<QueryResult> {
        self.materialize_window_values(select, &mut rows, last_insert_id)?;
        self.materialize_projection_values(&select.projection, &mut rows, last_insert_id)?;
        apply_ordering(&mut rows, order_by)?;
        let mut rows = rows
            .into_iter()
            .map(|row| self.project_row_ctx(&select.projection, &row, last_insert_id))
            .collect::<Result<Vec<_>>>()?;
        if select.distinct.is_some() {
            deduplicate_rows(&mut rows);
        }
        apply_limit_offset(&mut rows, limit, offset)?;

        let columns = self.select_result_columns(select, rows.first());
        let column_metadata = self.select_result_metadata(select, &columns, rows.first());
        Ok(QueryResult {
            rows_affected: 0,
            last_insert_id: 0,
            columns,
            column_metadata,
            rows,
        })
    }

    fn materialize_window_values(
        &self,
        select: &Select,
        rows: &mut [Map<String, Value>],
        last_insert_id: u64,
    ) -> Result<()> {
        if rows.is_empty() {
            return Ok(());
        }
        let snapshot = rows.to_vec();
        for item in &select.projection {
            let expr = match item {
                SelectItem::UnnamedExpr(expr) | SelectItem::ExprWithAlias { expr, .. } => expr,
                _ => continue,
            };
            let Expr::Function(function) = expr else {
                continue;
            };
            let Some(window) = &function.over else {
                continue;
            };
            let spec = resolve_window_spec(select, window)?;
            let function_name = function
                .name
                .0
                .last()
                .map(|name| name.value.to_ascii_uppercase())
                .unwrap_or_default();
            let arguments = window_function_arguments(function)?;

            let mut partitions = BTreeMap::<String, Vec<usize>>::new();
            for (index, row) in snapshot.iter().enumerate() {
                let key = spec
                    .partition_by
                    .iter()
                    .map(|expr| self.eval_expr_ctx(expr, row, last_insert_id))
                    .collect::<Result<Vec<_>>>()?
                    .iter()
                    .map(encode_json_value)
                    .collect::<Vec<_>>()
                    .join("|");
                partitions.entry(key).or_default().push(index);
            }

            let mut values = vec![Value::Null; rows.len()];
            for partition in partitions.values_mut() {
                let order_keys = partition
                    .iter()
                    .map(|index| {
                        spec.order_by
                            .iter()
                            .map(|order| {
                                self.eval_expr_ctx(&order.expr, &snapshot[*index], last_insert_id)
                            })
                            .collect::<Result<Vec<_>>>()
                    })
                    .collect::<Result<Vec<_>>>()?;
                let key_by_index = partition
                    .iter()
                    .copied()
                    .zip(order_keys)
                    .collect::<BTreeMap<_, _>>();
                partition.sort_by(|left, right| {
                    for (position, order) in spec.order_by.iter().enumerate() {
                        let left_value = &key_by_index[left][position];
                        let right_value = &key_by_index[right][position];
                        let ordering = compare_json_values(left_value, right_value);
                        if ordering != Ordering::Equal {
                            return if order.asc.unwrap_or(true) {
                                ordering
                            } else {
                                ordering.reverse()
                            };
                        }
                    }
                    left.cmp(right)
                });

                let mut rank = 1_usize;
                let mut dense_rank = 1_usize;
                for position in 0..partition.len() {
                    if position > 0
                        && key_by_index[&partition[position]]
                            != key_by_index[&partition[position - 1]]
                    {
                        rank = position + 1;
                        dense_rank += 1;
                    }
                    let row_index = partition[position];
                    let mut peer_start = position;
                    while peer_start > 0
                        && key_by_index[&partition[peer_start - 1]]
                            == key_by_index[&partition[position]]
                    {
                        peer_start -= 1;
                    }
                    let mut peer_end = position;
                    while peer_end + 1 < partition.len()
                        && key_by_index[&partition[peer_end + 1]]
                            == key_by_index[&partition[position]]
                    {
                        peer_end += 1;
                    }
                    let frame = window_frame_positions(
                        &spec,
                        position,
                        partition.len(),
                        !spec.order_by.is_empty(),
                        peer_start,
                        peer_end,
                    )?;
                    values[row_index] = self.window_function_value(
                        &function_name,
                        &arguments,
                        partition,
                        position,
                        frame,
                        rank,
                        dense_rank,
                        peer_end,
                        &snapshot,
                        last_insert_id,
                    )?;
                }
            }

            let key = projection_expr_column_name(expr);
            for (row, value) in rows.iter_mut().zip(values) {
                row.insert(key.clone(), value);
            }
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn window_function_value(
        &self,
        function: &str,
        arguments: &[Option<Expr>],
        partition: &[usize],
        position: usize,
        frame: Option<(usize, usize)>,
        rank: usize,
        dense_rank: usize,
        peer_end: usize,
        rows: &[Map<String, Value>],
        last_insert_id: u64,
    ) -> Result<Value> {
        let integer = |value: usize| Value::Number(Number::from(value as u64));
        match function {
            "ROW_NUMBER" => return Ok(integer(position + 1)),
            "RANK" => return Ok(integer(rank)),
            "DENSE_RANK" => return Ok(integer(dense_rank)),
            "PERCENT_RANK" => {
                return Ok(number_from_f64(if partition.len() <= 1 {
                    0.0
                } else {
                    (rank - 1) as f64 / (partition.len() - 1) as f64
                }));
            }
            "CUME_DIST" => {
                return Ok(number_from_f64(
                    (peer_end + 1) as f64 / partition.len() as f64,
                ));
            }
            "NTILE" => {
                let buckets = window_usize_argument(
                    self,
                    arguments.first(),
                    &rows[partition[position]],
                    last_insert_id,
                    0,
                )?;
                if buckets == 0 {
                    return Err(anyhow!("NTILE requires a positive bucket count"));
                }
                let quotient = partition.len() / buckets;
                let remainder = partition.len() % buckets;
                let first_span = (quotient + 1) * remainder;
                let bucket = if position < first_span {
                    position / (quotient + 1) + 1
                } else {
                    remainder + (position - first_span) / quotient + 1
                };
                return Ok(integer(bucket));
            }
            "LAG" | "LEAD" => {
                let offset = window_usize_argument(
                    self,
                    arguments.get(1),
                    &rows[partition[position]],
                    last_insert_id,
                    1,
                )?;
                let target = if function == "LAG" {
                    position.checked_sub(offset)
                } else {
                    position
                        .checked_add(offset)
                        .filter(|index| *index < partition.len())
                };
                if let Some(target) = target {
                    return eval_window_argument(
                        self,
                        arguments.first(),
                        &rows[partition[target]],
                        last_insert_id,
                    );
                }
                return eval_window_argument(
                    self,
                    arguments.get(2),
                    &rows[partition[position]],
                    last_insert_id,
                );
            }
            _ => {}
        }

        let frame_rows = frame
            .map(|(start, end)| &partition[start..end])
            .unwrap_or(&[]);
        match function {
            "FIRST_VALUE" => frame_rows
                .first()
                .map(|index| {
                    eval_window_argument(self, arguments.first(), &rows[*index], last_insert_id)
                })
                .transpose()
                .map(|value| value.unwrap_or(Value::Null)),
            "LAST_VALUE" => frame_rows
                .last()
                .map(|index| {
                    eval_window_argument(self, arguments.first(), &rows[*index], last_insert_id)
                })
                .transpose()
                .map(|value| value.unwrap_or(Value::Null)),
            "NTH_VALUE" => {
                let nth = window_usize_argument(
                    self,
                    arguments.get(1),
                    &rows[partition[position]],
                    last_insert_id,
                    0,
                )?;
                frame_rows
                    .get(nth.saturating_sub(1))
                    .map(|index| {
                        eval_window_argument(self, arguments.first(), &rows[*index], last_insert_id)
                    })
                    .transpose()
                    .map(|value| value.unwrap_or(Value::Null))
            }
            "COUNT" | "SUM" | "AVG" | "MIN" | "MAX" => {
                let mut aggregate_values = Vec::new();
                for index in frame_rows {
                    let value = if arguments.is_empty()
                        || arguments.first().is_some_and(|argument| argument.is_none())
                    {
                        Value::Number(Number::from(1))
                    } else {
                        eval_window_argument(
                            self,
                            arguments.first(),
                            &rows[*index],
                            last_insert_id,
                        )?
                    };
                    if value != Value::Null {
                        aggregate_values.push(value);
                    }
                }
                match function {
                    "COUNT" => Ok(integer(aggregate_values.len())),
                    "SUM" | "AVG" if aggregate_values.is_empty() => Ok(Value::Null),
                    "SUM" => Ok(number_from_f64(
                        aggregate_values
                            .iter()
                            .map(json_to_f64_lossy)
                            .try_fold(0.0, |sum, value| value.map(|value| sum + value))?,
                    )),
                    "AVG" => {
                        let sum = aggregate_values
                            .iter()
                            .map(json_to_f64_lossy)
                            .try_fold(0.0, |sum, value| value.map(|value| sum + value))?;
                        Ok(number_from_f64(sum / aggregate_values.len() as f64))
                    }
                    "MIN" => Ok(aggregate_values
                        .into_iter()
                        .min_by(compare_json_values)
                        .unwrap_or(Value::Null)),
                    "MAX" => Ok(aggregate_values
                        .into_iter()
                        .max_by(compare_json_values)
                        .unwrap_or(Value::Null)),
                    _ => unreachable!(),
                }
            }
            _ => Err(anyhow!("unsupported window function: {function}")),
        }
    }

    fn with_select_metadata(&self, select: &Select, mut result: QueryResult) -> QueryResult {
        if result.columns.is_empty() {
            result.columns = self.select_result_columns(select, result.rows.first());
        }
        result.column_metadata =
            self.select_result_metadata(select, &result.columns, result.rows.first());
        result
    }

    fn select_result_columns(
        &self,
        select: &Select,
        first_row: Option<&Map<String, Value>>,
    ) -> Vec<String> {
        let mut columns = Vec::new();
        for item in &select.projection {
            match item {
                SelectItem::UnnamedExpr(expr) => {
                    columns.push(projection_output_column_name(expr));
                }
                SelectItem::ExprWithAlias { alias, .. } => columns.push(alias.value.clone()),
                SelectItem::Wildcard(_) => {
                    for table in &select.from {
                        self.append_factor_columns(&table.relation, None, &mut columns);
                        for join in &table.joins {
                            self.append_factor_columns(&join.relation, None, &mut columns);
                        }
                    }
                }
                SelectItem::QualifiedWildcard(prefix, _) => {
                    let qualifier = prefix
                        .0
                        .iter()
                        .map(|part| part.value.as_str())
                        .collect::<Vec<_>>()
                        .join(".");
                    for table in &select.from {
                        self.append_factor_columns(&table.relation, Some(&qualifier), &mut columns);
                        for join in &table.joins {
                            self.append_factor_columns(
                                &join.relation,
                                Some(&qualifier),
                                &mut columns,
                            );
                        }
                    }
                }
            }
        }
        if columns.is_empty() {
            columns.extend(first_row.into_iter().flat_map(|row| row.keys().cloned()));
        }
        columns
    }

    fn append_factor_columns(
        &self,
        factor: &TableFactor,
        qualifier: Option<&str>,
        columns: &mut Vec<String>,
    ) {
        let TableFactor::Table { name, alias, .. } = factor else {
            return;
        };
        let Some(table) = name.0.last().map(|name| name.value.clone()) else {
            return;
        };
        if qualifier.is_some_and(|qualifier| {
            !qualifier.eq_ignore_ascii_case(&table)
                && !alias
                    .as_ref()
                    .is_some_and(|alias| qualifier.eq_ignore_ascii_case(&alias.name.value))
        }) {
            return;
        }
        if let Some(schema) = self.schemas.get(&table) {
            columns.extend(ordered_schema_columns(&schema));
        }
    }

    fn select_result_metadata(
        &self,
        select: &Select,
        columns: &[String],
        first_row: Option<&Map<String, Value>>,
    ) -> Vec<ColumnMetadata> {
        let mut metadata = Vec::new();
        for item in &select.projection {
            match item {
                SelectItem::UnnamedExpr(expr) => metadata.push(self.expression_metadata(
                    select,
                    expr,
                    projection_output_column_name(expr),
                    first_row,
                )),
                SelectItem::ExprWithAlias { expr, alias } => metadata
                    .push(self.expression_metadata(select, expr, alias.value.clone(), first_row)),
                SelectItem::Wildcard(_) => {
                    for table in &select.from {
                        self.append_factor_metadata(&table.relation, None, &mut metadata);
                        for join in &table.joins {
                            self.append_factor_metadata(&join.relation, None, &mut metadata);
                        }
                    }
                }
                SelectItem::QualifiedWildcard(prefix, _) => {
                    let qualifier = prefix
                        .0
                        .iter()
                        .map(|part| part.value.as_str())
                        .collect::<Vec<_>>()
                        .join(".");
                    for table in &select.from {
                        self.append_factor_metadata(
                            &table.relation,
                            Some(&qualifier),
                            &mut metadata,
                        );
                        for join in &table.joins {
                            self.append_factor_metadata(
                                &join.relation,
                                Some(&qualifier),
                                &mut metadata,
                            );
                        }
                    }
                }
            }
        }
        if metadata.len() != columns.len() {
            return columns
                .iter()
                .map(|name| {
                    ColumnMetadata::from_value(name, first_row.and_then(|row| row.get(name)))
                })
                .collect();
        }
        for (column, meta) in columns.iter().zip(&mut metadata) {
            meta.name = column.clone();
        }
        metadata
    }

    fn append_factor_metadata(
        &self,
        factor: &TableFactor,
        qualifier: Option<&str>,
        metadata: &mut Vec<ColumnMetadata>,
    ) {
        let TableFactor::Table { name, alias, .. } = factor else {
            return;
        };
        let Some(table) = name.0.last().map(|name| name.value.clone()) else {
            return;
        };
        if qualifier.is_some_and(|qualifier| {
            !qualifier.eq_ignore_ascii_case(&table)
                && !alias
                    .as_ref()
                    .is_some_and(|alias| qualifier.eq_ignore_ascii_case(&alias.name.value))
        }) {
            return;
        }
        if let Some(schema) = self.schemas.get(&table) {
            for column in ordered_schema_columns(&schema) {
                if let Some(hint) = schema.columns.get(&column) {
                    metadata.push(ColumnMetadata::from_declared(&column, &table, hint));
                }
            }
        }
    }

    fn expression_metadata(
        &self,
        select: &Select,
        expr: &Expr,
        output_name: String,
        first_row: Option<&Map<String, Value>>,
    ) -> ColumnMetadata {
        if let Some((table, hint)) = self.resolve_expression_column(select, expr) {
            let mut metadata = ColumnMetadata::from_declared(output_name, table, &hint);
            if select_nullable_tables(select)
                .iter()
                .any(|table| table.eq_ignore_ascii_case(&metadata.table))
            {
                metadata.nullable = true;
            }
            if !matches!(expr, Expr::Identifier(_) | Expr::CompoundIdentifier(_)) {
                metadata.table.clear();
            }
            return metadata;
        }

        let mut metadata = ColumnMetadata::from_value(
            output_name.clone(),
            first_row.and_then(|row| row.get(&output_name)),
        );
        match expr {
            Expr::Cast { data_type, .. } => {
                let hint = ColumnHint {
                    sql_type: Some(data_type.to_string()),
                    ..ColumnHint::default()
                };
                metadata = ColumnMetadata::from_declared(output_name, "", &hint);
            }
            Expr::Function(function) => {
                let name = function
                    .name
                    .0
                    .last()
                    .map(|name| name.value.to_ascii_uppercase())
                    .unwrap_or_default();
                metadata.column_type = match name.as_str() {
                    "COUNT" | "ROW_NUMBER" | "RANK" | "DENSE_RANK" | "NTILE" => {
                        metadata.unsigned = true;
                        MysqlColumnType::BigInt
                    }
                    "PERCENT_RANK" | "CUME_DIST" => MysqlColumnType::Double,
                    "AVG" | "SUM" => {
                        metadata.decimals = if name == "AVG" { 4 } else { 0 };
                        MysqlColumnType::Decimal
                    }
                    "ROUND" | "TRUNCATE" => MysqlColumnType::Decimal,
                    "CURRENT_DATE" | "CURDATE" | "DATE" => MysqlColumnType::Date,
                    "CURRENT_TIME" | "CURTIME" | "TIME" => MysqlColumnType::Time,
                    "NOW" | "CURRENT_TIMESTAMP" | "STR_TO_DATE" => MysqlColumnType::DateTime,
                    "JSON_OBJECT" | "JSON_ARRAY" | "JSON_EXTRACT" => MysqlColumnType::Json,
                    "LENGTH" | "CHAR_LENGTH" | "DATEDIFF" | "TIMESTAMPDIFF" => {
                        MysqlColumnType::BigInt
                    }
                    _ => metadata.column_type,
                };
            }
            Expr::Value(SqlValue::Number(number, _)) => {
                metadata.column_type = if number.contains(['.', 'e', 'E']) {
                    MysqlColumnType::Decimal
                } else {
                    MysqlColumnType::BigInt
                };
                if let Some((_, scale)) = number.split_once('.') {
                    metadata.decimals = scale.len().min(u8::MAX as usize) as u8;
                }
            }
            Expr::Value(SqlValue::Boolean(_)) => metadata.column_type = MysqlColumnType::TinyInt,
            Expr::Value(SqlValue::Null) => metadata.column_type = MysqlColumnType::Null,
            _ => {}
        }
        metadata
    }

    fn resolve_expression_column(
        &self,
        select: &Select,
        expr: &Expr,
    ) -> Option<(String, ColumnHint)> {
        let (qualifier, column) = match expr {
            Expr::Identifier(column) => (None, column.value.as_str()),
            Expr::CompoundIdentifier(parts) if parts.len() >= 2 => (
                parts.get(parts.len() - 2).map(|part| part.value.as_str()),
                parts.last()?.value.as_str(),
            ),
            _ => return None,
        };
        for table in &select.from {
            for factor in std::iter::once(&table.relation)
                .chain(table.joins.iter().map(|join| &join.relation))
            {
                let TableFactor::Table { name, alias, .. } = factor else {
                    continue;
                };
                let table_name = name.0.last()?.value.clone();
                if qualifier.is_some_and(|qualifier| {
                    !qualifier.eq_ignore_ascii_case(&table_name)
                        && !alias
                            .as_ref()
                            .is_some_and(|alias| qualifier.eq_ignore_ascii_case(&alias.name.value))
                }) {
                    continue;
                }
                let schema = self.schemas.get(&table_name)?;
                if let Some((_, hint)) = schema
                    .columns
                    .iter()
                    .find(|(known, _)| known.eq_ignore_ascii_case(column))
                {
                    return Some((table_name, hint.clone()));
                }
            }
        }
        None
    }

    fn validate_select_column_references(
        &self,
        select: &Select,
        order_by: &[OrderByExpr],
    ) -> Result<()> {
        if select.from.iter().any(|table| {
            matches!(table.relation, TableFactor::Derived { .. })
                || table
                    .joins
                    .iter()
                    .any(|join| matches!(join.relation, TableFactor::Derived { .. }))
        }) {
            // Derived columns are known only after their subquery is evaluated.
            return Ok(());
        }
        if select.from.first().is_some_and(|table| {
            table_factor_name_full(&table.relation)
                .is_ok_and(|name| name.to_ascii_lowercase().starts_with("information_schema."))
        }) {
            return Ok(());
        }

        let mut scope = ColumnScope::default();
        for table in &select.from {
            self.add_table_to_column_scope(&table.relation, &mut scope)?;
            for join in &table.joins {
                self.add_table_to_column_scope(&join.relation, &mut scope)?;
            }
        }

        for item in &select.projection {
            if let SelectItem::UnnamedExpr(expr) | SelectItem::ExprWithAlias { expr, .. } = item {
                validate_expr_columns(expr, &scope)?;
            }
        }
        if let Some(selection) = &select.selection {
            validate_expr_columns(selection, &scope)?;
        }

        let mut alias_scope = scope.clone();
        for item in &select.projection {
            match item {
                SelectItem::ExprWithAlias { alias, .. } => {
                    alias_scope.aliases.insert(alias.value.to_ascii_lowercase());
                }
                SelectItem::UnnamedExpr(expr) => {
                    alias_scope
                        .aliases
                        .insert(projection_expr_column_name(expr).to_ascii_lowercase());
                }
                _ => {}
            }
        }
        for expr in group_by_exprs(select) {
            validate_expr_columns(&expr, &alias_scope)?;
        }
        if let Some(having) = &select.having {
            validate_expr_columns(having, &alias_scope)?;
        }
        for order in order_by {
            validate_expr_columns(&order.expr, &alias_scope)?;
        }
        Ok(())
    }

    fn add_table_to_column_scope(
        &self,
        factor: &TableFactor,
        scope: &mut ColumnScope,
    ) -> Result<()> {
        let (table, alias) = table_factor_name_and_alias(factor)?;
        let Some(schema) = self.schemas.get(&table).map(|schema| schema.clone()) else {
            return Ok(());
        };
        let mut columns = ordered_schema_columns(&schema)
            .into_iter()
            .map(|column| column.to_ascii_lowercase())
            .collect::<BTreeSet<_>>();
        // A declared schema is authoritative for name binding. Historical
        // row fields remain available to drift tooling but, like MySQL
        // columns removed by ALTER TABLE, cannot be selected afterward.
        if columns.is_empty()
            && let Some(rows) = self.rows.get(&table)
        {
            for row in rows.values() {
                columns.extend(row.data.keys().map(|column| column.to_ascii_lowercase()));
            }
        }
        for column in columns {
            *scope.unqualified.entry(column.clone()).or_default() += 1;
            scope
                .qualified
                .insert(format!("{}.{}", table.to_ascii_lowercase(), column));
            if let Some(alias) = &alias {
                scope
                    .qualified
                    .insert(format!("{}.{}", alias.to_ascii_lowercase(), column));
            }
        }
        Ok(())
    }

    pub(super) fn select_single_table(
        &self,
        select: &Select,
        root: &TableWithJoins,
    ) -> Result<Vec<Map<String, Value>>> {
        let (table, alias) = table_factor_name_and_alias(&root.relation)?;
        if !self.schemas.contains_key(&table) {
            return Err(anyhow!("unknown table: {table}"));
        }
        let filter = select.selection.as_ref();
        let mut rows = Vec::new();

        if let Some(index_hit) = try_index_lookup(filter, &table)
            && let Some(index_rows) = self
                .indexes
                .get(&table)
                .and_then(|idx| idx.get(&index_hit.0).cloned())
            && let Some(keys) = index_rows.get(&index_hit.1)
            && let Some(table_rows) = self.rows.get(&table)
        {
            for key in keys {
                if let Some(row) = table_rows.get(key) {
                    let data = self.current_schema_row(&table, &row.data);
                    let mut view = data.clone();
                    add_qualified_columns(&mut view, &table, &data);
                    if let Some(alias) = &alias {
                        add_qualified_columns(&mut view, alias, &data);
                    }
                    if self.matches_selection_ctx(filter, &view, 0)? {
                        rows.push(view);
                    }
                }
            }
            return Ok(rows);
        }

        if let Some(table_rows) = self.rows.get(&table) {
            for row in table_rows.values() {
                let data = self.current_schema_row(&table, &row.data);
                let mut view = data.clone();
                add_qualified_columns(&mut view, &table, &data);
                if let Some(alias) = &alias {
                    add_qualified_columns(&mut view, alias, &data);
                }
                if !self.matches_selection_ctx(filter, &view, 0)? {
                    continue;
                }
                rows.push(view);
            }
        }
        Ok(rows)
    }

    pub(super) fn select_with_joins(
        &self,
        select: &Select,
        root: &TableWithJoins,
    ) -> Result<Vec<Map<String, Value>>> {
        self.joined_table_factor_rows(root, select.selection.as_ref())
    }

    pub(super) fn joined_table_factor_rows(
        &self,
        root: &TableWithJoins,
        selection: Option<&Expr>,
    ) -> Result<Vec<Map<String, Value>>> {
        let left = self.rows_for_table_factor(&root.relation)?;
        let mut current = left.rows;
        let mut current_nulls = left.nulls;

        for join in &root.joins {
            let right = self.rows_for_table_factor(&join.relation)?;
            let mut next = Vec::new();
            let mut matched_right = vec![false; right.rows.len()];

            for candidate in &current {
                let mut matched = false;
                for (right_index, right_row) in right.rows.iter().enumerate() {
                    let combined = merge_join_rows(candidate, right_row);
                    if self.join_factor_matches(
                        &join.join_operator,
                        candidate,
                        right_row,
                        &combined,
                    )? {
                        matched = true;
                        matched_right[right_index] = true;
                        next.push(combined);
                    }
                }
                if !matched && matches!(join.join_operator, JoinOperator::LeftOuter(_)) {
                    next.push(merge_join_rows(candidate, &right.nulls));
                }
            }

            if matches!(join.join_operator, JoinOperator::RightOuter(_)) {
                for (matched, right_row) in matched_right.into_iter().zip(&right.rows) {
                    if !matched {
                        next.push(merge_join_rows(&current_nulls, right_row));
                    }
                }
            }

            current_nulls = merge_join_rows(&current_nulls, &right.nulls);
            current = next;
        }

        current
            .into_iter()
            .filter_map(|row| match self.matches_selection_ctx(selection, &row, 0) {
                Ok(true) => Some(Ok(row)),
                Ok(false) => None,
                Err(error) => Some(Err(error)),
            })
            .collect()
    }

    fn rows_for_table_factor(&self, factor: &TableFactor) -> Result<TableFactorRows> {
        match factor {
            TableFactor::Table { name, alias, .. } => {
                let table = object_name(name)?;
                if !self.schemas.contains_key(&table) {
                    return Err(anyhow!("unknown table: {table}"));
                }
                let alias_name = alias.as_ref().map(|alias| alias.name.value.as_str());
                let mut rows = Vec::new();
                if let Some(stored_rows) = self.rows.get(&table) {
                    for stored in stored_rows.values() {
                        let raw = self.current_schema_row(&table, &stored.data);
                        rows.push(qualified_factor_row(&raw, &table, alias_name));
                    }
                }
                let raw_nulls = self.current_schema_null_row(&table);
                Ok(TableFactorRows {
                    rows,
                    nulls: qualified_factor_row(&raw_nulls, &table, alias_name),
                })
            }
            TableFactor::Derived {
                subquery, alias, ..
            } => {
                let alias = alias
                    .as_ref()
                    .ok_or_else(|| anyhow!("every derived table must have its own alias"))?;
                let result = self.select_query((**subquery).clone())?;
                if !alias.columns.is_empty() && alias.columns.len() != result.columns.len() {
                    return Err(anyhow!("derived table column alias count does not match"));
                }
                let output_columns = if alias.columns.is_empty() {
                    result.columns.clone()
                } else {
                    alias
                        .columns
                        .iter()
                        .map(|column| column.name.value.clone())
                        .collect()
                };
                let mut rows = Vec::new();
                for result_row in result.rows {
                    let raw = remap_set_row(&result_row, &result.columns, &output_columns)?;
                    rows.push(qualified_factor_row(&raw, &alias.name.value, None));
                }
                let raw_nulls = output_columns
                    .iter()
                    .map(|column| (column.clone(), Value::Null))
                    .collect();
                Ok(TableFactorRows {
                    rows,
                    nulls: qualified_factor_row(&raw_nulls, &alias.name.value, None),
                })
            }
            _ => Err(anyhow!("unsupported table factor")),
        }
    }

    fn join_factor_matches(
        &self,
        operator: &JoinOperator,
        left: &Map<String, Value>,
        right: &Map<String, Value>,
        combined: &Map<String, Value>,
    ) -> Result<bool> {
        let constraint = match operator {
            JoinOperator::Inner(constraint)
            | JoinOperator::LeftOuter(constraint)
            | JoinOperator::RightOuter(constraint) => constraint,
            JoinOperator::CrossJoin => return Ok(true),
            _ => return Err(anyhow!("unsupported join type")),
        };
        match constraint {
            JoinConstraint::On(expr) => self.matches_selection_ctx(Some(expr), combined, 0),
            JoinConstraint::Using(columns) => {
                for column in columns {
                    let left_value = unqualified_row_value(left, &column.value)
                        .ok_or_else(|| anyhow!("unknown column: {}", column.value))?;
                    let right_value = unqualified_row_value(right, &column.value)
                        .ok_or_else(|| anyhow!("unknown column: {}", column.value))?;
                    if !mysql_eq(left_value, right_value)
                        || left_value == &Value::Null
                        || right_value == &Value::Null
                    {
                        return Ok(false);
                    }
                }
                Ok(true)
            }
            JoinConstraint::Natural => {
                let shared = left
                    .keys()
                    .filter(|column| !column.contains('.'))
                    .filter(|column| unqualified_row_value(right, column).is_some())
                    .cloned()
                    .collect::<Vec<_>>();
                for column in shared {
                    let left_value = unqualified_row_value(left, &column).unwrap_or(&Value::Null);
                    let right_value = unqualified_row_value(right, &column).unwrap_or(&Value::Null);
                    if left_value == &Value::Null
                        || right_value == &Value::Null
                        || !mysql_eq(left_value, right_value)
                    {
                        return Ok(false);
                    }
                }
                Ok(true)
            }
            JoinConstraint::None => Ok(true),
        }
    }

    pub(super) fn current_schema_row(
        &self,
        table: &str,
        data: &Map<String, Value>,
    ) -> Map<String, Value> {
        let Some(schema) = self.schemas.get(table).map(|schema| schema.clone()) else {
            return data.clone();
        };
        if schema.columns.is_empty() {
            return data.clone();
        }

        let mut out = Map::new();
        for column in ordered_schema_columns(&schema) {
            let Some(hint) = schema.columns.get(&column) else {
                continue;
            };
            let value = data
                .get(&column)
                .cloned()
                .or_else(|| read_default_value(hint))
                .unwrap_or(Value::Null);
            out.insert(column, coerce_value_for_column(value, hint));
        }
        for column in ordered_schema_columns(&schema) {
            let Some(expression) = schema
                .columns
                .get(&column)
                .and_then(|hint| hint.generated.as_deref())
            else {
                continue;
            };
            if let Some(expression) = parse_scalar_expr(expression)
                && let Ok(value) = self.eval_expr_ctx(&expression, &out, 0)
            {
                out.insert(column, value);
            }
        }
        for column in data.keys() {
            if !schema
                .columns
                .keys()
                .any(|known| known.eq_ignore_ascii_case(column))
            {
                out.insert(historical_column_marker(column), Value::Null);
            }
        }
        out
    }

    pub(super) fn current_schema_null_row(&self, table: &str) -> Map<String, Value> {
        let Some(schema) = self.schemas.get(table).map(|schema| schema.clone()) else {
            return Map::new();
        };

        let mut row = ordered_schema_columns(&schema)
            .into_iter()
            .map(|column| (column, Value::Null))
            .collect::<Map<_, _>>();
        if let Some(rows) = self.rows.get(table) {
            for stored in rows.values() {
                for column in stored.data.keys() {
                    if !schema
                        .columns
                        .keys()
                        .any(|known| known.eq_ignore_ascii_case(column))
                    {
                        row.insert(historical_column_marker(column), Value::Null);
                    }
                }
            }
        }
        row
    }

    pub(super) fn materialize_projection_values(
        &self,
        projection: &[SelectItem],
        rows: &mut [Map<String, Value>],
        last_insert_id: u64,
    ) -> Result<()> {
        for row in rows {
            for item in projection {
                match item {
                    SelectItem::UnnamedExpr(expr) => {
                        let column = projection_expr_column_name(expr);
                        if !row.contains_key(&column) {
                            row.insert(column, self.eval_expr_ctx(expr, row, last_insert_id)?);
                        }
                    }
                    SelectItem::ExprWithAlias { expr, alias }
                        if !row.contains_key(&alias.value) =>
                    {
                        row.insert(
                            alias.value.clone(),
                            self.eval_expr_ctx(expr, row, last_insert_id)?,
                        );
                    }
                    _ => {}
                }
            }
        }
        Ok(())
    }

    pub(super) fn select_derived_rows(
        &self,
        select: &Select,
        root: &TableWithJoins,
    ) -> Result<Vec<Map<String, Value>>> {
        self.joined_table_factor_rows(root, select.selection.as_ref())
    }

    pub(super) fn project_row_ctx(
        &self,
        projection: &[SelectItem],
        data: &Map<String, Value>,
        last_insert_id: u64,
    ) -> Result<Map<String, Value>> {
        project_row_with(projection, data, |expr| {
            self.eval_expr_ctx(expr, data, last_insert_id)
        })
    }

    pub(super) fn eval_expr_ctx(
        &self,
        expr: &Expr,
        data: &Map<String, Value>,
        last_insert_id: u64,
    ) -> Result<Value> {
        if let Some(value) = data.get(&projection_expr_column_name(expr)) {
            return Ok(value.clone());
        }
        if let Some(value) = system_variable_expr_value(expr) {
            return Ok(value);
        }

        match expr {
            Expr::Subquery(query) => {
                reject_correlated_subquery(query)?;
                self.eval_scalar_subquery(query)
            }
            Expr::Exists { subquery, negated } => {
                reject_correlated_subquery(subquery)?;
                let exists = !self.select_query((**subquery).clone())?.rows.is_empty();
                Ok(Value::Bool(if *negated { !exists } else { exists }))
            }
            Expr::InSubquery {
                expr,
                subquery,
                negated,
            } => {
                reject_correlated_subquery(subquery)?;
                let value = self.eval_expr_ctx(expr, data, last_insert_id)?;
                let result = self.select_query((**subquery).clone())?;
                let candidates = result
                    .rows
                    .iter()
                    .map(|row| first_projected_value(row, &result.columns).unwrap_or(Value::Null))
                    .collect();
                Ok(eval_in_values(value, candidates, *negated))
            }
            Expr::Nested(expr) => self.eval_expr_ctx(expr, data, last_insert_id),
            Expr::UnaryOp { op, expr } if op.to_string() == "-" => {
                let value = self.eval_expr_ctx(expr, data, last_insert_id)?;
                if value == Value::Null {
                    return Ok(Value::Null);
                }
                Ok(number_from_f64(-json_to_f64_lossy(&value)?))
            }
            Expr::UnaryOp { op, expr } if op.to_string() == "+" => {
                let value = self.eval_expr_ctx(expr, data, last_insert_id)?;
                if value == Value::Null {
                    Ok(Value::Null)
                } else {
                    Ok(number_from_f64(json_to_f64_lossy(&value)?))
                }
            }
            Expr::UnaryOp { op, expr }
                if op.to_string().eq_ignore_ascii_case("NOT") || op.to_string() == "!" =>
            {
                Ok(sql_not_value(self.eval_expr_ctx(
                    expr,
                    data,
                    last_insert_id,
                )?))
            }
            Expr::UnaryOp { op, expr } if op.to_string() == "~" => {
                let value = self.eval_expr_ctx(expr, data, last_insert_id)?;
                if value == Value::Null {
                    Ok(Value::Null)
                } else {
                    Ok(Value::Number(Number::from(
                        !(json_to_f64_lossy(&value)? as i64),
                    )))
                }
            }
            Expr::UnaryOp { op, .. } => Err(anyhow!("unsupported unary operator: {op}")),
            Expr::BinaryOp { left, op, right } => {
                let left_value = self.eval_expr_ctx(left, data, last_insert_id)?;
                let right_value = self.eval_expr_ctx(right, data, last_insert_id)?;
                eval_binary_values(left_value, op, right_value)
            }
            Expr::IsTrue(expr) => Ok(Value::Bool(matches!(
                sql_truth(&self.eval_expr_ctx(expr, data, last_insert_id)?),
                SqlTruth::True
            ))),
            Expr::IsNotTrue(expr) => Ok(Value::Bool(!matches!(
                sql_truth(&self.eval_expr_ctx(expr, data, last_insert_id)?),
                SqlTruth::True
            ))),
            Expr::IsFalse(expr) => Ok(Value::Bool(matches!(
                sql_truth(&self.eval_expr_ctx(expr, data, last_insert_id)?),
                SqlTruth::False
            ))),
            Expr::IsNotFalse(expr) => Ok(Value::Bool(!matches!(
                sql_truth(&self.eval_expr_ctx(expr, data, last_insert_id)?),
                SqlTruth::False
            ))),
            Expr::IsUnknown(expr) => Ok(Value::Bool(
                self.eval_expr_ctx(expr, data, last_insert_id)? == Value::Null,
            )),
            Expr::IsNotUnknown(expr) => Ok(Value::Bool(
                self.eval_expr_ctx(expr, data, last_insert_id)? != Value::Null,
            )),
            Expr::IsNull(expr) => Ok(Value::Bool(
                self.eval_expr_ctx(expr, data, last_insert_id)? == Value::Null,
            )),
            Expr::IsNotNull(expr) => Ok(Value::Bool(
                self.eval_expr_ctx(expr, data, last_insert_id)? != Value::Null,
            )),
            Expr::InList {
                expr,
                list,
                negated,
            } => {
                let value = self.eval_expr_ctx(expr, data, last_insert_id)?;
                let candidates = list
                    .iter()
                    .map(|item| self.eval_expr_ctx(item, data, last_insert_id))
                    .collect::<Result<Vec<_>>>()?;
                Ok(eval_in_values(value, candidates, *negated))
            }
            Expr::Like {
                expr,
                pattern,
                negated,
                ..
            } => {
                let target = self.eval_expr_ctx(expr, data, last_insert_id)?;
                let pattern = self.eval_expr_ctx(pattern, data, last_insert_id)?;
                Ok(eval_like_values(target, pattern, *negated))
            }
            Expr::Between {
                expr,
                negated,
                low,
                high,
            } => {
                let v = self.eval_expr_ctx(expr, data, last_insert_id)?;
                let lo = self.eval_expr_ctx(low, data, last_insert_id)?;
                let hi = self.eval_expr_ctx(high, data, last_insert_id)?;
                Ok(eval_between_values(v, lo, hi, *negated))
            }
            Expr::Case {
                operand,
                conditions,
                results,
                else_result,
            } => {
                for (cond, result) in conditions.iter().zip(results.iter()) {
                    let matches = match operand {
                        Some(op) => mysql_eq(
                            &self.eval_expr_ctx(op, data, last_insert_id)?,
                            &self.eval_expr_ctx(cond, data, last_insert_id)?,
                        ),
                        None => value_truthy(&self.eval_expr_ctx(cond, data, last_insert_id)?),
                    };
                    if matches {
                        return self.eval_expr_ctx(result, data, last_insert_id);
                    }
                }
                match else_result {
                    Some(e) => self.eval_expr_ctx(e, data, last_insert_id),
                    None => Ok(Value::Null),
                }
            }
            Expr::Cast {
                expr, data_type, ..
            } => cast_json_value(
                self.eval_expr_ctx(expr, data, last_insert_id)?,
                &data_type.to_string(),
            ),
            _ => eval_expr(expr, data, last_insert_id),
        }
    }

    pub(super) fn eval_scalar_subquery(&self, query: &Query) -> Result<Value> {
        let result = self.select_query(query.clone())?;
        if result.columns.len() != 1 {
            return Err(anyhow!("scalar subquery must return exactly one column"));
        }
        if result.rows.len() > 1 {
            return Err(anyhow!("scalar subquery returns more than one row"));
        }
        Ok(result
            .rows
            .first()
            .and_then(|row| first_projected_value(row, &result.columns))
            .unwrap_or(Value::Null))
    }

    pub(super) fn matches_selection_ctx(
        &self,
        selection: Option<&Expr>,
        data: &Map<String, Value>,
        last_insert_id: u64,
    ) -> Result<bool> {
        matches_selection_with(selection, |expr| {
            self.eval_expr_ctx(expr, data, last_insert_id)
        })
    }

    pub(super) fn join_matches_ctx(
        &self,
        join: &JoinOperator,
        data: &Map<String, Value>,
    ) -> Result<bool> {
        match join {
            JoinOperator::Inner(constraint) | JoinOperator::LeftOuter(constraint) => {
                match constraint {
                    JoinConstraint::On(expr) => self.matches_selection_ctx(Some(expr), data, 0),
                    _ => Err(anyhow!("unsupported join constraint: {constraint:?}")),
                }
            }
            JoinOperator::CrossJoin => Ok(true),
            _ => Err(anyhow!("unsupported join type")),
        }
    }
    pub(super) fn select_information_schema_tables(&self, select: &Select) -> Result<QueryResult> {
        let mut rows = Vec::new();
        for table in self.schemas.iter() {
            let mut row = Map::new();
            row.insert("table_schema".to_string(), Value::String("app".to_string()));
            row.insert("table_name".to_string(), Value::String(table.key().clone()));
            row.insert(
                "table_type".to_string(),
                Value::String("BASE TABLE".to_string()),
            );
            rows.push(row);
        }
        virtual_select_result(select, rows)
    }

    pub(super) fn select_information_schema_schemata(
        &self,
        select: &Select,
    ) -> Result<QueryResult> {
        let mut row = Map::new();
        row.insert("catalog_name".to_string(), Value::String("def".to_string()));
        row.insert("schema_name".to_string(), Value::String("app".to_string()));
        row.insert(
            "default_character_set_name".to_string(),
            Value::String("utf8mb4".to_string()),
        );
        row.insert(
            "default_collation_name".to_string(),
            Value::String("utf8mb4_general_ci".to_string()),
        );
        virtual_select_result(select, vec![row])
    }

    pub(super) fn select_information_schema_columns(&self, select: &Select) -> Result<QueryResult> {
        let mut rows = Vec::new();
        for schema in self.schemas.iter() {
            for (idx, col) in ordered_schema_columns(&schema).into_iter().enumerate() {
                let Some(hint) = schema.columns.get(&col) else {
                    continue;
                };
                let column_key = mysql_column_key(&schema, &col);
                let is_pk = column_key == "PRI";
                let (column_type, data_type) =
                    mysql_column_metadata_types(hint.sql_type.as_deref());
                let mut row = Map::new();
                row.insert("table_schema".to_string(), Value::String("app".to_string()));
                row.insert(
                    "table_name".to_string(),
                    Value::String(schema.table.clone()),
                );
                row.insert("column_name".to_string(), Value::String(col.clone()));
                row.insert(
                    "ordinal_position".to_string(),
                    Value::Number(Number::from(idx + 1)),
                );
                row.insert(
                    "is_nullable".to_string(),
                    Value::String(if is_pk || hint.nullable == Some(false) {
                        "NO".to_string()
                    } else {
                        "YES".to_string()
                    }),
                );
                row.insert(
                    "column_default".to_string(),
                    hint.default
                        .clone()
                        .map(Value::String)
                        .unwrap_or(Value::Null),
                );
                row.insert("column_type".to_string(), Value::String(column_type));
                row.insert("data_type".to_string(), Value::String(data_type));
                row.insert(
                    "column_key".to_string(),
                    Value::String(column_key.to_string()),
                );
                row.insert(
                    "extra".to_string(),
                    Value::String(if hint.auto_increment {
                        "auto_increment".to_string()
                    } else {
                        String::new()
                    }),
                );
                rows.push(row);
            }
        }
        virtual_select_result(select, rows)
    }

    pub(super) fn select_information_schema_table_constraints(
        &self,
        select: &Select,
    ) -> Result<QueryResult> {
        let mut rows = Vec::new();
        for schema in self.schemas.iter() {
            if !schema.primary_key.is_empty() {
                let mut row = Map::new();
                row.insert(
                    "constraint_schema".to_string(),
                    Value::String("app".to_string()),
                );
                row.insert("table_schema".to_string(), Value::String("app".to_string()));
                row.insert(
                    "table_name".to_string(),
                    Value::String(schema.table.clone()),
                );
                row.insert(
                    "constraint_name".to_string(),
                    Value::String("PRIMARY".to_string()),
                );
                row.insert(
                    "constraint_type".to_string(),
                    Value::String("PRIMARY KEY".to_string()),
                );
                rows.push(row);
            }

            for unique in &schema.unique {
                let mut row = Map::new();
                row.insert(
                    "constraint_schema".to_string(),
                    Value::String("app".to_string()),
                );
                row.insert("table_schema".to_string(), Value::String("app".to_string()));
                row.insert(
                    "table_name".to_string(),
                    Value::String(schema.table.clone()),
                );
                row.insert(
                    "constraint_name".to_string(),
                    Value::String(unique_index_name(&schema, unique)),
                );
                row.insert(
                    "constraint_type".to_string(),
                    Value::String("UNIQUE".to_string()),
                );
                rows.push(row);
            }

            for foreign_key in &schema.foreign_keys {
                let mut row = Map::new();
                row.insert(
                    "constraint_schema".to_string(),
                    Value::String("app".to_string()),
                );
                row.insert("table_schema".to_string(), Value::String("app".to_string()));
                row.insert(
                    "table_name".to_string(),
                    Value::String(schema.table.clone()),
                );
                row.insert(
                    "constraint_name".to_string(),
                    Value::String(foreign_key.name.clone()),
                );
                row.insert(
                    "constraint_type".to_string(),
                    Value::String("FOREIGN KEY".to_string()),
                );
                rows.push(row);
            }
        }
        virtual_select_result(select, rows)
    }

    pub(super) fn select_information_schema_statistics(
        &self,
        select: &Select,
    ) -> Result<QueryResult> {
        let mut rows = Vec::new();
        for schema in self.schemas.iter() {
            for index in &schema.indexes {
                for (idx, col) in index.columns.iter().enumerate() {
                    let mut row = Map::new();
                    row.insert("table_schema".to_string(), Value::String("app".to_string()));
                    row.insert(
                        "table_name".to_string(),
                        Value::String(schema.table.clone()),
                    );
                    row.insert("index_name".to_string(), Value::String(index.name.clone()));
                    row.insert("column_name".to_string(), Value::String(col.clone()));
                    row.insert(
                        "seq_in_index".to_string(),
                        Value::Number(Number::from(idx + 1)),
                    );
                    row.insert(
                        "non_unique".to_string(),
                        Value::Number(Number::from(if index.unique { 0 } else { 1 })),
                    );
                    rows.push(row);
                }
            }
        }
        virtual_select_result(select, rows)
    }

    pub(super) fn select_information_schema_key_column_usage(
        &self,
        select: &Select,
    ) -> Result<QueryResult> {
        let mut rows = Vec::new();
        for schema in self.schemas.iter() {
            for (idx, col) in schema.primary_key.iter().enumerate() {
                rows.push(key_column_usage_row(
                    &schema.table,
                    "PRIMARY",
                    col,
                    idx + 1,
                    None,
                    None,
                ));
            }
            for unique in &schema.unique {
                let constraint_name = unique_index_name(&schema, unique);
                for (idx, col) in unique.iter().enumerate() {
                    rows.push(key_column_usage_row(
                        &schema.table,
                        &constraint_name,
                        col,
                        idx + 1,
                        None,
                        None,
                    ));
                }
            }
            for foreign_key in &schema.foreign_keys {
                for (idx, col) in foreign_key.columns.iter().enumerate() {
                    let referenced = foreign_key.referenced_columns.get(idx).cloned();
                    rows.push(key_column_usage_row(
                        &schema.table,
                        &foreign_key.name,
                        col,
                        idx + 1,
                        Some(idx + 1),
                        Some((foreign_key.referenced_table.clone(), referenced)),
                    ));
                }
            }
        }
        virtual_select_result(select, rows)
    }

    pub(super) fn select_information_schema_referential_constraints(
        &self,
        select: &Select,
    ) -> Result<QueryResult> {
        let mut rows = Vec::new();
        for schema in self.schemas.iter() {
            for foreign_key in &schema.foreign_keys {
                let mut row = Map::new();
                row.insert(
                    "constraint_catalog".to_string(),
                    Value::String("def".to_string()),
                );
                row.insert(
                    "constraint_schema".to_string(),
                    Value::String("app".to_string()),
                );
                row.insert(
                    "constraint_name".to_string(),
                    Value::String(foreign_key.name.clone()),
                );
                row.insert(
                    "unique_constraint_catalog".to_string(),
                    Value::String("def".to_string()),
                );
                row.insert(
                    "unique_constraint_schema".to_string(),
                    Value::String("app".to_string()),
                );
                row.insert(
                    "unique_constraint_name".to_string(),
                    Value::String("PRIMARY".to_string()),
                );
                row.insert(
                    "match_option".to_string(),
                    Value::String("NONE".to_string()),
                );
                row.insert(
                    "update_rule".to_string(),
                    Value::String(
                        foreign_key
                            .on_update
                            .clone()
                            .unwrap_or_else(|| "NO ACTION".to_string()),
                    ),
                );
                row.insert(
                    "delete_rule".to_string(),
                    Value::String(
                        foreign_key
                            .on_delete
                            .clone()
                            .unwrap_or_else(|| "NO ACTION".to_string()),
                    ),
                );
                row.insert(
                    "table_name".to_string(),
                    Value::String(schema.table.clone()),
                );
                row.insert(
                    "referenced_table_name".to_string(),
                    Value::String(foreign_key.referenced_table.clone()),
                );
                rows.push(row);
            }
        }
        virtual_select_result(select, rows)
    }

    pub(super) fn select_information_schema_character_sets(
        &self,
        select: &Select,
    ) -> Result<QueryResult> {
        let rows = [
            ("utf8mb4", "utf8mb4_general_ci", "UTF-8 Unicode", 4),
            ("utf8mb3", "utf8mb3_general_ci", "UTF-8 Unicode", 3),
            ("latin1", "latin1_swedish_ci", "cp1252 West European", 1),
            ("ascii", "ascii_general_ci", "US ASCII", 1),
            ("binary", "binary", "Binary pseudo charset", 1),
        ]
        .iter()
        .map(|(name, default_collation, description, maxlen)| {
            let mut row = Map::new();
            row.insert(
                "character_set_name".to_string(),
                Value::String((*name).to_string()),
            );
            row.insert(
                "default_collate_name".to_string(),
                Value::String((*default_collation).to_string()),
            );
            row.insert(
                "description".to_string(),
                Value::String((*description).to_string()),
            );
            row.insert("maxlen".to_string(), Value::Number(Number::from(*maxlen)));
            row
        })
        .collect();
        virtual_select_result(select, rows)
    }

    pub(super) fn select_information_schema_collations(
        &self,
        select: &Select,
    ) -> Result<QueryResult> {
        let rows = [
            ("utf8mb4_general_ci", "utf8mb4", 45, "Yes", "Yes", 1),
            ("utf8mb4_bin", "utf8mb4", 46, "", "Yes", 1),
            ("utf8mb3_general_ci", "utf8mb3", 33, "Yes", "Yes", 1),
            ("latin1_swedish_ci", "latin1", 8, "Yes", "Yes", 1),
            ("ascii_general_ci", "ascii", 11, "Yes", "Yes", 1),
            ("binary", "binary", 63, "Yes", "Yes", 1),
        ]
        .iter()
        .map(|(name, charset, id, is_default, is_compiled, sortlen)| {
            let mut row = Map::new();
            row.insert(
                "collation_name".to_string(),
                Value::String((*name).to_string()),
            );
            row.insert(
                "character_set_name".to_string(),
                Value::String((*charset).to_string()),
            );
            row.insert("id".to_string(), Value::Number(Number::from(*id)));
            row.insert(
                "is_default".to_string(),
                Value::String((*is_default).to_string()),
            );
            row.insert(
                "is_compiled".to_string(),
                Value::String((*is_compiled).to_string()),
            );
            row.insert("sortlen".to_string(), Value::Number(Number::from(*sortlen)));
            row
        })
        .collect();
        virtual_select_result(select, rows)
    }

    pub(super) fn select_information_schema_views(&self, select: &Select) -> Result<QueryResult> {
        virtual_select_result(select, Vec::new())
    }

    pub(super) fn select_information_schema_routines(
        &self,
        select: &Select,
    ) -> Result<QueryResult> {
        virtual_select_result(select, Vec::new())
    }

    pub(super) fn select_information_schema_engines(&self, select: &Select) -> Result<QueryResult> {
        let engines = vec![
            (
                "InnoDB",
                "YES",
                "Supports transactions, row-level locking, and foreign keys",
            ),
            ("MyISAM", "NO", "MyISAM storage engine"),
            ("MEMORY", "NO", "Hash based, stored in memory"),
            ("CSV", "NO", "CSV storage engine"),
            ("ARCHIVE", "NO", "Archive storage engine"),
        ];
        let rows = engines
            .iter()
            .map(|(name, support, comment)| {
                let mut row = Map::new();
                row.insert("engine".to_string(), Value::String((*name).to_string()));
                row.insert("support".to_string(), Value::String((*support).to_string()));
                row.insert("comment".to_string(), Value::String((*comment).to_string()));
                row.insert("transactions".to_string(), Value::String("NO".to_string()));
                row.insert("xa".to_string(), Value::String("NO".to_string()));
                row.insert("savepoints".to_string(), Value::String("NO".to_string()));
                row
            })
            .collect();
        virtual_select_result(select, rows)
    }

    pub(super) fn select_information_schema_processlist(
        &self,
        select: &Select,
    ) -> Result<QueryResult> {
        let mut row = Map::new();
        row.insert("id".to_string(), Value::Number(Number::from(1)));
        row.insert("user".to_string(), Value::String("root".to_string()));
        row.insert("host".to_string(), Value::String("localhost".to_string()));
        row.insert("db".to_string(), Value::String("app".to_string()));
        row.insert("command".to_string(), Value::String("Sleep".to_string()));
        row.insert("time".to_string(), Value::Number(Number::from(0)));
        row.insert("state".to_string(), Value::String("".to_string()));
        row.insert("info".to_string(), Value::Null);
        row.insert("time_ms".to_string(), Value::Number(Number::from(0)));
        virtual_select_result(select, vec![row])
    }

    pub(super) fn select_information_schema_session_variables(
        &self,
        select: &Select,
    ) -> Result<QueryResult> {
        let variables = vec![
            "version",
            "version_comment",
            "autocommit",
            "sql_mode",
            "time_zone",
            "transaction_isolation",
            "tx_isolation",
            "character_set_client",
            "character_set_connection",
            "character_set_results",
            "collation_connection",
            "max_allowed_packet",
        ];
        let rows = variables
            .iter()
            .map(|name| {
                let value = session_variable_default(name);
                let mut row = Map::new();
                row.insert(
                    "variable_name".to_string(),
                    Value::String(name.to_uppercase()),
                );
                row.insert("variable_value".to_string(), value);
                row
            })
            .collect();
        virtual_select_result(select, rows)
    }

    pub(super) fn select_information_schema_global_variables(
        &self,
        select: &Select,
    ) -> Result<QueryResult> {
        // Global variables are same as session variables for our purposes
        self.select_information_schema_session_variables(select)
    }

    pub(super) fn select_information_schema_keywords(
        &self,
        select: &Select,
    ) -> Result<QueryResult> {
        let keywords = vec![
            "ACCESSIBLE",
            "ADD",
            "ALL",
            "ALTER",
            "ANALYZE",
            "AND",
            "AS",
            "ASC",
            "ASENSITIVE",
            "BEFORE",
            "BETWEEN",
            "BIGINT",
            "BINARY",
            "BLOB",
            "BOTH",
            "BY",
            "CALL",
            "CASCADE",
            "CASE",
            "CHANGE",
            "CHAR",
            "CHARACTER",
            "CHECK",
            "COLLATE",
            "COLUMN",
            "CONDITION",
            "CONSTRAINT",
            "CONTINUE",
            "CONVERT",
            "CREATE",
            "CROSS",
            "CURRENT_DATE",
            "CURRENT_TIME",
            "CURRENT_TIMESTAMP",
            "CURRENT_USER",
            "CURSOR",
            "DATABASE",
            "DATABASES",
            "DAY_HOUR",
            "DAY_MICROSECOND",
            "DAY_MINUTE",
            "DAY_SECOND",
            "DEC",
            "DECIMAL",
            "DECLARE",
            "DEFAULT",
            "DELAYED",
            "DELETE",
            "DESC",
            "DESCRIBE",
            "DETERMINISTIC",
            "DISTINCT",
            "DISTINCTROW",
            "DIV",
            "DOUBLE",
            "DROP",
            "DUAL",
            "EACH",
            "ELSE",
            "ELSEIF",
            "ENCLOSED",
            "ESCAPED",
            "EXISTS",
            "EXIT",
            "EXPLAIN",
            "FALSE",
            "FETCH",
            "FLOAT",
            "FLOAT4",
            "FLOAT8",
            "FOR",
            "FORCE",
            "FOREIGN",
            "FROM",
            "FULLTEXT",
            "GENERAL",
            "GET",
            "GRANT",
            "GROUP",
            "HAVING",
            "HIGH_PRIORITY",
            "HOUR_MICROSECOND",
            "HOUR_MINUTE",
            "HOUR_SECOND",
            "IF",
            "IGNORE",
            "IN",
            "INDEX",
            "INFILE",
            "INNER",
            "INOUT",
            "INSENSITIVE",
            "INSERT",
            "INT",
            "INT1",
            "INT2",
            "INT3",
            "INT4",
            "INT8",
            "INTEGER",
            "INTERVAL",
            "INTO",
            "IO_AFTER_GTIDS",
            "IO_BEFORE_GTIDS",
            "IS",
            "ITERATE",
            "JOIN",
            "KEY",
            "KEYS",
            "KILL",
            "LEADING",
            "LEAVE",
            "LEFT",
            "LIKE",
            "LIMIT",
            "LINEAR",
            "LINES",
            "LOAD",
            "LOCALTIME",
            "LOCALTIMESTAMP",
            "LOCK",
            "LONG",
            "LONGBLOB",
            "LONGTEXT",
            "LOOP",
            "LOW_PRIORITY",
            "MASTER_BIND",
            "MASTER_SSL_VERIFY_SERVER_CERT",
            "MATCH",
            "MEDIUMBLOB",
            "MEDIUMINT",
            "MEDIUMTEXT",
            "MIDDLEINT",
            "MINUTE_MICROSECOND",
            "MINUTE_SECOND",
            "MOD",
            "MODIFIES",
            "NATURAL",
            "NOT",
            "NO_WRITE_TO_BINLOG",
            "NULL",
            "NUMERIC",
            "ON",
            "ONE_SHOT",
            "OR",
            "ORDER",
            "OUT",
            "OUTER",
            "OUTFILE",
            "PARTITION",
            "PRECISION",
            "PRIMARY",
            "PROCEDURE",
            "PURGE",
            "RANGE",
            "READ",
            "READS",
            "READ_WRITE",
            "REFERENCES",
            "REGEXP",
            "RELEASE",
            "RENAME",
            "REPEAT",
            "REPLACE",
            "REQUIRE",
            "RESIGNAL",
            "RESTRICT",
            "RETURN",
            "REVOKE",
            "RIGHT",
            "RLIKE",
            "SCHEMA",
            "SCHEMAS",
            "SECOND_MICROSECOND",
            "SELECT",
            "SENSITIVE",
            "SEPARATOR",
            "SET",
            "SHOW",
            "SIGNAL",
            "SPATIAL",
            "SPECIFIC",
            "SQL",
            "SQLEXCEPTION",
            "SQLSTATE",
            "SQLWARNING",
            "SQL_BIG_RESULT",
            "SQL_CALC_FOUND_ROWS",
            "SQL_SMALL_RESULT",
            "SSL",
            "STARTING",
            "STRAIGHT_JOIN",
            "TABLE",
            "TERMINATED",
            "THEN",
            "TINYBLOB",
            "TINYINT",
            "TINYTEXT",
            "TO",
            "TRAILING",
            "TRIGGER",
            "TRUE",
            "UNDO",
            "UNION",
            "UNIQUE",
            "UNLOCK",
            "UNSIGNED",
            "UPDATE",
            "USAGE",
            "USE",
            "USING",
            "UTC_DATE",
            "UTC_TIME",
            "UTC_TIMESTAMP",
            "VALUES",
            "VARBINARY",
            "VARCHAR",
            "VARCHARACTER",
            "VARYING",
            "WHEN",
            "WHERE",
            "WHILE",
            "WITH",
            "WRITE",
            "X509",
            "XOR",
            "YEAR_MONTH",
            "ZEROFILL",
        ];
        let rows = keywords
            .iter()
            .map(|keyword| {
                let mut row = Map::new();
                row.insert("keyword".to_string(), Value::String((*keyword).to_string()));
                row.insert("reserved".to_string(), Value::Number(Number::from(1)));
                row
            })
            .collect();
        virtual_select_result(select, rows)
    }

    pub(super) fn select_information_schema_triggers(
        &self,
        select: &Select,
    ) -> Result<QueryResult> {
        // Triggers not supported yet
        virtual_select_result(select, Vec::new())
    }

    pub(super) fn select_information_schema_check_constraints(
        &self,
        select: &Select,
    ) -> Result<QueryResult> {
        // CHECK constraints not validated yet
        virtual_select_result(select, Vec::new())
    }

    pub(super) fn select_information_schema_files(&self, select: &Select) -> Result<QueryResult> {
        // File storage information
        virtual_select_result(select, Vec::new())
    }

    pub(super) fn show_tables(&self) -> QueryResult {
        let columns = vec!["Tables_in_app".to_string()];
        let rows = self
            .schemas
            .iter()
            .map(|schema| {
                let mut row = Map::new();
                row.insert(columns[0].clone(), Value::String(schema.table.clone()));
                row
            })
            .collect();
        QueryResult {
            rows_affected: 0,
            last_insert_id: 0,
            columns,
            column_metadata: vec![],
            rows,
        }
    }

    pub(super) fn show_columns(&self, table: &str) -> QueryResult {
        let columns = ["Field", "Type", "Null", "Key", "Default", "Extra"]
            .into_iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>();
        let rows = self
            .schemas
            .get(table)
            .map(|schema| {
                ordered_schema_columns(&schema)
                    .into_iter()
                    .filter_map(|column| {
                        let hint = schema.columns.get(&column)?;
                        let mut row = Map::new();
                        let key = mysql_column_key(&schema, &column);
                        row.insert("Field".to_string(), Value::String(column.clone()));
                        row.insert(
                            "Type".to_string(),
                            Value::String(mysql_column_metadata_types(hint.sql_type.as_deref()).0),
                        );
                        row.insert(
                            "Null".to_string(),
                            Value::String(if key == "PRI" || hint.nullable == Some(false) {
                                "NO".to_string()
                            } else {
                                "YES".to_string()
                            }),
                        );
                        row.insert("Key".to_string(), Value::String(key.to_string()));
                        row.insert(
                            "Default".to_string(),
                            hint.default
                                .clone()
                                .map(Value::String)
                                .unwrap_or(Value::Null),
                        );
                        row.insert(
                            "Extra".to_string(),
                            Value::String(if hint.auto_increment {
                                "auto_increment".to_string()
                            } else {
                                String::new()
                            }),
                        );
                        Some(row)
                    })
                    .collect()
            })
            .unwrap_or_default();
        QueryResult {
            rows_affected: 0,
            last_insert_id: 0,
            columns,
            column_metadata: vec![],
            rows,
        }
    }

    pub(super) fn show_index(&self, table: &str) -> QueryResult {
        let columns = [
            "Table",
            "Non_unique",
            "Key_name",
            "Seq_in_index",
            "Column_name",
            "Collation",
            "Cardinality",
            "Sub_part",
            "Packed",
            "Null",
            "Index_type",
            "Comment",
            "Index_comment",
            "Visible",
            "Expression",
        ]
        .into_iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>();
        let mut rows = Vec::new();
        if let Some(schema) = self.schemas.get(table) {
            for index in &schema.indexes {
                for (idx, column) in index.columns.iter().enumerate() {
                    let mut row = Map::new();
                    row.insert("Table".to_string(), Value::String(schema.table.clone()));
                    row.insert(
                        "Non_unique".to_string(),
                        Value::Number(Number::from(if index.unique { 0 } else { 1 })),
                    );
                    row.insert("Key_name".to_string(), Value::String(index.name.clone()));
                    row.insert(
                        "Seq_in_index".to_string(),
                        Value::Number(Number::from(idx + 1)),
                    );
                    row.insert("Column_name".to_string(), Value::String(column.clone()));
                    row.insert("Collation".to_string(), Value::String("A".to_string()));
                    row.insert("Cardinality".to_string(), Value::Null);
                    row.insert(
                        "Sub_part".to_string(),
                        index
                            .prefix_lengths
                            .get(idx)
                            .copied()
                            .flatten()
                            .map(|length| Value::Number(Number::from(length)))
                            .unwrap_or(Value::Null),
                    );
                    row.insert("Packed".to_string(), Value::Null);
                    row.insert(
                        "Null".to_string(),
                        Value::String(
                            schema
                                .columns
                                .get(column)
                                .and_then(|hint| hint.nullable)
                                .map(|nullable| if nullable { "YES" } else { "" })
                                .unwrap_or("YES")
                                .to_string(),
                        ),
                    );
                    row.insert("Index_type".to_string(), Value::String("BTREE".to_string()));
                    row.insert("Comment".to_string(), Value::String(String::new()));
                    row.insert("Index_comment".to_string(), Value::String(String::new()));
                    row.insert("Visible".to_string(), Value::String("YES".to_string()));
                    row.insert("Expression".to_string(), Value::Null);
                    rows.push(row);
                }
            }
        }
        QueryResult {
            rows_affected: 0,
            last_insert_id: 0,
            columns,
            column_metadata: vec![],
            rows,
        }
    }

    pub(super) fn show_create_table(&self, table: &str) -> QueryResult {
        let columns = vec!["Table".to_string(), "Create Table".to_string()];
        let create = self
            .schemas
            .get(table)
            .map(|schema| render_create_table(&schema))
            .unwrap_or_else(|| format!("CREATE TABLE `{table}` ()"));
        let mut row = Map::new();
        row.insert("Table".to_string(), Value::String(table.to_string()));
        row.insert("Create Table".to_string(), Value::String(create));
        QueryResult {
            rows_affected: 0,
            last_insert_id: 0,
            columns,
            column_metadata: vec![],
            rows: vec![row],
        }
    }

    pub(super) fn rename_table(&self, from: &str, to: &str) -> Result<QueryResult> {
        if self.schemas.contains_key(to) {
            return Err(anyhow!("table already exists: {to}"));
        }
        let Some((_, mut schema)) = self.schemas.remove(from) else {
            return Err(anyhow!("unknown table: {from}"));
        };
        schema.table = to.to_string();
        schema.updated_at = Some(Utc::now());
        self.schemas.insert(to.to_string(), schema);
        if let Some((_, mut table_rows)) = self.rows.remove(from) {
            for row in table_rows.values_mut() {
                row.table = to.to_string();
                row.updated_at = Utc::now();
            }
            self.rows.insert(to.to_string(), table_rows);
        }
        self.indexes.remove(from);
        self.rebuild_indexes(to);

        let auto_inc_updates = self
            .auto_inc
            .iter()
            .filter_map(|item| {
                item.key()
                    .strip_prefix(&format!("{from}:"))
                    .map(|suffix| (item.key().clone(), format!("{to}:{suffix}"), *item.value()))
            })
            .collect::<Vec<_>>();
        for (old, new, value) in auto_inc_updates {
            self.auto_inc.remove(&old);
            self.auto_inc.insert(new, value);
        }

        self.delete_table_from_storage(from)?;
        self.persist_schema(to)?;
        self.persist_auto_inc()?;
        if let Some(rows) = self.rows.get(to).map(|rows| rows.clone()) {
            for (pk, row) in rows {
                self.persist_row(to, &pk, &row)?;
            }
        }
        Ok(QueryResult::default())
    }
}

fn select_nullable_tables(select: &Select) -> BTreeSet<String> {
    let mut nullable = BTreeSet::new();
    for table in &select.from {
        let mut accumulated = table_factor_base_name(&table.relation)
            .into_iter()
            .collect::<Vec<_>>();
        for join in &table.joins {
            let right = table_factor_base_name(&join.relation);
            match join.join_operator {
                JoinOperator::LeftOuter(_) => {
                    if let Some(right) = &right {
                        nullable.insert(right.clone());
                    }
                }
                JoinOperator::RightOuter(_) => {
                    nullable.extend(accumulated.iter().cloned());
                }
                _ => {}
            }
            if let Some(right) = right {
                accumulated.push(right);
            }
        }
    }
    nullable
}

fn table_factor_base_name(factor: &TableFactor) -> Option<String> {
    let TableFactor::Table { name, .. } = factor else {
        return None;
    };
    name.0.last().map(|name| name.value.clone())
}

fn mysql_column_metadata_types(sql_type: Option<&str>) -> (String, String) {
    let column_type = sql_type.unwrap_or("text").trim().to_ascii_lowercase();
    let data_type = column_type
        .split(|character: char| character == '(' || character.is_ascii_whitespace())
        .next()
        .filter(|value| !value.is_empty())
        .unwrap_or("text")
        .to_string();
    (column_type, data_type)
}

fn mysql_column_key(schema: &TableSchemaHint, column: &str) -> &'static str {
    if schema.primary_key.iter().any(|primary| primary == column) {
        return "PRI";
    }
    if schema.indexes.iter().any(|index| {
        index.unique
            && index.columns.len() == 1
            && index.columns.first().is_some_and(|c| c == column)
    }) || schema
        .unique
        .iter()
        .any(|unique| unique.len() == 1 && unique.first().is_some_and(|c| c == column))
    {
        return "UNI";
    }
    if schema.indexes.iter().any(|index| {
        index
            .columns
            .first()
            .is_some_and(|indexed| indexed == column)
    }) {
        return "MUL";
    }
    ""
}

fn remap_set_row(
    row: &Map<String, Value>,
    source_columns: &[String],
    target_columns: &[String],
) -> Result<Map<String, Value>> {
    source_columns
        .iter()
        .zip(target_columns)
        .map(|(source, target)| {
            row.get(source)
                .cloned()
                .map(|value| (target.clone(), value))
                .ok_or_else(|| anyhow!("set operation result is missing column: {source}"))
        })
        .collect()
}

struct TableFactorRows {
    rows: Vec<Map<String, Value>>,
    nulls: Map<String, Value>,
}

fn qualified_factor_row(
    raw: &Map<String, Value>,
    table: &str,
    alias: Option<&str>,
) -> Map<String, Value> {
    let mut row = raw.clone();
    add_qualified_columns(&mut row, table, raw);
    if let Some(alias) = alias {
        add_qualified_columns(&mut row, alias, raw);
    }
    row
}

fn merge_join_rows(left: &Map<String, Value>, right: &Map<String, Value>) -> Map<String, Value> {
    let mut combined = left.clone();
    for (column, value) in right {
        combined
            .entry(column.clone())
            .or_insert_with(|| value.clone());
    }
    combined
}

fn unqualified_row_value<'a>(row: &'a Map<String, Value>, column: &str) -> Option<&'a Value> {
    row.iter()
        .find(|(candidate, _)| !candidate.contains('.') && candidate.eq_ignore_ascii_case(column))
        .map(|(_, value)| value)
}

fn resolve_window_spec(
    select: &Select,
    window: &sqlparser::ast::WindowType,
) -> Result<sqlparser::ast::WindowSpec> {
    use sqlparser::ast::WindowType;

    match window {
        WindowType::WindowSpec(spec) => {
            if let Some(base) = &spec.window_name {
                let mut resolved = resolve_named_window(select, &base.value, &mut BTreeSet::new())?;
                if !spec.partition_by.is_empty() {
                    resolved.partition_by = spec.partition_by.clone();
                }
                if !spec.order_by.is_empty() {
                    resolved.order_by = spec.order_by.clone();
                }
                if spec.window_frame.is_some() {
                    resolved.window_frame = spec.window_frame.clone();
                }
                Ok(resolved)
            } else {
                Ok(spec.clone())
            }
        }
        WindowType::NamedWindow(name) => {
            resolve_named_window(select, &name.value, &mut BTreeSet::new())
        }
    }
}

fn resolve_named_window(
    select: &Select,
    name: &str,
    seen: &mut BTreeSet<String>,
) -> Result<sqlparser::ast::WindowSpec> {
    use sqlparser::ast::NamedWindowExpr;

    if !seen.insert(name.to_ascii_lowercase()) {
        return Err(anyhow!("circular named window definition: {name}"));
    }
    let definition = select
        .named_window
        .iter()
        .find(|definition| definition.0.value.eq_ignore_ascii_case(name))
        .ok_or_else(|| anyhow!("unknown window: {name}"))?;
    match &definition.1 {
        NamedWindowExpr::WindowSpec(spec) => Ok(spec.clone()),
        NamedWindowExpr::NamedWindow(parent) => resolve_named_window(select, &parent.value, seen),
    }
}

fn window_function_arguments(function: &sqlparser::ast::Function) -> Result<Vec<Option<Expr>>> {
    let FunctionArguments::List(arguments) = &function.args else {
        return Ok(Vec::new());
    };
    if arguments.duplicate_treatment.is_some() {
        return Err(anyhow!("DISTINCT window aggregates are not supported"));
    }
    arguments
        .args
        .iter()
        .map(|argument| {
            let argument = match argument {
                FunctionArg::Named { arg, .. }
                | FunctionArg::ExprNamed { arg, .. }
                | FunctionArg::Unnamed(arg) => arg,
            };
            match argument {
                FunctionArgExpr::Expr(expr) => Ok(Some(expr.clone())),
                FunctionArgExpr::Wildcard | FunctionArgExpr::QualifiedWildcard(_) => Ok(None),
            }
        })
        .collect()
}

fn window_frame_positions(
    spec: &sqlparser::ast::WindowSpec,
    position: usize,
    len: usize,
    ordered: bool,
    peer_start: usize,
    peer_end: usize,
) -> Result<Option<(usize, usize)>> {
    use sqlparser::ast::{WindowFrameBound, WindowFrameUnits};

    let Some(frame) = &spec.window_frame else {
        // MySQL's implicit ordered frame is RANGE ... CURRENT ROW, so all
        // rows tied on the ORDER BY key belong to the current frame.
        return Ok(Some(if ordered { (0, peer_end + 1) } else { (0, len) }));
    };
    if matches!(frame.units, WindowFrameUnits::Groups) {
        return Err(anyhow!("GROUPS window frames are not supported"));
    }
    let range_frame = matches!(frame.units, WindowFrameUnits::Range);
    let bound = |bound: &WindowFrameBound, is_start: bool| -> Result<usize> {
        Ok(match bound {
            WindowFrameBound::CurrentRow if range_frame && is_start => peer_start,
            WindowFrameBound::CurrentRow if range_frame => peer_end,
            WindowFrameBound::CurrentRow => position,
            WindowFrameBound::Preceding(None) => 0,
            WindowFrameBound::Following(None) => len.saturating_sub(1),
            WindowFrameBound::Preceding(Some(offset)) => {
                if range_frame {
                    return Err(anyhow!("bounded RANGE window frames are not supported yet"));
                }
                position.saturating_sub(expr_to_usize(offset)?)
            }
            WindowFrameBound::Following(Some(offset)) => {
                if range_frame {
                    return Err(anyhow!("bounded RANGE window frames are not supported yet"));
                }
                position
                    .saturating_add(expr_to_usize(offset)?)
                    .min(len.saturating_sub(1))
            }
        })
    };
    let start = bound(&frame.start_bound, true)?;
    let end = bound(
        frame
            .end_bound
            .as_ref()
            .unwrap_or(&WindowFrameBound::CurrentRow),
        false,
    )?;
    Ok((start <= end).then_some((start, end + 1)))
}

fn eval_window_argument(
    engine: &Engine,
    argument: Option<&Option<Expr>>,
    row: &Map<String, Value>,
    last_insert_id: u64,
) -> Result<Value> {
    argument
        .and_then(Option::as_ref)
        .map(|expr| engine.eval_expr_ctx(expr, row, last_insert_id))
        .transpose()
        .map(|value| value.unwrap_or(Value::Null))
}

fn window_usize_argument(
    engine: &Engine,
    argument: Option<&Option<Expr>>,
    row: &Map<String, Value>,
    last_insert_id: u64,
    default: usize,
) -> Result<usize> {
    let Some(argument) = argument.and_then(Option::as_ref) else {
        return Ok(default);
    };
    let value = engine.eval_expr_ctx(argument, row, last_insert_id)?;
    value
        .as_u64()
        .or_else(|| value.as_i64().and_then(|value| u64::try_from(value).ok()))
        .map(|value| value as usize)
        .ok_or_else(|| anyhow!("window function argument must be a nonnegative integer"))
}

fn inline_common_table_expressions(mut query: Query) -> Result<Query> {
    use sqlparser::ast::{TableAlias, VisitMut, VisitorMut};
    use std::ops::ControlFlow;

    let Some(with) = query.with.take() else {
        return Ok(query);
    };
    if with.recursive {
        return Err(anyhow!(
            "recursive common table expressions are not supported yet"
        ));
    }

    #[derive(Clone)]
    struct CteDefinition {
        query: Box<Query>,
        columns: Vec<sqlparser::ast::TableAliasColumnDef>,
    }

    struct Inliner<'a> {
        definitions: &'a BTreeMap<String, CteDefinition>,
    }

    impl VisitorMut for Inliner<'_> {
        type Break = ();

        fn pre_visit_table_factor(&mut self, factor: &mut TableFactor) -> ControlFlow<Self::Break> {
            let TableFactor::Table { name, alias, .. } = factor else {
                return ControlFlow::Continue(());
            };
            if name.0.len() != 1 {
                return ControlFlow::Continue(());
            }
            let cte_name = name.0[0].value.to_ascii_lowercase();
            let Some(definition) = self.definitions.get(&cte_name) else {
                return ControlFlow::Continue(());
            };
            let mut derived_alias = alias.clone().unwrap_or_else(|| TableAlias {
                name: name.0[0].clone(),
                columns: Vec::new(),
            });
            if derived_alias.columns.is_empty() {
                derived_alias.columns = definition.columns.clone();
            }
            *factor = TableFactor::Derived {
                lateral: false,
                subquery: definition.query.clone(),
                alias: Some(derived_alias),
            };
            ControlFlow::Continue(())
        }
    }

    let mut definitions = BTreeMap::new();
    for cte in with.cte_tables {
        if cte.from.is_some() || cte.materialized.is_some() {
            return Err(anyhow!("unsupported common table expression modifier"));
        }
        let mut cte_query = *cte.query;
        let _ = cte_query.visit(&mut Inliner {
            definitions: &definitions,
        });
        definitions.insert(
            cte.alias.name.value.to_ascii_lowercase(),
            CteDefinition {
                query: Box::new(cte_query),
                columns: cte.alias.columns,
            },
        );
    }
    let _ = query.visit(&mut Inliner {
        definitions: &definitions,
    });
    Ok(query)
}

fn set_intersection(
    left: Vec<Map<String, Value>>,
    right: Vec<Map<String, Value>>,
    all: bool,
) -> Vec<Map<String, Value>> {
    let mut right_counts = BTreeMap::<String, usize>::new();
    for row in right {
        *right_counts.entry(encode_json_row(&row)).or_default() += 1;
    }
    let mut emitted = BTreeSet::new();
    let mut output = Vec::new();
    for row in left {
        let key = encode_json_row(&row);
        if right_counts.get(&key).copied().unwrap_or(0) == 0 {
            continue;
        }
        if all {
            *right_counts.get_mut(&key).expect("positive count exists") -= 1;
            output.push(row);
        } else if emitted.insert(key) {
            output.push(row);
        }
    }
    output
}

fn set_difference(
    left: Vec<Map<String, Value>>,
    right: Vec<Map<String, Value>>,
    all: bool,
) -> Vec<Map<String, Value>> {
    let mut right_counts = BTreeMap::<String, usize>::new();
    for row in right {
        *right_counts.entry(encode_json_row(&row)).or_default() += 1;
    }
    let mut emitted = BTreeSet::new();
    let mut output = Vec::new();
    for row in left {
        let key = encode_json_row(&row);
        if all {
            if let Some(count) = right_counts.get_mut(&key)
                && *count > 0
            {
                *count -= 1;
                continue;
            }
            output.push(row);
        } else if !right_counts.contains_key(&key) && emitted.insert(key) {
            output.push(row);
        }
    }
    output
}

#[derive(Clone, Default)]
struct ColumnScope {
    unqualified: BTreeMap<String, usize>,
    qualified: BTreeSet<String>,
    aliases: BTreeSet<String>,
}

fn validate_expr_columns(expr: &Expr, scope: &ColumnScope) -> Result<()> {
    if system_variable_expr_value(expr).is_some() {
        return Ok(());
    }

    match expr {
        Expr::Identifier(identifier) => {
            let name = identifier.value.to_ascii_lowercase();
            if scope.aliases.contains(&name) || is_bare_datetime_keyword(&identifier.value) {
                return Ok(());
            }
            match scope.unqualified.get(&name).copied() {
                Some(1) => Ok(()),
                Some(_) => Err(anyhow!("ambiguous column: {}", identifier.value)),
                None => Err(anyhow!("unknown column: {}", identifier.value)),
            }
        }
        Expr::CompoundIdentifier(parts) => {
            let name = parts
                .iter()
                .map(|part| part.value.to_ascii_lowercase())
                .collect::<Vec<_>>()
                .join(".");
            if scope.qualified.contains(&name) {
                Ok(())
            } else {
                Err(anyhow!("unknown column: {name}"))
            }
        }
        Expr::Nested(expr)
        | Expr::UnaryOp { expr, .. }
        | Expr::IsFalse(expr)
        | Expr::IsNotFalse(expr)
        | Expr::IsTrue(expr)
        | Expr::IsNotTrue(expr)
        | Expr::IsNull(expr)
        | Expr::IsNotNull(expr)
        | Expr::IsUnknown(expr)
        | Expr::IsNotUnknown(expr)
        | Expr::Cast { expr, .. }
        | Expr::Extract { expr, .. }
        | Expr::Ceil { expr, .. }
        | Expr::Floor { expr, .. } => validate_expr_columns(expr, scope),
        Expr::Convert { expr, styles, .. } => {
            validate_expr_columns(expr, scope)?;
            for style in styles {
                validate_expr_columns(style, scope)?;
            }
            Ok(())
        }
        Expr::BinaryOp { left, right, .. } => {
            validate_expr_columns(left, scope)?;
            validate_expr_columns(right, scope)
        }
        Expr::InList { expr, list, .. } => {
            validate_expr_columns(expr, scope)?;
            for item in list {
                validate_expr_columns(item, scope)?;
            }
            Ok(())
        }
        Expr::InSubquery { expr, .. } => validate_expr_columns(expr, scope),
        Expr::Between {
            expr, low, high, ..
        } => {
            validate_expr_columns(expr, scope)?;
            validate_expr_columns(low, scope)?;
            validate_expr_columns(high, scope)
        }
        Expr::Like { expr, pattern, .. } => {
            validate_expr_columns(expr, scope)?;
            validate_expr_columns(pattern, scope)
        }
        Expr::Position { expr, r#in } => {
            validate_expr_columns(expr, scope)?;
            validate_expr_columns(r#in, scope)
        }
        Expr::Substring {
            expr,
            substring_from,
            substring_for,
            ..
        } => {
            validate_expr_columns(expr, scope)?;
            if let Some(from) = substring_from {
                validate_expr_columns(from, scope)?;
            }
            if let Some(for_expr) = substring_for {
                validate_expr_columns(for_expr, scope)?;
            }
            Ok(())
        }
        Expr::Trim {
            expr,
            trim_what,
            trim_characters,
            ..
        } => {
            validate_expr_columns(expr, scope)?;
            if let Some(trim_what) = trim_what {
                validate_expr_columns(trim_what, scope)?;
            }
            if let Some(items) = trim_characters {
                for item in items {
                    validate_expr_columns(item, scope)?;
                }
            }
            Ok(())
        }
        Expr::Case {
            operand,
            conditions,
            results,
            else_result,
        } => {
            if let Some(operand) = operand {
                validate_expr_columns(operand, scope)?;
            }
            for expr in conditions.iter().chain(results.iter()) {
                validate_expr_columns(expr, scope)?;
            }
            if let Some(else_result) = else_result {
                validate_expr_columns(else_result, scope)?;
            }
            Ok(())
        }
        Expr::Function(function) => {
            validate_function_arguments(&function.parameters, scope, false)?;
            let function_name = function
                .name
                .0
                .last()
                .map(|part| part.value.to_ascii_uppercase())
                .unwrap_or_default();
            validate_function_arguments(
                &function.args,
                scope,
                matches!(function_name.as_str(), "TIMESTAMPADD" | "TIMESTAMPDIFF"),
            )
        }
        Expr::Interval(interval) => validate_expr_columns(&interval.value, scope),
        Expr::Value(_) | Expr::Subquery(_) | Expr::Exists { .. } => Ok(()),
        // The support validator rejects all other expression shapes before
        // execution. Keeping this arm makes name binding resilient to new AST
        // variants while preserving that fail-closed boundary.
        _ => Ok(()),
    }
}

fn validate_function_arguments(
    arguments: &FunctionArguments,
    scope: &ColumnScope,
    skip_first_unit: bool,
) -> Result<()> {
    let FunctionArguments::List(list) = arguments else {
        return Ok(());
    };

    for (index, argument) in list.args.iter().enumerate() {
        let argument = match argument {
            FunctionArg::Named { arg, .. }
            | FunctionArg::ExprNamed { arg, .. }
            | FunctionArg::Unnamed(arg) => arg,
        };
        if skip_first_unit
            && index == 0
            && matches!(argument, FunctionArgExpr::Expr(Expr::Identifier(_)))
        {
            continue;
        }
        if let FunctionArgExpr::Expr(expr) = argument {
            validate_expr_columns(expr, scope)?;
        }
    }
    for clause in &list.clauses {
        match clause {
            FunctionArgumentClause::OrderBy(items) => {
                for item in items {
                    validate_expr_columns(&item.expr, scope)?;
                }
            }
            FunctionArgumentClause::Limit(expr) => validate_expr_columns(expr, scope)?,
            FunctionArgumentClause::Having(bound) => validate_expr_columns(&bound.1, scope)?,
            _ => {}
        }
    }
    Ok(())
}

fn reject_correlated_subquery(query: &Query) -> Result<()> {
    let local_qualifiers = query_local_qualifiers(query)?;
    if let Some(identifier) = query_outer_reference(query, &local_qualifiers)? {
        return Err(anyhow!(
            "correlated subqueries are not supported yet: {identifier}"
        ));
    }
    Ok(())
}

fn query_local_qualifiers(query: &Query) -> Result<BTreeSet<String>> {
    let mut qualifiers = BTreeSet::new();
    collect_set_expr_qualifiers(&query.body, &mut qualifiers)?;
    Ok(qualifiers)
}

fn collect_set_expr_qualifiers(body: &SetExpr, qualifiers: &mut BTreeSet<String>) -> Result<()> {
    match body {
        SetExpr::Select(select) => collect_select_qualifiers(select, qualifiers),
        SetExpr::Query(query) => collect_set_expr_qualifiers(&query.body, qualifiers),
        SetExpr::SetOperation { left, right, .. } => {
            collect_set_expr_qualifiers(left, qualifiers)?;
            collect_set_expr_qualifiers(right, qualifiers)
        }
        _ => Ok(()),
    }
}

fn collect_select_qualifiers(select: &Select, qualifiers: &mut BTreeSet<String>) -> Result<()> {
    for table in &select.from {
        collect_table_qualifier(&table.relation, qualifiers)?;
        for join in &table.joins {
            collect_table_qualifier(&join.relation, qualifiers)?;
        }
    }
    Ok(())
}

fn collect_table_qualifier(
    relation: &TableFactor,
    qualifiers: &mut BTreeSet<String>,
) -> Result<()> {
    match relation {
        TableFactor::Table { name, alias, .. } => {
            let full_name = object_name(name)?;
            qualifiers.insert(full_name.clone());
            if let Some(last) = full_name.rsplit('.').next() {
                qualifiers.insert(last.to_string());
            }
            if let Some(alias) = alias {
                qualifiers.insert(alias.name.value.clone());
            }
        }
        TableFactor::Derived {
            alias: Some(alias), ..
        } => {
            qualifiers.insert(alias.name.value.clone());
        }
        _ => {}
    }
    Ok(())
}

fn query_outer_reference(
    query: &Query,
    local_qualifiers: &BTreeSet<String>,
) -> Result<Option<String>> {
    if let Some(order_by) = &query.order_by {
        for item in &order_by.exprs {
            if let Some(identifier) = expr_outer_reference(&item.expr, local_qualifiers)? {
                return Ok(Some(identifier));
            }
        }
    }
    set_expr_outer_reference(&query.body, local_qualifiers)
}

fn set_expr_outer_reference(
    body: &SetExpr,
    local_qualifiers: &BTreeSet<String>,
) -> Result<Option<String>> {
    match body {
        SetExpr::Select(select) => select_outer_reference(select, local_qualifiers),
        SetExpr::Query(query) => query_outer_reference(query, local_qualifiers),
        SetExpr::SetOperation { left, right, .. } => {
            if let Some(identifier) = set_expr_outer_reference(left, local_qualifiers)? {
                return Ok(Some(identifier));
            }
            set_expr_outer_reference(right, local_qualifiers)
        }
        _ => Ok(None),
    }
}

fn select_outer_reference(
    select: &Select,
    local_qualifiers: &BTreeSet<String>,
) -> Result<Option<String>> {
    for item in &select.projection {
        if let Some(identifier) = select_item_outer_reference(item, local_qualifiers)? {
            return Ok(Some(identifier));
        }
    }
    for table in &select.from {
        for join in &table.joins {
            if let Some(identifier) = join_outer_reference(&join.join_operator, local_qualifiers)? {
                return Ok(Some(identifier));
            }
        }
    }
    for expr in [&select.selection, &select.having, &select.qualify]
        .into_iter()
        .flatten()
    {
        if let Some(identifier) = expr_outer_reference(expr, local_qualifiers)? {
            return Ok(Some(identifier));
        }
    }
    match &select.group_by {
        GroupByExpr::Expressions(exprs, _) => {
            for expr in exprs {
                if let Some(identifier) = expr_outer_reference(expr, local_qualifiers)? {
                    return Ok(Some(identifier));
                }
            }
        }
        GroupByExpr::All(_) => {}
    }
    Ok(None)
}

fn select_item_outer_reference(
    item: &SelectItem,
    local_qualifiers: &BTreeSet<String>,
) -> Result<Option<String>> {
    match item {
        SelectItem::UnnamedExpr(expr) | SelectItem::ExprWithAlias { expr, .. } => {
            expr_outer_reference(expr, local_qualifiers)
        }
        _ => Ok(None),
    }
}

fn join_outer_reference(
    join: &JoinOperator,
    local_qualifiers: &BTreeSet<String>,
) -> Result<Option<String>> {
    match join {
        JoinOperator::Inner(JoinConstraint::On(expr))
        | JoinOperator::LeftOuter(JoinConstraint::On(expr)) => {
            expr_outer_reference(expr, local_qualifiers)
        }
        _ => Ok(None),
    }
}

fn expr_outer_reference(
    expr: &Expr,
    local_qualifiers: &BTreeSet<String>,
) -> Result<Option<String>> {
    match expr {
        Expr::CompoundIdentifier(parts) => {
            let identifier = parts
                .iter()
                .map(|part| part.value.clone())
                .collect::<Vec<_>>()
                .join(".");
            let qualifier = parts
                .iter()
                .take(parts.len().saturating_sub(1))
                .map(|part| part.value.clone())
                .collect::<Vec<_>>()
                .join(".");
            if !qualifier.is_empty() && !local_qualifiers.contains(&qualifier) {
                return Ok(Some(identifier));
            }
            Ok(None)
        }
        Expr::Nested(expr)
        | Expr::UnaryOp { expr, .. }
        | Expr::IsNull(expr)
        | Expr::IsNotNull(expr)
        | Expr::Cast { expr, .. } => expr_outer_reference(expr, local_qualifiers),
        Expr::BinaryOp { left, right, .. } => {
            if let Some(identifier) = expr_outer_reference(left, local_qualifiers)? {
                return Ok(Some(identifier));
            }
            expr_outer_reference(right, local_qualifiers)
        }
        Expr::InList { expr, list, .. } => {
            if let Some(identifier) = expr_outer_reference(expr, local_qualifiers)? {
                return Ok(Some(identifier));
            }
            for item in list {
                if let Some(identifier) = expr_outer_reference(item, local_qualifiers)? {
                    return Ok(Some(identifier));
                }
            }
            Ok(None)
        }
        Expr::Between {
            expr, low, high, ..
        } => {
            for expr in [expr.as_ref(), low.as_ref(), high.as_ref()] {
                if let Some(identifier) = expr_outer_reference(expr, local_qualifiers)? {
                    return Ok(Some(identifier));
                }
            }
            Ok(None)
        }
        Expr::Like { expr, pattern, .. } => {
            if let Some(identifier) = expr_outer_reference(expr, local_qualifiers)? {
                return Ok(Some(identifier));
            }
            expr_outer_reference(pattern, local_qualifiers)
        }
        Expr::Case {
            operand,
            conditions,
            results,
            else_result,
        } => {
            if let Some(operand) = operand
                && let Some(identifier) = expr_outer_reference(operand, local_qualifiers)?
            {
                return Ok(Some(identifier));
            }
            for expr in conditions.iter().chain(results.iter()) {
                if let Some(identifier) = expr_outer_reference(expr, local_qualifiers)? {
                    return Ok(Some(identifier));
                }
            }
            if let Some(expr) = else_result
                && let Some(identifier) = expr_outer_reference(expr, local_qualifiers)?
            {
                return Ok(Some(identifier));
            }
            Ok(None)
        }
        Expr::Function(function) => function_outer_reference(function, local_qualifiers),
        Expr::Subquery(query) => query_outer_reference(query, &query_local_qualifiers(query)?),
        Expr::Exists { subquery, .. } => {
            query_outer_reference(subquery, &query_local_qualifiers(subquery)?)
        }
        Expr::InSubquery { expr, subquery, .. } => {
            if let Some(identifier) = expr_outer_reference(expr, local_qualifiers)? {
                return Ok(Some(identifier));
            }
            query_outer_reference(subquery, &query_local_qualifiers(subquery)?)
        }
        _ => Ok(None),
    }
}

fn function_outer_reference(
    function: &sqlparser::ast::Function,
    local_qualifiers: &BTreeSet<String>,
) -> Result<Option<String>> {
    for args in [&function.parameters, &function.args] {
        if let Some(identifier) = function_args_outer_reference(args, local_qualifiers)? {
            return Ok(Some(identifier));
        }
    }
    if let Some(filter) = &function.filter
        && let Some(identifier) = expr_outer_reference(filter, local_qualifiers)?
    {
        return Ok(Some(identifier));
    }
    for item in &function.within_group {
        if let Some(identifier) = expr_outer_reference(&item.expr, local_qualifiers)? {
            return Ok(Some(identifier));
        }
    }
    Ok(None)
}

fn function_args_outer_reference(
    args: &FunctionArguments,
    local_qualifiers: &BTreeSet<String>,
) -> Result<Option<String>> {
    match args {
        FunctionArguments::None => Ok(None),
        FunctionArguments::Subquery(query) => {
            query_outer_reference(query, &query_local_qualifiers(query)?)
        }
        FunctionArguments::List(list) => {
            for arg in &list.args {
                if let Some(identifier) = function_arg_outer_reference(arg, local_qualifiers)? {
                    return Ok(Some(identifier));
                }
            }
            for clause in &list.clauses {
                if let Some(identifier) =
                    function_arg_clause_outer_reference(clause, local_qualifiers)?
                {
                    return Ok(Some(identifier));
                }
            }
            Ok(None)
        }
    }
}

fn function_arg_outer_reference(
    arg: &FunctionArg,
    local_qualifiers: &BTreeSet<String>,
) -> Result<Option<String>> {
    match arg {
        FunctionArg::Named { arg, .. } | FunctionArg::Unnamed(arg) => {
            function_arg_expr_outer_reference(arg, local_qualifiers)
        }
        FunctionArg::ExprNamed { name, arg, .. } => {
            if let Some(identifier) = expr_outer_reference(name, local_qualifiers)? {
                return Ok(Some(identifier));
            }
            function_arg_expr_outer_reference(arg, local_qualifiers)
        }
    }
}

fn function_arg_expr_outer_reference(
    arg: &FunctionArgExpr,
    local_qualifiers: &BTreeSet<String>,
) -> Result<Option<String>> {
    match arg {
        FunctionArgExpr::Expr(expr) => expr_outer_reference(expr, local_qualifiers),
        FunctionArgExpr::QualifiedWildcard(_) | FunctionArgExpr::Wildcard => Ok(None),
    }
}

fn function_arg_clause_outer_reference(
    clause: &FunctionArgumentClause,
    local_qualifiers: &BTreeSet<String>,
) -> Result<Option<String>> {
    match clause {
        FunctionArgumentClause::OrderBy(order_by) => {
            for item in order_by {
                if let Some(identifier) = expr_outer_reference(&item.expr, local_qualifiers)? {
                    return Ok(Some(identifier));
                }
            }
            Ok(None)
        }
        FunctionArgumentClause::Limit(expr) => expr_outer_reference(expr, local_qualifiers),
        FunctionArgumentClause::Having(bound) => expr_outer_reference(&bound.1, local_qualifiers),
        _ => Ok(None),
    }
}

fn key_column_usage_row(
    table: &str,
    constraint_name: &str,
    column_name: &str,
    ordinal_position: usize,
    position_in_unique_constraint: Option<usize>,
    referenced: Option<(String, Option<String>)>,
) -> Map<String, Value> {
    let mut row = Map::new();
    row.insert(
        "constraint_catalog".to_string(),
        Value::String("def".to_string()),
    );
    row.insert(
        "constraint_schema".to_string(),
        Value::String("app".to_string()),
    );
    row.insert(
        "constraint_name".to_string(),
        Value::String(constraint_name.to_string()),
    );
    row.insert(
        "table_catalog".to_string(),
        Value::String("def".to_string()),
    );
    row.insert("table_schema".to_string(), Value::String("app".to_string()));
    row.insert("table_name".to_string(), Value::String(table.to_string()));
    row.insert(
        "column_name".to_string(),
        Value::String(column_name.to_string()),
    );
    row.insert(
        "ordinal_position".to_string(),
        Value::Number(Number::from(ordinal_position)),
    );
    row.insert(
        "position_in_unique_constraint".to_string(),
        position_in_unique_constraint
            .map(|pos| Value::Number(Number::from(pos)))
            .unwrap_or(Value::Null),
    );
    let (ref_schema, ref_table, ref_column) = match referenced {
        Some((table, column)) => (
            Value::String("app".to_string()),
            Value::String(table),
            column.map(Value::String).unwrap_or(Value::Null),
        ),
        None => (Value::Null, Value::Null, Value::Null),
    };
    row.insert("referenced_table_schema".to_string(), ref_schema);
    row.insert("referenced_table_name".to_string(), ref_table);
    row.insert("referenced_column_name".to_string(), ref_column);
    row
}
