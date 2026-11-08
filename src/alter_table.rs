//! Spec R5.5 (`docs/plans/specs/R5_5_drop_column_and_multi_alter.md`, parent R5 decision
//! 5 and design §8 F3): one `ALTER TABLE` statement — every clause of it — is ONE write
//! group, on both executor families.
//!
//! # Why
//!
//! Each clause used to write on its own: `DROP COLUMN` wrote the schema, then every row
//! one group at a time, then the constraint records and the columnar side data, so a
//! reader or a crash in between saw a schema without the column over rows that still
//! had it, every later column's value under the wrong name; and a statement of several
//! clauses (`ADD c INT DEFAULT 1 CHECK (c > 5)`, `ADD a, DROP b`) kept the clauses before
//! one that failed. `AGENTS.md` promises "Multi-op `ALTER TABLE` is atomic per
//! statement".
//!
//! # How
//!
//! [`AlterStatement`] is the table as the statement has it so far, in memory: the
//! schema, the missing values, the constraint and identity records, the other tables'
//! records a clause touches, and what every clause does to the rows ([`RowStep`]).
//! Each clause is asked against that, so it sees the clauses before it (a CHECK sees the
//! column an earlier clause added, with its value in every row) and refuses before
//! anything is written. Nothing is staged until the last clause is through; then
//! [`EmbeddedDatabase::finish_alter`] stages the whole statement into one `DdlBatch`:
//! the rows' new images when a clause rewrites rows (`DROP COLUMN`, a volatile
//! default), each with a version at the statement's one timestamp, up to `[storage]
//! max_atomic_rewrite_bytes` (54000 above it, nothing written); the catalog records
//! (`Catalog::stage_altered_table`), with the layout record a read of an older
//! version takes it across (`storage::table_layout`); the constraint, identity and
//! columnar records. One write; the caches and the index registry are published after
//! it, and a statement that fails publishes nothing and takes back the indexes it
//! registered (hidden) while it ran.
//!
//! A volatile default is evaluated once per row (it used to be twice in a statement of
//! several clauses: once to ask its refusals, once to write). The rows of the
//! transaction the statement runs in are re-staged with what the statement did to the
//! table, after the write (DDL is not transactional here).
//!
//! `RENAME TO` is a statement of its own: combined with other clauses it is refused
//! (0A000), as PostgreSQL refuses the combination.

use std::collections::{BTreeMap, HashMap};

use crate::storage::alter_batch::{AlteredTable, ColumnMigration, RewriteLimit};
use crate::storage::row_shape::{ColumnFill, RowShape};
use crate::storage::{self, Catalog, DdlBatch, Transaction};
use crate::{sql, Column, ColumnStorageMode, EmbeddedDatabase, Error, Result, Schema, Tuple, Value};

/// What a clause does to every row, in order, on top of the row as it is stored,
/// shaped to the table's width when the statement began.
#[derive(Debug, Clone)]
enum RowStep {
    /// A column appended with one value for every row (none: NULL; a stable
    /// default: its value, which is also the column's missing value).
    Append(Value),
    /// A column appended with each row's own value (a volatile default): the next of
    /// the row's values the statement evaluated.
    AppendPerRow,
    /// The column at this position dropped.
    Remove(usize),
}

/// Apply `steps` to `tuple`, `per_row` being the row's evaluated values.
fn apply_steps(steps: &[RowStep], tuple: &mut Tuple, per_row: Option<&Vec<Value>>) -> Result<()> {
    let mut next = 0usize;
    for step in steps {
        match step {
            RowStep::Append(value) => tuple.values.push(value.clone()),
            RowStep::AppendPerRow => {
                let value = per_row.and_then(|values| values.get(next)).ok_or_else(|| {
                    Error::internal("ALTER TABLE: a row was written during the statement that holds its table")
                })?;
                tuple.values.push(value.clone());
                next += 1;
            }
            RowStep::Remove(position) => {
                if *position < tuple.values.len() {
                    tuple.values.remove(*position);
                }
            }
        }
    }
    Ok(())
}

/// How many constraints a record holds.
fn constraint_count(constraints: &sql::TableConstraints) -> usize {
    constraints.foreign_keys.len() + constraints.check_constraints.len() + constraints.unique_constraints.len()
}

/// Do two column lists name the same set (case-insensitive)?
fn same_column_set(a: &[String], b: &[String]) -> bool {
    a.len() == b.len() && a.iter().all(|x| b.iter().any(|y| y.eq_ignore_ascii_case(x)))
}

/// One `ALTER TABLE` statement on main's table, as its clauses have left it so far.
/// See the module docs.
pub struct AlterStatement<'e> {
    table: String,
    txn: Option<&'e Transaction>,
    ddl: DdlBatch<'e>,
    /// The table's schema when the statement began.
    base: Schema,
    /// Shapes a stored row to `base`'s width with the missing values of then.
    base_shape: RowShape,
    base_missing: HashMap<String, Value>,
    schema: Schema,
    missing: HashMap<String, Value>,
    constraints: Option<sql::TableConstraints>,
    constraints_changed: bool,
    /// Other tables' constraint records a clause changed (foreign keys that reference
    /// a renamed or a dropped column), as changed.
    other_constraints: BTreeMap<String, sql::TableConstraints>,
    identity: Option<Vec<String>>,
    identity_changed: bool,
    column_renames: Vec<(String, String)>,
    steps: Vec<RowStep>,
    /// The values of the `AppendPerRow` steps, by row id, in step order.
    per_row: HashMap<u64, Vec<Value>>,
    /// The same for the transaction's staged rows, by staged key.
    own_per_row: HashMap<Vec<u8>, Vec<Value>>,
    /// Bytes the per-row evaluations have seen (an estimate of the rewrite, so a table
    /// far above the cap is refused before every row is evaluated).
    per_row_bytes: u64,
    drops: Vec<(usize, usize)>,
    shape_changed: bool,
    /// UNIQUE / PRIMARY KEY constraints `DROP CONSTRAINT` retired: their indexes and
    /// column flags are settled against what the statement leaves.
    retired_uniques: Vec<sql::UniqueConstraint>,
    /// Indexes to drop after the write.
    indexes_to_drop: Vec<String>,
    /// Indexes this statement registered (hidden until the write).
    created_indexes: Vec<String>,
    /// Published before the schema: the live index entries a rename repoints.
    publish_first: Vec<Box<dyn FnOnce() + 'e>>,
    /// Published after the schema and the constraints: what reads them.
    publish_after: Vec<Box<dyn FnOnce() + 'e>>,
    /// The transaction this runs in has staged rows of the table.
    own_rows: bool,
    /// Live branches that copy main's rows of the table as they change (phase B4).
    copies: u64,
}

impl<'e> AlterStatement<'e> {
    /// The transaction, when it has staged rows of the table.
    fn own_txn(&self) -> Option<&'e Transaction> {
        self.txn.filter(|_| self.own_rows)
    }

    fn rewrites_rows(&self) -> bool {
        self.steps
            .iter()
            .any(|step| matches!(step, RowStep::Remove(_) | RowStep::AppendPerRow))
    }

    fn constraints_mut(&mut self, catalog: &Catalog<'_>) -> Result<&mut sql::TableConstraints> {
        if self.constraints.is_none() {
            self.constraints = Some(catalog.load_table_constraints(&self.table)?);
        }
        self.constraints
            .as_mut()
            .ok_or_else(|| Error::internal("ALTER TABLE: the constraint record was not loaded"))
    }

    fn identity_mut(&mut self, catalog: &Catalog<'_>) -> Result<&mut Vec<String>> {
        if self.identity.is_none() {
            self.identity = Some(catalog.list_identity_columns(&self.table)?);
        }
        self.identity
            .as_mut()
            .ok_or_else(|| Error::internal("ALTER TABLE: the identity record was not loaded"))
    }

    /// Another table's constraint record as the statement has it.
    fn other_constraints(&self, catalog: &Catalog<'_>, other: &str) -> Result<sql::TableConstraints> {
        match self.other_constraints.get(other) {
            Some(constraints) => Ok(constraints.clone()),
            None => catalog.load_table_constraints(other),
        }
    }
}

