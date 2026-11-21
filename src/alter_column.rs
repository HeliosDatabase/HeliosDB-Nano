//! `ALTER TABLE … ALTER COLUMN`: `SET DEFAULT` / `DROP DEFAULT`, `SET NOT NULL`,
//! `[SET DATA] TYPE … [USING …]`, `ADD GENERATED … AS IDENTITY`, `DROP IDENTITY` and
//! `SET STATISTICS`, as clauses of the statement's one write group (see the docs of
//! the parent module).
//!
//! * `SET DEFAULT` stores the expression the way `CREATE TABLE` stores one, after
//!   the check an INSERT using it would make (a stable expression is evaluated and
//!   cast to the column's type once, here). The rows that exist keep their values:
//!   a row older than the column still reads the column's missing value.
//! * `SET NOT NULL` reads every row as the statement has it and refuses (23502) on
//!   the first NULL.
//! * `TYPE` evaluates the new value of every row first (the cast, or `USING`), then
//!   builds the column's indexes over the new values and validates the table's CHECK
//!   constraints against the new rows; only then are the rows rewritten, with the
//!   schema, in the statement's group, and the new index trees take the place of
//!   the old ones before the schema is published. A value that cannot be cast names
//!   its row. A read `AS OF` a time before the change is refused (0A000), as it is
//!   across a rename: an older version holds values of the old type.

use std::collections::BTreeMap;
use std::sync::Arc;

use super::{AlterStatement, RowStep};
use crate::error::{NOT_NULL_VIOLATION_MARKER, UNSUPPORTED_STATEMENT_KIND_MARKER};
use crate::sql::logical_plan::AlterColumnAction;
use crate::storage;
use crate::storage::row_shape::ColumnFill;
use crate::{sql, ColumnStorageMode, DataType, EmbeddedDatabase, Error, Result, Tuple, Value};

/// A 0A000 refusal of one `ALTER COLUMN` form, in SQL words.
fn unsupported(what: impl std::fmt::Display, hint: &str) -> Error {
    Error::query_execution(format!("{what} {UNSUPPORTED_STATEMENT_KIND_MARKER}{hint}"))
}

fn is_integer(data_type: &DataType) -> bool {
    matches!(data_type, DataType::Int2 | DataType::Int4 | DataType::Int8)
}

/// Does `text` hold `word` as a whole identifier (ASCII case-insensitive)?
fn names_identifier(text: &str, word: &str) -> bool {
    if word.is_empty() {
        return false;
    }
    let (text, word) = (text.to_ascii_lowercase(), word.to_ascii_lowercase());
    let is_ident = |c: char| c.is_alphanumeric() || c == '_' || c == '$';
    let mut from = 0usize;
    while let Some(found) = text.get(from..).and_then(|rest| rest.find(&word)) {
        let start = from + found;
        let end = start + word.len();
        let before = text.get(..start).and_then(|s| s.chars().next_back());
        let after = text.get(end..).and_then(|s| s.chars().next());
        if !before.is_some_and(is_ident) && !after.is_some_and(is_ident) {
            return true;
        }
        from = end;
    }
    false
}

impl AlterStatement<'_> {
    /// The position of `column_name` in the table as the statement has it: the
    /// exact name first, then the name whatever its case.
    fn column_position(&self, column_name: &str) -> Result<usize> {
        self.schema
            .columns
            .iter()
            .position(|c| c.name == column_name)
            .or_else(|| {
                self.schema
                    .columns
                    .iter()
                    .position(|c| c.name.eq_ignore_ascii_case(column_name))
            })
            .ok_or_else(|| {
                Error::query_execution(format!(
                    "Column '{}' does not exist in table '{}'",
                    column_name, self.table
                ))
            })
    }
}