/// The table every clause of an `ALTER TABLE` names.
fn clause_table(plan: &sql::LogicalPlan) -> Option<&str> {
    use sql::LogicalPlan as P;
    match plan {
        P::AlterTableAddColumn { table_name, .. }
        | P::AlterTableDropColumn { table_name, .. }
        | P::AlterTableRenameColumn { table_name, .. }
        | P::AlterTableAlterColumnNullability { table_name, .. }
        | P::AlterTableAddUnique { table_name, .. }
        | P::AlterTableAddPrimaryKey { table_name, .. }
        | P::AlterTableAddCheck { table_name, .. }
        | P::AlterTableDropConstraint { table_name, .. }
        | P::AlterTableAddForeignKey { table_name, .. }
        | P::AlterTableRename { table_name, .. } => Some(table_name.as_str()),
        _ => None,
    }
}

impl EmbeddedDatabase {
    /// Spec R5.5: run `clauses`, one `ALTER TABLE` statement on main's table, as ONE
    /// write group (see the module docs). Returns the rows rewritten.
    pub(crate) fn execute_alter_statement(
        &self,
        clauses: &[sql::LogicalPlan],
        txn: Option<&Transaction>,
    ) -> Result<u64> {
        if let [sql::LogicalPlan::AlterTableRename {
            table_name,
            new_table_name,
        }] = clauses
        {
            return self.alter_table_rename_to(table_name, new_table_name, txn);
        }
        let Some(table) = clauses.first().and_then(clause_table) else {
            return Ok(0);
        };
        for clause in clauses {
            match clause_table(clause) {
                Some(named) if named == table => {}
                Some(named) => {
                    return Err(Error::internal(format!(
                        "ALTER TABLE {table}: a clause names another table, {named}"
                    )))
                }
                None => {
                    return Err(Error::internal(format!(
                        "execute_alter_statement called with non-ALTER plan: {:?}",
                        clause.plan_type_name()
                    )))
                }
            }
            if clauses.len() > 1 && matches!(clause, sql::LogicalPlan::AlterTableRename { .. }) {
                return Err(Error::query_execution(format!(
                    "ALTER TABLE … RENAME TO together with other clauses {}: run the rename as a statement of \
                     its own",
                    crate::error::UNSUPPORTED_STATEMENT_KIND_MARKER
                )));
            }
        }
        let touches_inbound = clauses.iter().any(|clause| {
            matches!(
                clause,
                sql::LogicalPlan::AlterTableDropColumn { .. } | sql::LogicalPlan::AlterTableRenameColumn { .. }
            )
        });
        let mut statement = self.begin_alter(table, txn, touches_inbound)?;
        for clause in clauses {
            self.alter_clause(&mut statement, clause)?;
        }
        self.finish_alter(statement)
    }

    fn begin_alter<'e>(
        &'e self,
        table: &str,
        txn: Option<&'e Transaction>,
        touches_inbound: bool,
    ) -> Result<AlterStatement<'e>> {
        let catalog = self.storage.catalog();
        // sprinter 6bc4a4040eb7 (phase B3): MAIN's catalog only; a table an unfinished
        // merge holds is refused (55006). Held until the publication.
        let guard = self.storage.main_catalog_write(&[table], "ALTER TABLE")?;
        if !catalog.main_table_exists(table)? {
            return Err(Error::query_execution(format!("Table '{}' does not exist", table)));
        }
        // The plan guard takes known neighbours in sorted order. Recheck under the
        // parent lock as well: an FK may have been added between plan inspection
        // and acquiring it. Nothing can add another inbound FK while this lock is
        // held, and every record is locked before its read, not just when staged.
        // Take these guards before opening DdlBatch, since B4 may freeze a child's
        // definition while the guard is acquired.
        let inbound_guard = if touches_inbound {
            let mut tables: Vec<String> = catalog
                .get_referencing_fks(table)?
                .into_iter()
                .map(|fk| fk.table_name)
                .collect();
            tables.sort_unstable();
            tables.dedup();
            let tables: Vec<&str> = tables.iter().map(String::as_str).collect();
            Some(
                self.storage
                    .main_catalog_write(&tables, "ALTER TABLE inbound foreign keys")?,
            )
        } else {
            None
        };
        let base = catalog.get_table_schema(table)?;
        let base_shape = self.storage.row_shape(table, base.columns.len())?;
        let base_missing = self.storage.stored_missing_values(table)?;
        let mut ddl = DdlBatch::new(&self.storage);
        ddl.hold_until_published(guard);
        if let Some(guard) = inbound_guard {
            ddl.hold_until_published(guard);
        }
        Ok(AlterStatement {
            table: table.to_string(),
            txn,
            ddl,
            schema: base.clone(),
            base,
            base_shape,
            missing: base_missing.clone(),
            base_missing,
            constraints: None,
            constraints_changed: false,
            other_constraints: BTreeMap::new(),
            identity: None,
            identity_changed: false,
            column_renames: Vec::new(),
            steps: Vec::new(),
            per_row: HashMap::new(),
            own_per_row: HashMap::new(),
            per_row_bytes: 0,
            drops: Vec::new(),
            shape_changed: false,
            retired_uniques: Vec::new(),
            indexes_to_drop: Vec::new(),
            created_indexes: Vec::new(),
            publish_first: Vec::new(),
            publish_after: Vec::new(),
            own_rows: txn.is_some_and(|txn| txn.has_staged_rows(table)),
            copies: self.storage.branches_copying_main_rows(table)?,
        })
    }

    fn alter_clause<'e>(&'e self, st: &mut AlterStatement<'e>, clause: &sql::LogicalPlan) -> Result<()> {
        use sql::LogicalPlan as P;
        match clause {
            P::AlterTableAddColumn {
                column_def,
                if_not_exists,
                ..
            } => self.alter_add_column(st, column_def, *if_not_exists),
            P::AlterTableDropColumn {
                column_name,
                if_exists,
                cascade,
                ..
            } => self.alter_drop_column(st, column_name, *if_exists, *cascade),
            P::AlterTableRenameColumn {
                old_column_name,
                new_column_name,
                ..
            } => self.alter_rename_column(st, old_column_name, new_column_name),
            P::AlterTableAlterColumnNullability {
                column_name, nullable, ..
            } => Self::alter_set_nullability(st, column_name, *nullable),
            P::AlterTableAddUnique {
                constraint_name,
                columns,
                ..
            } => self.alter_add_unique(st, constraint_name, columns),
            P::AlterTableAddPrimaryKey {
                constraint_name,
                columns,
                ..
            } => self.alter_add_primary_key(st, constraint_name, columns),
            P::AlterTableAddCheck {
                constraint_name,
                expression,
                ..
            } => self.alter_add_check(st, constraint_name, expression),
            P::AlterTableDropConstraint {
                constraint_name,
                if_exists,
                ..
            } => self.alter_drop_constraint(st, constraint_name, *if_exists),
            P::AlterTableAddForeignKey { .. } => self.alter_add_foreign_key(st, clause),
            other => Err(Error::internal(format!(
                "alter_clause called with {:?}",
                other.plan_type_name()
            ))),
        }
    }

    /// Every row of the table as the statement has it so far, values resolved (a
    /// dictionary, content-addressed or columnar reference is its value), with row
    /// ids: what a constraint clause validates and indexes.
    fn alter_current_rows(&self, st: &AlterStatement<'_>) -> Result<Vec<Tuple>> {
        let mut rows = self.storage.scan_table_with_schema(&st.table, &st.base)?;
        for row in &mut rows {
            let per_row = row.row_id.and_then(|row_id| st.per_row.get(&row_id));
            apply_steps(&st.steps, row, per_row)?;
        }
        Ok(rows)
    }

    // ---------------------------------------------------------------------
    // Columns
    // ---------------------------------------------------------------------

    /// `ADD COLUMN`. Spec R5.2's rule: a stable default (a literal, or an expression of
    /// stable functions) is evaluated once, by the rule an INSERT uses, and becomes
    /// the column's missing value: no row is rewritten for it. A volatile default is
    /// evaluated once per row, here, and the rows are rewritten. NOT NULL with nothing
    /// to give the rows that exist is 23502.
    fn alter_add_column(
        &self,
        st: &mut AlterStatement<'_>,
        column_def: &sql::logical_plan::ColumnDef,
        if_not_exists: bool,
    ) -> Result<()> {
        if st.schema.columns.iter().any(|c| c.name == column_def.name) {
            if if_not_exists {
                return Ok(());
            }
            return Err(Error::query_execution(format!(
                "Column '{}' already exists in table '{}'",
                column_def.name, st.table
            )));
        }
        let new_column = Column {
            name: column_def.name.clone(),
            data_type: column_def.data_type.clone(),
            nullable: !column_def.not_null,
            primary_key: column_def.primary_key,
            source_table: None,
            source_table_name: Some(st.table.clone()),
            default_expr: Self::serialize_default_expr(&column_def.default),
            unique: column_def.unique,
            storage_mode: column_def.storage_mode,
        };
        let not_null = !new_column.nullable;
        let fill = ColumnFill::for_default(column_def.default.as_ref(), &column_def.data_type)?;

        // The transaction's own staged rows, before anything else: they are rows of the
        // table as much as the committed ones.
        if let Some(txn) = st.own_txn() {
            if not_null && matches!(fill, ColumnFill::Constant(Value::Null)) {
                return Err(Error::add_column_null_on_existing_rows(&st.table, &column_def.name));
            }
            if matches!(fill, ColumnFill::PerRow(_)) {
                // A ROLLBACK TO would bring back images this statement cannot reach.
                if txn.has_savepoints() {
                    return Err(Error::ddl_over_staged_rows(
                        "ALTER TABLE … ADD COLUMN with a volatile default, under a savepoint,",
                        &st.table,
                    ));
                }
                let _effects = storage::ddl_batch::allow_side_write(storage::ddl_batch::VOLATILE_DEFAULT_EFFECTS);
                let values = Self::own_row_fills(
                    txn,
                    &st.table,
                    &[(column_def.name.as_str(), &column_def.data_type, not_null, &fill)],
                )?;
                for (key, row_values) in values {
                    st.own_per_row.entry(key).or_default().extend(row_values);
                }
            }
        }

        match &fill {
            ColumnFill::Constant(Value::Null) => {
                if not_null && self.storage.table_has_rows(&st.table)? {
                    return Err(Error::add_column_null_on_existing_rows(&st.table, &column_def.name));
                }
                st.missing.remove(&column_def.name);
                st.steps.push(RowStep::Append(Value::Null));
            }
            ColumnFill::Constant(value) => {
                st.missing.insert(column_def.name.clone(), value.clone());
                st.steps.push(RowStep::Append(value.clone()));
            }
            ColumnFill::PerRow(_) => {
                self.alter_evaluate_per_row(st, &column_def.name, &column_def.data_type, not_null, &fill)?;
                st.missing.remove(&column_def.name);
                st.steps.push(RowStep::AppendPerRow);
            }
        }
        st.schema.columns.push(new_column);
        Ok(())
    }

    /// A volatile default's value for every committed row, once each. Its rows will be
    /// rewritten, so the estimate of the rewrite grows by each row's size (an image and
    /// a version, with the copies of the live branches) and a table well above the cap
    /// is refused before all of it is evaluated.
    fn alter_evaluate_per_row(
        &self,
        st: &mut AlterStatement<'_>,
        column: &str,
        data_type: &crate::DataType,
        not_null: bool,
        fill: &ColumnFill,
    ) -> Result<()> {
        let max_bytes = self.storage.config().storage.max_atomic_rewrite_bytes;
        let prefix = format!("data:{}:", st.table);
        let mut read_opts = rocksdb::ReadOptions::default();
        read_opts.set_total_order_seek(true);
        let db = self.storage.db();
        let rows = db.iterator_opt(
            rocksdb::IteratorMode::From(prefix.as_bytes(), rocksdb::Direction::Forward),
            read_opts,
        );
        let _effects = storage::ddl_batch::allow_side_write(storage::ddl_batch::VOLATILE_DEFAULT_EFFECTS);
        for item in rows {
            let (key, raw) = item.map_err(|e| Error::storage(format!("Iterator error: {e}")))?;
            if !key.starts_with(prefix.as_bytes()) {
                break;
            }
            let Some(row_id) = storage::StorageEngine::parse_row_id_after_prefix(&key, prefix.len()) else {
                continue;
            };
            let value = fill.next_value(data_type)?;
            if not_null && matches!(value, Value::Null) {
                return Err(Error::add_column_null_on_existing_rows(&st.table, column));
            }
            st.per_row.entry(row_id).or_default().push(value);
            let row_bytes = ((key.len() + raw.len()) as u64).saturating_mul(2);
            st.per_row_bytes = st
                .per_row_bytes
                .saturating_add(row_bytes.saturating_add(((key.len() + raw.len()) as u64).saturating_mul(st.copies)));
            if st.per_row_bytes > max_bytes {
                return Err(Error::atomic_rewrite_too_large(&st.table, st.per_row_bytes, max_bytes));
            }
        }
        Ok(())
    }

    /// `DROP COLUMN`: the column's value leaves every row, its missing value, the
    /// constraints and constraint-owned indexes naming it, its identity entry and its
    /// columnar side data go, in the statement's one group. Refused without CASCADE
    /// when another table's foreign key references it, or when it is the primary key.
    fn alter_drop_column(
        &self,
        st: &mut AlterStatement<'_>,
        column_name: &str,
        if_exists: bool,
        cascade: bool,
    ) -> Result<()> {
        let Some(position) = st.schema.columns.iter().position(|c| c.name == *column_name) else {
            if if_exists {
                return Ok(());
            }
            return Err(Error::query_execution(format!(
                "Column '{}' does not exist in table '{}'",
                column_name, st.table
            )));
        };
        let column = st
            .schema
            .columns
            .get(position)
            .cloned()
            .ok_or_else(|| Error::internal("column index out of bounds"))?;
        if column.primary_key && !cascade {
            return Err(Error::query_execution(format!(
                "Cannot drop primary key column '{}' without CASCADE",
                column_name
            )));
        }

        // Refuse BEFORE anything changes: another table's foreign key depends on the
        // column and no CASCADE was given (PostgreSQL's rule, and the fail-closed one).
        let catalog = self.storage.catalog();
        let mut inbound: Vec<(String, sql::TableConstraints)> = Vec::new();
        let mut dependents: Vec<String> = Vec::new();
        for other in catalog.list_tables()? {
            if other == st.table {
                continue;
            }
            let mut constraints = st.other_constraints(&catalog, &other)?;
            for fk in &constraints.foreign_keys {
                if fk.references_table == st.table
                    && fk
                        .references_columns
                        .iter()
                        .any(|c| c.eq_ignore_ascii_case(column_name))
                {
                    dependents.push(format!("constraint {} on table {}", fk.name, other));
                }
            }
            if Catalog::retire_inbound_references(&st.table, column_name, &mut constraints) > 0 {
                inbound.push((other, constraints));
            }
        }
        if !dependents.is_empty() && !cascade {
            return Err(Error::query_execution(format!(
                "cannot drop column \"{}\" of relation \"{}\" because other objects depend on it: {}. \
                 Use DROP COLUMN … CASCADE, or drop those constraints first.",
                column_name,
                st.table,
                dependents.join(", ")
            )));
        }

        // Spec R5.2 (F5): the rows this transaction staged lose the column too, or
        // they would commit one value too wide, every later column shifted by one.
        // A ROLLBACK TO would bring back images this statement cannot reach.
        if st.own_txn().is_some_and(Transaction::has_savepoints) {
            return Err(Error::ddl_over_staged_rows(
                "ALTER TABLE … DROP COLUMN under a savepoint",
                &st.table,
            ));
        }

        // The table's own constraints naming the column (a composite one in full).
        // Their indexes are among the constraint-owned ones over the column, below.
        let table = st.table.clone();
        let (changed, _retired) =
            Catalog::retire_column_constraints(&table, column_name, st.constraints_mut(&catalog)?)?;
        st.constraints_changed |= changed;
        // CASCADE: the other tables' foreign keys that reference it.
        for (other, constraints) in inbound {
            tracing::info!(
                "DROP COLUMN {}.{} CASCADE: retiring the foreign keys of table '{}' that reference it",
                st.table,
                column_name,
                other
            );
            st.other_constraints.insert(other, constraints);
        }
        // Every index over the column goes with it. User indexes also need their
        // durable definition removed in the final batch, or a reopen (or a later
        // column with the same name) brings a tree over the old values back.
        let column_renames = &st.column_renames;
        let names_column = |column: &str| {
            let mut current = column.to_string();
            for (old, new) in column_renames {
                if current.eq_ignore_ascii_case(old) {
                    current.clone_from(new);
                }
            }
            current.eq_ignore_ascii_case(column_name)
        };
        let art = self.storage.art_indexes();
        for (name, _kind, columns) in art.list_table_indexes(&st.table) {
            if !columns.iter().any(|column| names_column(column)) {
                continue;
            }
            if !st.indexes_to_drop.contains(&name) {
                st.indexes_to_drop.push(name);
            }
        }
        // HNSW and DDL-only indexes have no ART entry. Multi-column user indexes
        // name the rest of their key in UniqueColumns, not column_name.
        for (name, definition) in catalog.list_index_definitions()? {
            let names_dropped_column = names_column(&definition.column_name)
                || definition.options.iter().any(|option| {
                    matches!(option, sql::logical_plan::IndexOption::UniqueColumns(columns)
                        if columns.iter().any(|column| names_column(column)))
                });
            if definition.table_name == st.table && names_dropped_column && !st.indexes_to_drop.contains(&name) {
                st.indexes_to_drop.push(name);
            }
        }
        // The identity entry.
        let identity = st.identity_mut(&catalog)?;
        let before = identity.len();
        identity.retain(|c| !c.eq_ignore_ascii_case(column_name));
        if identity.len() != before {
            st.identity_changed = true;
        }

        let width = st.schema.columns.len();
        st.schema.columns.remove(position);
        st.missing.remove(column_name);
        st.steps.push(RowStep::Remove(position));
        st.drops.push((width, position));
        st.shape_changed = true;
        Ok(())
    }

    /// `RENAME COLUMN`: the schema, the missing value, every constraint record naming
    /// the column (other tables' foreign keys included), the identity entry and the
    /// index definitions in the statement's group; the live index entries repointed
    /// after it. A CHECK body that cannot be rewritten refuses the statement first.
    fn alter_rename_column<'e>(&'e self, st: &mut AlterStatement<'e>, old: &str, new: &str) -> Result<()> {
        if st.schema.columns.iter().any(|c| c.name == *new) {
            return Err(Error::query_execution(format!(
                "Column '{}' already exists in table '{}'",
                new, st.table
            )));
        }
        let position = st.schema.columns.iter().position(|c| c.name == *old).ok_or_else(|| {
            Error::query_execution(format!("Column '{}' does not exist in table '{}'", old, st.table))
        })?;
        let catalog = self.storage.catalog();
        let table = st.table.clone();
        Catalog::validate_rename_column_in(&table, old, st.constraints_mut(&catalog)?)?;

        st.schema
            .get_column_at_mut(position)
            .ok_or_else(|| Error::internal("column index out of bounds"))?
            .name = new.to_string();
        if let Some(value) = st.missing.remove(old) {
            st.missing.insert(new.to_string(), value);
        }
        st.column_renames.push((old.to_string(), new.to_string()));
        st.shape_changed = true;

        if !old.eq_ignore_ascii_case(new) {
            let changed = Catalog::rename_column_in_constraint_set(&table, old, new, st.constraints_mut(&catalog)?)?;
            st.constraints_changed |= changed;
            for other in catalog.list_tables()? {
                if other == table {
                    continue;
                }
                let mut constraints = st.other_constraints(&catalog, &other)?;
                if Catalog::rename_referenced_column(&table, old, new, &mut constraints) {
                    st.other_constraints.insert(other, constraints);
                }
            }
            let identity = st.identity_mut(&catalog)?;
            let mut renamed = false;
            for column in identity.iter_mut() {
                if column.eq_ignore_ascii_case(old) {
                    *column = new.to_string();
                    renamed = true;
                }
            }
            st.identity_changed |= renamed;
        }

        // The live index entries, before the schema is published: the UNIQUE rule is
        // never bound to a name the published schema does not have.
        let art = std::sync::Arc::clone(self.storage.art_indexes());
        let (old, new) = (old.to_string(), new.to_string());
        st.publish_first.push(Box::new(move || {
            let repointed = art.rename_column_in_indexes(&table, &old, &new);
            if !repointed.is_empty() {
                tracing::debug!(
                    "Repointed {} live index(es) of '{}' from column '{}' to '{}': {:?}",
                    repointed.len(),
                    table,
                    old,
                    new,
                    repointed
                );
            }
        }));
        Ok(())
    }

    /// `ALTER COLUMN … DROP NOT NULL` (and the SET NOT NULL spelling the plan allows).
    fn alter_set_nullability(st: &mut AlterStatement<'_>, column_name: &str, nullable: bool) -> Result<()> {
        let table = st.table.clone();
        let column = st
            .schema
            .columns
            .iter_mut()
            .find(|c| c.name == column_name)
            .ok_or_else(|| {
                Error::query_execution(format!("Column '{}' does not exist in table '{}'", column_name, table))
            })?;
        if nullable && column.primary_key {
            return Err(Error::query_execution(format!(
                "Cannot drop NOT NULL from primary key column '{}'",
                column_name
            )));
        }
        column.nullable = nullable;
        Ok(())
    }

    // ---------------------------------------------------------------------
    // Constraints
    // ---------------------------------------------------------------------

    /// Is `columns` already enforced by a constraint-owned PK / UNIQUE index this
    /// statement keeps? (`Catalog::constraint_owned_unique_index_on`, without the
    /// indexes the statement drops.)
    fn alter_constraint_owned_unique_index_on(&self, st: &AlterStatement<'_>, columns: &[String]) -> Result<bool> {
        let catalog = self.storage.catalog();
        for (name, kind, index_columns) in self.storage.art_indexes().list_table_indexes(&st.table) {
            if !matches!(kind, storage::ArtIndexType::PrimaryKey | storage::ArtIndexType::Unique) {
                continue;
            }
            if index_columns.as_slice() != columns || st.indexes_to_drop.contains(&name) {
                continue;
            }
            if !catalog.index_definition_exists(&name)? {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// `ADD [CONSTRAINT name] UNIQUE (columns)` (and `ADD COLUMN … UNIQUE`): validated
    /// and indexed over the rows as the statement has them (so over a column an earlier
    /// clause added, with its value), the index registered HIDDEN (writers would
    /// maintain it, the read path skips it) until the statement's group is written. A
    /// duplicate is 23505 with nothing written and no index left.
    fn alter_add_unique<'e>(
        &'e self,
        st: &mut AlterStatement<'e>,
        constraint_name: &Option<String>,
        columns: &[String],
    ) -> Result<()> {
        if self.storage.is_branch_active() {
            // Same rule as CREATE INDEX: index state lives on main, and a
            // branch is supposed to be discardable.
            return Err(Error::query_execution(
                "ALTER TABLE … ADD CONSTRAINT … UNIQUE must run on the main branch",
            ));
        }
        // Resolve every named column to the catalog's own spelling: the ART index
        // resolves its columns by name, so a differently-cased name would build an
        // index that never matches a row.
        let mut resolved: Vec<String> = Vec::with_capacity(columns.len());
        for col in columns {
            let found = st
                .schema
                .columns
                .iter()
                .find(|c| c.name.eq_ignore_ascii_case(col))
                .ok_or_else(|| {
                    Error::query_execution(format!(
                        "column \"{}\" named in key does not exist in table \"{}\"",
                        col, st.table
                    ))
                })?;
            if resolved.iter().any(|c| c.eq_ignore_ascii_case(&found.name)) {
                return Err(Error::query_execution(format!(
                    "column \"{}\" appears twice in unique constraint",
                    found.name
                )));
            }
            resolved.push(found.name.clone());
        }
        let table = st.table.clone();
        let name = constraint_name
            .clone()
            .unwrap_or_else(|| format!("{}_{}_key", table, resolved.join("_")));
        let catalog = self.storage.catalog();
        Self::reject_duplicate_constraint_name(st.constraints_mut(&catalog)?, &table, &name)?;

        // A column set a constraint-owned PK / UNIQUE index already enforces gets no
        // second tree (`register_unique_constraint_indexes` applies the same test at
        // open); a user `CREATE UNIQUE INDEX` does not count.
        if !self.alter_constraint_owned_unique_index_on(st, &resolved)? {
            let art = self.storage.art_indexes();
            // THE RETURNED NAME IS LOAD-BEARING: the key is minted from (table,
            // columns), the constraint name rides along as a label.
            let index_name = art
                .create_constraint_unique_index_hidden(&st.table, &resolved, Some(&name))
                .map_err(|e| {
                    Error::query_execution(format!("could not create unique constraint \"{}\": {}", name, e))
                })?;
            {
                let dropped = std::sync::Arc::clone(art);
                let unwritten = index_name.clone();
                st.ddl.on_abort(move || {
                    let _ = dropped.drop_index(&unwritten);
                    dropped.reveal_index(&unwritten);
                });
                let shown = std::sync::Arc::clone(art);
                let hidden = index_name.clone();
                st.publish_after.push(Box::new(move || shown.reveal_index(&hidden)));
            }
            st.created_indexes.push(index_name.clone());
            let rows = self.alter_current_rows(st)?;
            if let Err(e) = art.backfill_unique_index(&index_name, &st.schema, &rows) {
                // The batch is dropped unwritten: its undo drops the index.
                return Err(Error::constraint_violation(format!(
                    "could not create unique constraint \"{}\" on relation \"{}\": {}",
                    name, st.table, e
                )));
            }
        }
        st.constraints_mut(&catalog)?
            .add_unique(sql::UniqueConstraint::new(name, table, resolved, false));
        st.constraints_changed = true;
        Ok(())
    }

    /// `ADD COLUMN … PRIMARY KEY`'s key (the only spelling that plans one): refused
    /// when the table has a key already or holds rows (every row would read the same
    /// value, or NULL, in it); otherwise its index is registered, and taken back if the
    /// statement's group is not written. The column's flag is the ADD COLUMN's.
    fn alter_add_primary_key<'e>(
        &'e self,
        st: &mut AlterStatement<'e>,
        constraint_name: &Option<String>,
        columns: &[String],
    ) -> Result<()> {
        if self.storage.is_branch_active() {
            return Err(Error::query_execution(
                "ALTER TABLE … ADD PRIMARY KEY must run on the main branch",
            ));
        }
        let mut resolved: Vec<String> = Vec::with_capacity(columns.len());
        for col in columns {
            let found = st
                .schema
                .columns
                .iter()
                .find(|c| c.name.eq_ignore_ascii_case(col))
                .ok_or_else(|| {
                    Error::query_execution(format!(
                        "column \"{}\" named in key does not exist in table \"{}\"",
                        col, st.table
                    ))
                })?;
            resolved.push(found.name.clone());
        }
        let existing_pk = st
            .schema
            .columns
            .iter()
            .any(|c| c.primary_key && !resolved.iter().any(|n| n.eq_ignore_ascii_case(&c.name)));
        let catalog = self.storage.catalog();
        let recorded_pk = st
            .constraints_mut(&catalog)?
            .unique_constraints
            .iter()
            .any(|uc| uc.is_primary_key);
        if existing_pk || recorded_pk {
            return Err(Error::query_execution(format!(
                "multiple primary keys for table \"{}\" are not allowed",
                st.table
            )));
        }
        let rows = self.storage.scan_table_with_schema(&st.table, &st.base)?.len();
        if rows > 0 {
            return Err(Error::query_execution(format!(
                "ALTER TABLE … ADD COLUMN … PRIMARY KEY is not supported on a non-empty relation \
                 (\"{}\" holds {} row(s)): the added column would be NULL — or the same DEFAULT — in every \
                 existing row. Add the column, backfill it, then add the key.",
                st.table, rows
            )));
        }
        let art = self.storage.art_indexes();
        let index_name = art.create_pk_index(&st.table, &resolved).map_err(|e| {
            Error::query_execution(format!(
                "could not create the index enforcing PRIMARY KEY on {}({}): {} — \
                 the constraint would not be enforced",
                st.table,
                resolved.join(", "),
                e
            ))
        })?;
        let dropped = std::sync::Arc::clone(art);
        let unwritten = index_name.clone();
        st.ddl.on_abort(move || {
            let _ = dropped.drop_index(&unwritten);
        });
        st.created_indexes.push(index_name);
        tracing::info!(
            "Adding PRIMARY KEY{} on {}({})",
            constraint_name
                .as_ref()
                .map(|n| format!(" '{}'", n))
                .unwrap_or_default(),
            st.table,
            resolved.join(", ")
        );
        Ok(())
    }

    /// `ADD [CONSTRAINT name] CHECK (expression)` (and `ADD COLUMN … CHECK`): the rows
    /// as the statement has them are validated first (`ADD c INT DEFAULT 1 CHECK (c >
    /// 5)` is 23514 with nothing written), then the record. Stored as `CREATE TABLE`
    /// stores one.
    fn alter_add_check(
        &self,
        st: &mut AlterStatement<'_>,
        constraint_name: &Option<String>,
        expression: &sql::LogicalExpr,
    ) -> Result<()> {
        let catalog = self.storage.catalog();
        let table = st.table.clone();
        let name = {
            let constraints = st.constraints_mut(&catalog)?;
            let name = match constraint_name {
                Some(name) => name.clone(),
                None => Self::derived_check_constraint_name(&table, expression, constraints),
            };
            Self::reject_duplicate_constraint_name(constraints, &table, &name)?;
            name
        };
        let body = serde_json::to_string(expression)
            .map_err(|e| Error::query_execution(format!("could not store CHECK constraint \"{}\": {}", name, e)))?;
        let rows = self.alter_current_rows(st)?;
        if !rows.is_empty() {
            let probe = sql::TableConstraints {
                foreign_keys: Vec::new(),
                unique_constraints: Vec::new(),
                check_constraints: vec![sql::CheckConstraint::new(name.clone(), table.clone(), body.clone())],
            };
            for row in &rows {
                self.validate_check_constraints(&probe, &st.schema, &row.values)
                    .map_err(|e| {
                        Error::constraint_violation(format!(
                            "check constraint \"{}\" of relation \"{}\" is violated by some row: {}",
                            name, table, e
                        ))
                    })?;
            }
        }
        st.constraints_mut(&catalog)?
            .add_check(sql::CheckConstraint::new(name, table, body));
        st.constraints_changed = true;
        Ok(())
    }

    /// `DROP CONSTRAINT [IF EXISTS] name`: matched across FOREIGN KEY, UNIQUE / PRIMARY
    /// KEY and CHECK by name (case-insensitive). A retired UNIQUE's index and column
    /// flag are settled against what the statement leaves ([`Self::finish_alter`]).
    fn alter_drop_constraint(&self, st: &mut AlterStatement<'_>, constraint_name: &str, if_exists: bool) -> Result<()> {
        let catalog = self.storage.catalog();
        let constraints = st.constraints_mut(&catalog)?;
        let before = constraint_count(constraints);
        constraints
            .foreign_keys
            .retain(|c| !c.name.eq_ignore_ascii_case(constraint_name));
        constraints
            .check_constraints
            .retain(|c| !c.name.eq_ignore_ascii_case(constraint_name));
        let retired: Vec<sql::UniqueConstraint> = constraints
            .unique_constraints
            .iter()
            .filter(|c| c.name.eq_ignore_ascii_case(constraint_name))
            .cloned()
            .collect();
        constraints
            .unique_constraints
            .retain(|c| !c.name.eq_ignore_ascii_case(constraint_name));
        if constraint_count(constraints) == before {
            if if_exists {
                return Ok(());
            }
            return Err(Error::query_execution(format!(
                "constraint \"{}\" of relation \"{}\" does not exist",
                constraint_name, st.table
            )));
        }
        st.constraints_changed = true;
        st.retired_uniques.extend(retired);
        Ok(())
    }

    /// `ADD [CONSTRAINT name] FOREIGN KEY … REFERENCES …` (and `ADD COLUMN …
    /// REFERENCES`): the parent and its columns validated (42P01 / 42703 / 42830), the
    /// referencing columns against the table as the statement has it, the name
    /// against its constraints; the lookup index registered after the write.
    fn alter_add_foreign_key<'e>(&'e self, st: &mut AlterStatement<'e>, plan: &sql::LogicalPlan) -> Result<()> {
        let sql::LogicalPlan::AlterTableAddForeignKey {
            constraint_name,
            columns,
            references_table,
            references_columns,
            on_delete,
            on_update,
            deferrable,
            initially_deferred,
            enforcement,
            ..
        } = plan
        else {
            return Err(Error::internal("alter_add_foreign_key called with another plan"));
        };
        let catalog = self.storage.catalog();
        let table = st.table.clone();
        // Loaded first: a self-reference is judged against it.
        st.constraints_mut(&catalog)?;
        if references_table == &table {
            // A self-reference: the parent is the table as the statement has it.
            self.validate_self_reference(st, columns, references_columns)?;
        } else {
            Self::validate_fk_reference(&catalog, references_table, columns, references_columns)?;
        }
        for col in columns {
            if !st.schema.columns.iter().any(|c| c.name.eq_ignore_ascii_case(col)) {
                return Err(Self::fk_column_does_not_exist(col));
            }
        }
        let fk_name = {
            let constraints = st.constraints_mut(&catalog)?;
            if let Some(name) = constraint_name {
                let collides = constraints
                    .foreign_keys
                    .iter()
                    .map(|c| c.name.as_str())
                    .chain(constraints.check_constraints.iter().map(|c| c.name.as_str()))
                    .chain(constraints.unique_constraints.iter().map(|c| c.name.as_str()))
                    .any(|existing_name| existing_name.eq_ignore_ascii_case(name));
                if collides {
                    return Err(Error::query_execution(format!(
                        "constraint \"{}\" for relation \"{}\" already exists",
                        name, table
                    )));
                }
            }
            match constraint_name {
                Some(name) => name.clone(),
                None => {
                    // Deduplicated against the table's foreign keys: the schema no
                    // longer tells two like-named parents apart.
                    let existing: Vec<String> = constraints.foreign_keys.iter().map(|f| f.name.clone()).collect();
                    sql::ForeignKeyConstraint::generate_unique_name(&table, columns, references_table, &existing)
                }
            }
        };
        // `REFERENCES parent` without a column list binds to the parent's PRIMARY KEY.
        let references_columns = if references_columns.is_empty() && references_table == &table {
            st.schema
                .columns
                .iter()
                .filter(|c| c.primary_key)
                .map(|c| c.name.clone())
                .collect()
        } else {
            Self::resolve_fk_referenced_columns(&catalog, references_table, references_columns)?
        };
        let mut fk = sql::ForeignKeyConstraint::new(
            fk_name,
            table.clone(),
            columns.clone(),
            references_table.clone(),
            references_columns,
        );
        if let Some(action) = on_delete {
            fk = fk.on_delete(crate::convert_logical_referential_action(action));
        }
        if let Some(action) = on_update {
            fk = fk.on_update(crate::convert_logical_referential_action(action));
        }
        if *deferrable {
            fk = fk.deferrable(*initially_deferred);
        }
        fk = fk.with_enforcement(*enforcement);
        st.constraints_mut(&catalog)?.add_foreign_key(fk.clone());
        st.constraints_changed = true;
        let storage = &self.storage;
        st.publish_after
            .push(Box::new(move || Catalog::new(storage).register_foreign_key_index(&fk)));
        Ok(())
    }

    /// A foreign key of the table to itself, against the table as the statement has
    /// it (a referenced column an earlier clause added counts): the referenced columns
    /// exist, their count is the referencing one's, and they are a key (the PRIMARY
    /// KEY, a column's UNIQUE flag, a UNIQUE record, or a live unique index), as
    /// `validate_fk_reference` asks of another table.
    fn validate_self_reference(
        &self,
        st: &AlterStatement<'_>,
        referencing: &[String],
        referenced: &[String],
    ) -> Result<()> {
        for col in referenced {
            if !st.schema.columns.iter().any(|c| c.name.eq_ignore_ascii_case(col)) {
                return Err(Self::fk_column_does_not_exist(col));
            }
        }
        let primary_key: Vec<String> = st
            .schema
            .columns
            .iter()
            .filter(|c| c.primary_key)
            .map(|c| c.name.clone())
            .collect();
        let referenced_count = if referenced.is_empty() {
            if primary_key.is_empty() {
                return Err(Self::fk_no_primary_key_error(&st.table));
            }
            primary_key.len()
        } else {
            referenced.len()
        };
        if referencing.len() != referenced_count {
            return Err(Self::fk_arity_error());
        }
        if referenced.is_empty() {
            return Ok(());
        }
        let flagged = referenced.len() == 1
            && st
                .schema
                .columns
                .iter()
                .any(|c| c.unique && same_column_set(std::slice::from_ref(&c.name), referenced));
        let recorded = st.constraints.as_ref().is_some_and(|constraints| {
            constraints
                .unique_constraints
                .iter()
                .any(|uc| same_column_set(&uc.columns, referenced))
        });
        let indexed = || {
            self.storage
                .art_indexes()
                .unique_column_sets(&st.table)
                .iter()
                .any(|set| same_column_set(set, referenced))
        };
        if same_column_set(&primary_key, referenced) || flagged || recorded || indexed() {
            return Ok(());
        }
        Err(Self::fk_not_unique_error(&st.table))
    }

    // ---------------------------------------------------------------------
    // The one write
    // ---------------------------------------------------------------------

    /// Is `columns` still claimed, after the statement, by its PRIMARY KEY or a UNIQUE
    /// record (`Catalog::column_set_claimed_by_record_or_pk`, over what it leaves)?
    fn claimed_by_record_or_pk(st: &AlterStatement<'_>, columns: &[String]) -> bool {
        let primary_key: Vec<String> = st
            .schema
            .columns
            .iter()
            .filter(|c| c.primary_key)
            .map(|c| c.name.clone())
            .collect();
        (!primary_key.is_empty() && same_column_set(&primary_key, columns))
            || st.constraints.as_ref().is_some_and(|constraints| {
                constraints
                    .unique_constraints
                    .iter()
                    .any(|uc| same_column_set(&uc.columns, columns))
            })
    }

    /// The UNIQUE constraints `DROP CONSTRAINT` retired, against what the statement
    /// leaves (`Catalog::drop_unique_constraint_indexes`' rules): a single-column one
    /// that co-declared the column's `unique` flag clears it, unless another rule still
    /// claims the column; and its constraint-owned indexes go after the write, unless a
    /// rule still claims the column set (two rules can share one tree).
    fn settle_retired_uniques(&self, st: &mut AlterStatement<'_>) -> Result<()> {
        let retired = std::mem::take(&mut st.retired_uniques);
        if retired.is_empty() {
            return Ok(());
        }
        let catalog = self.storage.catalog();
        let art = self.storage.art_indexes();
        for uc in retired {
            if !uc.is_primary_key && uc.columns.len() == 1 && !Self::claimed_by_record_or_pk(st, &uc.columns) {
                for column in st.schema.columns.iter_mut() {
                    if column.unique
                        && !column.primary_key
                        && uc.columns.iter().any(|c| column.name.eq_ignore_ascii_case(c))
                    {
                        column.unique = false;
                    }
                }
            }
            let still_claimed = Self::claimed_by_record_or_pk(st, &uc.columns)
                || (uc.columns.len() == 1
                    && st.schema.columns.iter().any(|c| {
                        c.unique && !c.primary_key && uc.columns.iter().any(|w| c.name.eq_ignore_ascii_case(w))
                    }));
            if still_claimed {
                continue;
            }
            for candidate in art.unique_indexes_on_columns(&st.table, &uc.columns) {
                // A durable definition means a user `CREATE UNIQUE INDEX`, which
                // PostgreSQL keeps; a read failure keeps it too (over-enforcing beats
                // silently unenforcing).
                if catalog.index_definition_exists(&candidate).unwrap_or(true)
                    || st.created_indexes.contains(&candidate)
                    || st.indexes_to_drop.contains(&candidate)
                {
                    continue;
                }
                st.indexes_to_drop.push(candidate);
            }
        }
        Ok(())
    }

    /// Stage the whole statement into its one group and write it (see the module
    /// docs). Returns the rows rewritten.
    fn finish_alter<'e>(&'e self, mut st: AlterStatement<'e>) -> Result<u64> {
        self.settle_retired_uniques(&mut st)?;
        // Track column identity through structural edits. A temporary rename,
        // swap, or DROP followed by reuse of its name must read the original
        // sidecar, not another clause's pending destination.
        let mut origins: Vec<_> = st.base.columns.iter().map(|column| Some(column.name.clone())).collect();
        for step in &st.steps {
            match step {
                RowStep::Append(_) | RowStep::AppendPerRow => origins.push(None),
                RowStep::Remove(position) if *position < origins.len() => {
                    origins.remove(*position);
                }
                RowStep::Remove(_) => return Err(Error::internal("ALTER TABLE column identity is out of bounds")),
            }
        }
        if origins.len() != st.schema.columns.len() {
            return Err(Error::internal(
                "ALTER TABLE column identities differ from the final schema",
            ));
        }
        let net_renames: Vec<_> = origins
            .iter()
            .zip(&st.schema.columns)
            .filter_map(|(origin, column)| {
                origin
                    .as_ref()
                    .filter(|old| old.as_str() != column.name)
                    .map(|old| (old.clone(), column.name.clone()))
            })
            .collect();
        let columnar_removed: Vec<_> = st
            .base
            .columns
            .iter()
            .filter(|column| {
                column.storage_mode == ColumnStorageMode::Columnar
                    && !origins.iter().any(|origin| origin.as_ref() == Some(&column.name))
            })
            .map(|column| column.name.clone())
            .collect();
        // A live child retains its old schema. Freeze its rows as logical values
        // in this SAME group before moving sidecars away from those old names.
        // Without children, a representation-preserving rename stays metadata-only.
        let freeze_stored_rename = st.copies > 0
            && st.base.columns.iter().any(|column| {
                column.storage_mode != ColumnStorageMode::Default
                    && net_renames.iter().any(|(old, _)| old == &column.name)
            });
        let rewrites = st.rewrites_rows() || freeze_stored_rename;
        let AlterStatement {
            table,
            txn,
            mut ddl,
            base,
            base_shape,
            base_missing,
            schema,
            missing,
            constraints,
            constraints_changed,
            other_constraints,
            identity,
            identity_changed,
            column_renames,
            steps,
            per_row,
            own_per_row,
            drops,
            shape_changed,
            indexes_to_drop,
            created_indexes,
            publish_first,
            publish_after,
            own_rows,
            ..
        } = st;
        let change = AlteredTable {
            table: table.clone(),
            base,
            schema,
            base_missing,
            missing,
            column_renames,
            drops,
            shape_changed,
            at: self.storage.next_timestamp(),
            cut: None,
        };
        if !change.changed()
            && !constraints_changed
            && other_constraints.is_empty()
            && !identity_changed
            && !rewrites
            && indexes_to_drop.is_empty()
            && created_indexes.is_empty()
        {
            // Every clause was a no-op (`IF [NOT] EXISTS`): nothing to write.
            return Ok(0);
        }

        // Global topology planning precedes any dictionary/zone-stats guard.
        // The actual purge stays after rename puts below; only ownership is
        // prepared here, and ordinary ALTER takes no persistent planning lease.
        if !indexes_to_drop.is_empty()
            && self
                .storage
                .catalog()
                .index_definitions_strict()?
                .iter()
                .any(|(name, definition)| {
                    indexes_to_drop.contains(name) && definition.index_type.as_deref() == Some("persistent_hnsw")
                })
        {
            ddl.prepare_persistent_retirements();
        }

        // The rows, when a clause rewrites them: each new image with its version at
        // the statement's timestamp, measured against the cap as it is staged.
        let what = if steps.iter().any(|step| matches!(step, RowStep::Remove(_))) {
            "DROP COLUMN"
        } else if steps.iter().any(|step| matches!(step, RowStep::AppendPerRow)) {
            "ADD COLUMN with a volatile default"
        } else {
            "RENAME COLUMN with frozen side storage"
        };
        let max_bytes = self.storage.config().storage.max_atomic_rewrite_bytes;
        let refuse = |bytes: u64| {
            if what != "ADD COLUMN with a volatile default" {
                Error::atomic_alter_too_large(&table, what, bytes, max_bytes)
            } else {
                Error::atomic_rewrite_too_large(&table, bytes, max_bytes)
            }
        };
        let limit = RewriteLimit {
            max_bytes,
            refuse: &refuse,
        };
        let needs_row_witness = crate::sql::temp_tables::is_temp_key(&table) && change.schema == change.base;
        let mut rewritten_witness = None;
        let staged = if rewrites {
            let mut rewrite = |row_id: u64, stored: &mut Tuple, logical: &mut Tuple| {
                // The volatile values in per_row were evaluated exactly once.
                // Apply the same structural edits to the stored row and history.
                apply_steps(&steps, stored, per_row.get(&row_id))?;
                apply_steps(&steps, logical, per_row.get(&row_id))?;
                if needs_row_witness && rewritten_witness.is_none() {
                    let key = format!("data:{table}:{row_id}").into_bytes();
                    let after = bincode::serialize(stored)
                        .map_err(|error| Error::storage(format!("Cannot encode ALTER row witness: {error}")))?;
                    if self.storage.get(&key)?.as_ref() != Some(&after) {
                        rewritten_witness = Some((key, after));
                    }
                }
                Ok(())
            };
            Some(self.storage.stage_rewritten_rows(
                &table,
                &base_shape,
                change.at,
                &mut rewrite,
                &limit,
                ddl.batch_mut(),
            )?)
        } else {
            None
        };
        let rewritten = staged.as_ref().map_or(0, |staged| staged.rows);
        let frozen_copy_bytes = staged.as_ref().map_or(0, |staged| staged.frozen_copy_bytes);

        let catalog = self.storage.catalog();
        // Cache settlement is queued before any schema/index publication. Its
        // global dictionary guard releases in the earlier resource phase.
        catalog.stage_altered_column_side_storage(
            &table,
            &net_renames,
            &columnar_removed,
            change
                .schema
                .columns
                .iter()
                .any(|column| column.storage_mode == ColumnStorageMode::Columnar),
            &mut ddl,
        )?;

        // Published first, in this order: no reader pairs the new schema with a row
        // the row cache holds from before (it is swept again after the bump, which a
        // fill that read the old bytes cannot pass); the live index entries a rename
        // repoints.
        if rewrites {
            let storage = &self.storage;
            let swept = table.clone();
            ddl.on_commit(move || storage.row_cache().invalidate_table(&swept));
        }
        for publish in publish_first {
            ddl.on_commit(publish);
        }
        if !net_renames.is_empty() {
            let storage = &self.storage;
            let table = table.clone();
            let renamed = net_renames.clone();
            ddl.on_commit(move || storage.vector_indexes().relabel_table_columns(&table, &renamed));
        }
        catalog.stage_altered_table(&change, &mut ddl)?;
        if constraints_changed {
            if let Some(constraints) = &constraints {
                catalog.stage_table_constraints(&table, constraints, &mut ddl)?;
            }
        }
        for (other, constraints) in &other_constraints {
            catalog.stage_table_constraints(other, constraints, &mut ddl)?;
        }
        if identity_changed {
            if let Some(identity) = &identity {
                catalog.stage_identity_columns(&table, identity, &mut ddl)?;
            }
        }
        // Published after the schema and the constraints: the rewrite's snapshot, the
        // row cache once more, the indexes the statement retired and the ones it
        // registered hidden.
        {
            let storage = &self.storage;
            let published = table.clone();
            let snapshot = staged.and_then(|staged| staged.snapshot);
            ddl.on_commit(move || {
                if let Some(snapshot) = snapshot {
                    storage
                        .snapshot_manager()
                        .publish_rewrite_snapshot(&published, snapshot);
                }
                if rewrites {
                    storage.row_cache().invalidate_table(&published);
                }
            });
        }
        if !indexes_to_drop.is_empty() {
            // Stage after stage_altered_table's rename puts: a multi-clause
            // statement which renames and then drops a column must leave no
            // definition for it, regardless of the order of those staged puts.
            for name in indexes_to_drop {
                if let Some(definition) = catalog.get_index_definition(&name)? {
                    Catalog::stage_persistent_index_retirement(&name, &definition, &mut ddl)?;
                    Catalog::stage_index_definition_drop(&name, &mut ddl);
                    ddl.log(storage::WalOperation::DropIndex { name: name.clone() });
                    let storage = &self.storage;
                    ddl.on_commit(move || {
                        if let Err(e) = storage::teardown_index_structures(storage, &name, definition.index_type.as_deref()) {
                            tracing::warn!(
                                "ALTER TABLE: definition of index '{}' removed, but its derived structure could not be torn down: {}",
                                name,
                                e
                            );
                        }
                    });
                } else {
                    let art = std::sync::Arc::clone(self.storage.art_indexes());
                    let owner = table.clone();
                    ddl.on_commit(move || {
                        if let Err(e) = art.drop_index(&name) {
                            tracing::warn!(
                                "ALTER TABLE {}: could not drop the constraint index '{}': {} — the retired rule may \
                             keep rejecting rows until the next open",
                                owner,
                                name,
                                e
                            );
                        }
                    });
                }
            }
        }
        for publish in publish_after {
            ddl.on_commit(publish);
        }
        let measured = (ddl.size_in_bytes() as u64).saturating_add(frozen_copy_bytes);
        if rewrites && measured > max_bytes {
            return Err(refuse(measured));
        }
        catalog.stage_alter_application_witness(
            &change,
            constraints.as_ref().filter(|_| constraints_changed),
            identity.as_ref().filter(|_| identity_changed),
            rewritten_witness,
            other_constraints.keys().cloned().collect(),
            &mut ddl,
        )?;
        let applied = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let published = std::sync::Arc::clone(&applied);
        ddl.on_commit(move || published.store(true, std::sync::atomic::Ordering::Release));
        ddl.exit_after_write("alter_table", &table);
        let _publication = storage::layout_epochs::PublicationScope::enter();
        let _census = self.storage.write_census().hold_for_replay(vec![table.clone()], false);
        let written = ddl.commit();

        // Spec R5.2 (F5): the transaction's own staged rows get what the statement did
        // to the table, so what they commit (and log) is as wide as the table they
        // commit into. In memory; the statement is durable already.
        if let Some(txn) =
            txn.filter(|_| applied.load(std::sync::atomic::Ordering::Acquire) && own_rows && !steps.is_empty())
        {
            txn.restage_table_rows(&table, |key, tuple| {
                base_shape.apply(tuple);
                apply_steps(&steps, tuple, own_per_row.get(key))
            })?;
        }

        let written = written?;
        tracing::info!("ALTER TABLE {}: one write group, {} row(s) rewritten", table, rewritten);
        written.wait_replicated();
        Ok(rewritten)
    }

    // ---------------------------------------------------------------------
    // SET STORAGE
    // ---------------------------------------------------------------------

    /// `ALTER TABLE t ALTER COLUMN c SET STORAGE mode` as ONE write group: every row
    /// with the column in its new representation (and a version of it), the schema,
    /// the dictionary record, the content-addressed payloads, the columnar batches and
    /// the presence purge, with the logical-WAL entries, up to `[storage]
    /// max_atomic_rewrite_bytes` (54000 above it, nothing written). It used to write a
    /// group per row, then the schema: a reader or a crash in between saw rows of one
    /// representation under a schema of the other. A version from before the move out
    /// of a stored representation holds references the column no longer resolves, so
    /// a read older than it is refused (`storage::table_layout`).
    pub(crate) fn alter_column_storage(
        &self,
        table_name: &str,
        column_name: &str,
        storage_mode: ColumnStorageMode,
        txn: Option<&Transaction>,
    ) -> Result<u64> {
        let catalog = self.storage.catalog();
        let mut schema = catalog.get_table_schema(table_name)?;
        let position = schema
            .columns
            .iter()
            .position(|c| c.name == *column_name)
            .ok_or_else(|| {
                Error::query_execution(format!("Column '{}' not found in table '{}'", column_name, table_name))
            })?;
        let column = schema
            .get_column_at(position)
            .cloned()
            .ok_or_else(|| Error::internal("column index out of bounds"))?;
        let old_mode = column.storage_mode;
        if old_mode == storage_mode {
            return Ok(0);
        }
        Self::refuse_ddl_over_staged_rows(txn, "ALTER TABLE … SET STORAGE", table_name)?;

        let guard = self
            .storage
            .main_catalog_write(&[table_name], "ALTER TABLE … SET STORAGE")?;
        if !catalog.main_table_exists(table_name)? {
            return Err(Error::query_execution(format!("Table '{}' does not exist", table_name)));
        }
        let mut ddl = DdlBatch::new(&self.storage);
        ddl.hold_until_published(guard);
        let at = self.storage.next_timestamp();
        let max_bytes = self.storage.config().storage.max_atomic_rewrite_bytes;
        let refuse =
            |bytes: u64| Error::atomic_alter_too_large(table_name, "ALTER COLUMN … SET STORAGE", bytes, max_bytes);
        let limit = RewriteLimit {
            max_bytes,
            refuse: &refuse,
        };
        let migration = ColumnMigration {
            position,
            column: column.clone(),
            old_mode,
            new_mode: storage_mode,
        };
        let staged = self
            .storage
            .stage_column_storage_migration(table_name, &migration, at, &limit, &mut ddl)?;

        let base = schema.clone();
        schema
            .get_column_at_mut(position)
            .ok_or_else(|| Error::internal("column index out of bounds"))?
            .storage_mode = storage_mode;
        let base_missing = self.storage.stored_missing_values(table_name)?;
        let change = AlteredTable {
            table: table_name.to_string(),
            base,
            schema,
            missing: base_missing.clone(),
            base_missing,
            column_renames: Vec::new(),
            drops: Vec::new(),
            shape_changed: true,
            at,
            cut: (old_mode != ColumnStorageMode::Default).then_some(storage::table_layout::HistoryCut::StorageMoved),
        };
        {
            let storage = &self.storage;
            let swept = table_name.to_string();
            ddl.on_commit(move || storage.row_cache().invalidate_table(&swept));
        }
        catalog.stage_altered_table(&change, &mut ddl)?;
        // The entry the replay of the log and a logical consumer read the move from.
        let mode = bincode::serialize(&storage_mode)
            .map_err(|e| Error::storage(format!("Failed to serialize storage mode: {}", e)))?;
        ddl.log(storage::WalOperation::AlterColumnStorage {
            table: table_name.to_string(),
            column: column_name.to_string(),
            storage_mode: mode,
        });
        let measured = (ddl.size_in_bytes() as u64).saturating_add(staged.frozen_copy_bytes);
        if measured > max_bytes {
            return Err(refuse(measured));
        }
        catalog.stage_alter_application_witness(&change, None, None, None, Vec::new(), &mut ddl)?;
        {
            let storage = &self.storage;
            let published = table_name.to_string();
            let snapshot = staged.snapshot;
            ddl.on_commit(move || {
                if let Some(snapshot) = snapshot {
                    storage
                        .snapshot_manager()
                        .publish_rewrite_snapshot(&published, snapshot);
                }
                storage.row_cache().invalidate_table(&published);
            });
        }
        ddl.exit_after_write("alter_table", table_name);
        let written = ddl.commit()?;
        tracing::info!(
            "Altered {}.{} storage from {:?} to {:?}, migrated {} rows in one write group",
            table_name,
            column_name,
            old_mode,
            storage_mode,
            staged.rows
        );
        written.wait_replicated();
        Ok(staged.rows)
    }
}