impl EmbeddedDatabase {
    /// One `ALTER COLUMN` clause against the statement so far.
    pub(super) fn alter_column<'e>(
        &'e self,
        st: &mut AlterStatement<'e>,
        column_name: &str,
        action: &AlterColumnAction,
    ) -> Result<()> {
        let position = st.column_position(column_name)?;
        match action {
            AlterColumnAction::SetDefault(expr) => self.alter_column_set_default(st, position, Some(expr.as_ref())),
            AlterColumnAction::DropDefault => self.alter_column_set_default(st, position, None),
            AlterColumnAction::SetDataType { data_type, using } => {
                self.alter_column_set_type(st, position, data_type, using.as_deref())
            }
            AlterColumnAction::AddIdentity => self.alter_column_add_identity(st, position),
            AlterColumnAction::DropIdentity { if_exists } => self.alter_column_drop_identity(st, position, *if_exists),
            AlterColumnAction::SetStatistics(target) => self.alter_column_set_statistics(st, position, *target),
        }
    }

    fn alter_column_is_identity(&self, st: &mut AlterStatement<'_>, name: &str) -> Result<bool> {
        let catalog = self.storage.catalog();
        Ok(st.identity_mut(&catalog)?.iter().any(|c| c.eq_ignore_ascii_case(name)))
    }

    /// `SET DEFAULT expr` / `DROP DEFAULT`.
    fn alter_column_set_default(
        &self,
        st: &mut AlterStatement<'_>,
        position: usize,
        default: Option<&sql::LogicalExpr>,
    ) -> Result<()> {
        let (name, data_type, current) = {
            let column = st
                .schema
                .get_column_at(position)
                .ok_or_else(|| Error::internal("column index out of bounds"))?;
            (
                column.name.clone(),
                column.data_type.clone(),
                column.default_expr.clone(),
            )
        };
        if crate::generated_columns::is_generated_default(current.as_deref()) {
            return Err(Error::query_execution(format!(
                "column \"{}\" of relation \"{}\" is a generated column: its expression is not a default \
                 ({})",
                name,
                st.table,
                crate::generated_columns::GENERATED_COLUMN_WRITE_MARKER
            )));
        }
        if default.is_some() && self.alter_column_is_identity(st, &name)? {
            return Err(Error::query_execution(format!(
                "column \"{}\" of relation \"{}\" is an identity column: ALTER COLUMN … SET DEFAULT on it {}; \
                 DROP IDENTITY first",
                name, st.table, UNSUPPORTED_STATEMENT_KIND_MARKER
            )));
        }
        // What an INSERT using it would find out, now: a stable default that cannot
        // be evaluated or cast to the column's type is refused, nothing written.
        ColumnFill::for_default(default, &data_type)?;
        let stored = Self::serialize_default_expr(&default.cloned());
        st.schema
            .get_column_at_mut(position)
            .ok_or_else(|| Error::internal("column index out of bounds"))?
            .default_expr = stored;
        Ok(())
    }

    /// `SET NOT NULL`'s scan: the first NULL in the column, in the rows as the
    /// statement has them, refuses it (23502), naming the column.
    pub(super) fn alter_refuse_null_in_column(&self, st: &AlterStatement<'_>, position: usize) -> Result<()> {
        let name = st
            .schema
            .get_column_at(position)
            .map(|c| c.name.clone())
            .unwrap_or_default();
        if st.own_rows {
            return Err(Error::ddl_over_staged_rows(
                "ALTER TABLE … ALTER COLUMN … SET NOT NULL",
                &st.table,
            ));
        }
        for row in self.alter_current_rows(st)? {
            if matches!(row.values.get(position), None | Some(Value::Null)) {
                return Err(Error::ConstraintViolation(format!(
                    "column \"{}\" of relation \"{}\" contains null values: ALTER COLUMN … SET NOT NULL {}",
                    name, st.table, NOT_NULL_VIOLATION_MARKER
                )));
            }
        }
        Ok(())
    }

    /// `ADD GENERATED { ALWAYS | BY DEFAULT } AS IDENTITY`. Generated keys come from
    /// the table's row-id allocator, which fills an omitted integer PRIMARY KEY: on
    /// any other column the clause would record an identity nothing fills, so it is
    /// refused.
    fn alter_column_add_identity(&self, st: &mut AlterStatement<'_>, position: usize) -> Result<()> {
        let (name, data_type, primary_key, has_default) = {
            let column = st
                .schema
                .get_column_at(position)
                .ok_or_else(|| Error::internal("column index out of bounds"))?;
            (
                column.name.clone(),
                column.data_type.clone(),
                column.primary_key,
                column.default_expr.is_some(),
            )
        };
        if self.alter_column_is_identity(st, &name)? {
            return Err(Error::query_execution(format!(
                "column \"{}\" of relation \"{}\" is already an identity column",
                name, st.table
            )));
        }
        if has_default {
            return Err(Error::query_execution(format!(
                "column \"{}\" of relation \"{}\" already has a default value",
                name, st.table
            )));
        }
        if !is_integer(&data_type) {
            return Err(Error::TypeConversion(format!(
                "identity column type must be smallint, integer, or bigint: column \"{}\" of relation \"{}\" is {}",
                name, st.table, data_type
            )));
        }
        if !primary_key {
            return Err(unsupported(
                format_args!(
                    "ALTER TABLE … ALTER COLUMN … ADD GENERATED … AS IDENTITY on \"{}\", which is not the primary \
                     key of \"{}\",",
                    name, st.table
                ),
                ": generated keys are assigned to an integer PRIMARY KEY column only",
            ));
        }
        let catalog = self.storage.catalog();
        st.identity_mut(&catalog)?.push(name);
        st.identity_changed = true;
        // As CREATE TABLE leaves an identity key: an omitted value reaches the
        // allocator as NULL.
        st.schema
            .get_column_at_mut(position)
            .ok_or_else(|| Error::internal("column index out of bounds"))?
            .nullable = true;
        Ok(())
    }

    /// `DROP IDENTITY [IF EXISTS]`: the column keeps its values and its key; an
    /// INSERT must give it a value from now on.
    fn alter_column_drop_identity(&self, st: &mut AlterStatement<'_>, position: usize, if_exists: bool) -> Result<()> {
        let (name, primary_key) = {
            let column = st
                .schema
                .get_column_at(position)
                .ok_or_else(|| Error::internal("column index out of bounds"))?;
            (column.name.clone(), column.primary_key)
        };
        if !self.alter_column_is_identity(st, &name)? {
            if if_exists {
                return Ok(());
            }
            return Err(Error::query_execution(format!(
                "column \"{}\" of relation \"{}\" is not an identity column",
                name, st.table
            )));
        }
        let catalog = self.storage.catalog();
        st.identity_mut(&catalog)?.retain(|c| !c.eq_ignore_ascii_case(&name));
        st.identity_changed = true;
        if primary_key {
            st.schema
                .get_column_at_mut(position)
                .ok_or_else(|| Error::internal("column index out of bounds"))?
                .nullable = false;
        }
        Ok(())
    }

    /// `SET STATISTICS n`: recorded and shown as `pg_attribute.attstattarget`. The
    /// planner's statistics are collected the same way whatever the target.
    fn alter_column_set_statistics(
        &self,
        st: &mut AlterStatement<'_>,
        position: usize,
        target: Option<i32>,
    ) -> Result<()> {
        let name = st
            .schema
            .get_column_at(position)
            .map(|c| c.name.clone())
            .ok_or_else(|| Error::internal("column index out of bounds"))?;
        if let Some(target) = target {
            if !(-1..=10_000).contains(&target) {
                return Err(Error::query_execution(format!(
                    "statistics target {target} is out of range: it must be between 0 and 10000, or -1 for the \
                     default"
                )));
            }
        }
        if st.statistics_targets.is_none() {
            st.statistics_targets = Some(self.storage.catalog().column_statistics_targets(&st.table)?);
        }
        let targets = st
            .statistics_targets
            .as_mut()
            .ok_or_else(|| Error::internal("ALTER TABLE: the statistics targets were not loaded"))?;
        let before = targets.get(&name).copied();
        let after = target.filter(|target| *target >= 0);
        match after {
            Some(target) => {
                targets.insert(name, target);
            }
            None => {
                targets.remove(&name);
            }
        }
        if after != before {
            st.statistics_changed = true;
        }
        Ok(())
    }

    /// The statistics targets after the statement's renames and drops: a renamed
    /// column keeps its target under the new name, a dropped column's entry goes.
    pub(super) fn alter_settle_statistics_targets(
        &self,
        st: &mut AlterStatement<'_>,
        net_renames: &[(String, String)],
    ) -> Result<()> {
        if net_renames.is_empty() && st.drops.is_empty() {
            return Ok(());
        }
        if st.statistics_targets.is_none() {
            st.statistics_targets = Some(self.storage.catalog().column_statistics_targets(&st.table)?);
        }
        let Some(targets) = st.statistics_targets.as_mut() else {
            return Ok(());
        };
        if targets.is_empty() {
            return Ok(());
        }
        let mut settled: BTreeMap<String, i32> = BTreeMap::new();
        for (column, target) in targets.iter() {
            let name = net_renames
                .iter()
                .find(|(old, _)| old == column)
                .map_or(column, |(_, new)| new);
            if st.schema.columns.iter().any(|c| &c.name == name) {
                settled.insert(name.clone(), *target);
            }
        }
        if settled != *targets {
            *targets = settled;
            st.statistics_changed = true;
        }
        Ok(())
    }

    /// `[SET DATA] TYPE new_type [USING expr]`. See the module docs.
    fn alter_column_set_type<'e>(
        &'e self,
        st: &mut AlterStatement<'e>,
        position: usize,
        new_type: &DataType,
        using: Option<&sql::LogicalExpr>,
    ) -> Result<()> {
        let column = st
            .schema
            .get_column_at(position)
            .cloned()
            .ok_or_else(|| Error::internal("column index out of bounds"))?;
        let table = st.table.clone();
        let clause = "ALTER TABLE … ALTER COLUMN … TYPE";
        if column.data_type == *new_type && using.is_none() {
            return Ok(());
        }
        if self.storage.is_branch_active() {
            return Err(Error::query_execution(format!("{clause} must run on the main branch")));
        }
        if column.storage_mode != ColumnStorageMode::Default {
            return Err(unsupported(
                format_args!(
                    "{clause} of column \"{}\", which is kept in {:?} storage,",
                    column.name, column.storage_mode
                ),
                ": ALTER COLUMN … SET STORAGE DEFAULT first",
            ));
        }
        if crate::generated_columns::is_generated_default(column.default_expr.as_deref()) {
            return Err(unsupported(
                format_args!("{clause} of the generated column \"{}\"", column.name),
                ": drop the column and add it with the new type",
            ));
        }
        if matches!(new_type, DataType::Vector(_)) || matches!(column.data_type, DataType::Vector(_)) {
            return Err(unsupported(
                format_args!("{clause} to or from a vector type (column \"{}\")", column.name),
                "",
            ));
        }
        if st.own_rows {
            return Err(Error::ddl_over_staged_rows(clause, &table));
        }
        if !st.column_renames.is_empty() {
            return Err(unsupported(
                format_args!("{clause} after RENAME COLUMN in one ALTER TABLE statement"),
                ": run the type change as a statement of its own",
            ));
        }
        if st.copies > 0 {
            return Err(unsupported(
                format_args!("{clause} of table \"{table}\" while a branch reads its rows"),
                ": drop or merge the branch first",
            ));
        }
        let catalog = self.storage.catalog();
        if self.alter_column_is_identity(st, &column.name)? && !is_integer(new_type) {
            return Err(Error::TypeConversion(format!(
                "identity column type must be smallint, integer, or bigint: column \"{}\" of relation \"{}\" cannot \
                 become {}",
                column.name, table, new_type
            )));
        }

        // What reads the column by its type and is not rewritten with it.
        for other in &st.schema.columns {
            if crate::generated_columns::generated_reads(other.default_expr.as_deref(), &column.name) {
                return Err(unsupported(
                    format_args!(
                        "{clause} of column \"{}\", which the generated column \"{}\" reads,",
                        column.name, other.name
                    ),
                    ": drop the generated column first",
                ));
            }
        }
        let views = self.storage.view_catalog();
        let bare_table = table.rsplit('.').next().unwrap_or(&table);
        for view_name in views.list_views()? {
            let Ok(view) = views.get_view(&view_name) else {
                continue;
            };
            if names_identifier(&view.query_sql, bare_table)
                && (names_identifier(&view.query_sql, &column.name) || view.query_sql.contains('*'))
            {
                return Err(unsupported(
                    format_args!(
                        "{clause} of column \"{}\" of relation \"{}\", which view \"{}\" uses,",
                        column.name, table, view_name
                    ),
                    ": drop the view, change the type, then create the view again",
                ));
            }
        }
        // A foreign key compares the two columns' values: between integer types a
        // value keeps its meaning; anything else would have to be validated again.
        let integers = is_integer(&column.data_type) && is_integer(new_type);
        let in_own_fk = st
            .constraints_mut(&catalog)?
            .foreign_keys
            .iter()
            .find(|fk| fk.columns.iter().any(|c| c.eq_ignore_ascii_case(&column.name)))
            .map(|fk| fk.name.clone());
        let in_inbound_fk = catalog
            .get_referencing_fks(&table)?
            .into_iter()
            .find(|fk| {
                fk.references_columns
                    .iter()
                    .any(|c| c.eq_ignore_ascii_case(&column.name))
            })
            .map(|fk| fk.name);
        if let Some(fk) = in_own_fk.or(in_inbound_fk).filter(|_| !integers) {
            return Err(unsupported(
                format_args!(
                    "{clause} of column \"{}\", which foreign key \"{}\" uses, to a type that is not an integer type",
                    column.name, fk
                ),
                ": drop the constraint, change the type, then add the constraint again",
            ));
        }
        // An index this registry does not rebuild (a vector index, an index over an
        // expression): its entries were computed from the old values.
        for (index_name, definition) in catalog.list_index_definitions()? {
            if definition.table_name != table {
                continue;
            }
            let reads_in_expression = definition.options.iter().any(|option| {
                matches!(option, sql::logical_plan::IndexOption::Expression { columns, .. }
                    if columns.iter().any(|c| c.eq_ignore_ascii_case(&column.name)))
            });
            let names_column = definition.column_name.eq_ignore_ascii_case(&column.name);
            let is_art = self.storage.art_indexes().index_exists(&index_name);
            if reads_in_expression || (names_column && !is_art) {
                return Err(unsupported(
                    format_args!(
                        "{clause} of column \"{}\", which index \"{}\" is built over,",
                        column.name, index_name
                    ),
                    ": drop the index, change the type, then create the index again",
                ));
            }
        }
        // The default must still give a value of the column's type.
        if let Some(json) = &column.default_expr {
            let default: sql::LogicalExpr = serde_json::from_str(json)
                .map_err(|e| Error::storage(format!("the default of column \"{}\": {e}", column.name)))?;
            ColumnFill::for_default(Some(&default), new_type).map_err(|e| {
                Error::TypeConversion(format!(
                    "default for column \"{}\" cannot be cast automatically to type {}: {}",
                    column.name, new_type, e
                ))
            })?;
        }

        // Every row's new value, before anything is written.
        let rows = self.alter_current_rows(st)?;
        let evaluator = sql::Evaluator::new(Arc::new(st.schema.clone()));
        let max_bytes = self.storage.config().storage.max_atomic_rewrite_bytes;
        let mut new_rows: Vec<Tuple> = Vec::with_capacity(rows.len());
        for mut row in rows {
            let row_id = row
                .row_id
                .ok_or_else(|| Error::internal("ALTER COLUMN … TYPE: a stored row has no row id"))?;
            let old = row.values.get(position).cloned().unwrap_or(Value::Null);
            let computed = match using {
                Some(expr) => evaluator.evaluate(expr, &row),
                None => Ok(old.clone()),
            }
            .and_then(|value| {
                if matches!(value, Value::Null) || value.data_type() == *new_type {
                    Ok(value)
                } else {
                    evaluator.cast_value(value, new_type)
                }
            });
            let value = computed.map_err(|e| {
                Error::TypeConversion(format!(
                    "column \"{}\" of relation \"{}\" cannot be changed to type {}: row {} holds {}: {}",
                    column.name, table, new_type, row_id, old, e
                ))
            })?;
            if matches!(value, Value::Null) && !column.nullable && !column.primary_key {
                return Err(Error::ConstraintViolation(format!(
                    "column \"{}\" of relation \"{}\" would hold a null value in row {}: the new value {}",
                    column.name, table, row_id, NOT_NULL_VIOLATION_MARKER
                )));
            }
            st.per_row_bytes = st
                .per_row_bytes
                .saturating_add(bincode::serialized_size(&row).unwrap_or(0).saturating_mul(2));
            if st.per_row_bytes > max_bytes {
                return Err(Error::atomic_alter_too_large(
                    &table,
                    "ALTER COLUMN … TYPE",
                    st.per_row_bytes,
                    max_bytes,
                ));
            }
            if let Some(slot) = row.values.get_mut(position) {
                *slot = value.clone();
            }
            st.per_row.entry(row_id).or_default().push(value);
            new_rows.push(row);
        }

        // The table as it will be.
        {
            let changed = st
                .schema
                .get_column_at_mut(position)
                .ok_or_else(|| Error::internal("column index out of bounds"))?;
            changed.data_type = new_type.clone();
        }
        if let Some(missing) = st.missing.remove(&column.name) {
            // Every row is rewritten at the table's width, so no row reads the
            // missing value again; kept in the new type for the rows of a branch.
            if let Ok(cast) = evaluator.cast_value(missing, new_type) {
                st.missing.insert(column.name.clone(), cast);
            }
        }
        // CHECK constraints, against the new rows.
        let constraints = st.constraints_mut(&catalog)?.clone();
        if !constraints.check_constraints.is_empty() {
            for row in &new_rows {
                self.validate_check_constraints(&constraints, &st.schema, &row.values)
                    .map_err(|e| {
                        Error::constraint_violation(format!(
                            "check constraint of relation \"{}\" is violated by row {} after the type change: {}",
                            table,
                            row.row_id.unwrap_or_default(),
                            e
                        ))
                    })?;
            }
        }
        // The column's indexes, over the new values; a PRIMARY KEY or UNIQUE one
        // refuses two rows the new type makes equal (23505).
        let art = Arc::clone(self.storage.art_indexes());
        let mut rebuilt: BTreeMap<String, storage::AdaptiveRadixTree> = BTreeMap::new();
        for (index_name, _kind, columns) in art.list_table_indexes(&table) {
            if !columns.iter().any(|c| c.eq_ignore_ascii_case(&column.name)) {
                continue;
            }
            if st.indexes_to_drop.contains(&index_name) || st.created_indexes.contains(&index_name) {
                continue;
            }
            let tree = art.build_index_tree(&index_name, &st.schema, &new_rows).map_err(|e| {
                Error::constraint_violation(format!(
                    "could not rebuild index \"{}\" of relation \"{}\" for the new type of column \"{}\": {}",
                    index_name, table, column.name, e
                ))
            })?;
            rebuilt.insert(index_name, tree);
        }
        st.publish_first.push(Box::new(move || {
            for (index_name, tree) in rebuilt {
                if let Err(e) = art.install_index_tree(&index_name, tree) {
                    tracing::warn!(
                        "ALTER COLUMN … TYPE: index '{}' could not take its rebuilt tree: {} — REINDEX it",
                        index_name,
                        e
                    );
                }
            }
        }));
        st.steps.push(RowStep::ReplacePerRow(position));
        st.shape_changed = true;
        st.type_changed = true;
        Ok(())
    }
}
