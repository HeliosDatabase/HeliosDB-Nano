//! Implementation of dump/restore traits for EmbeddedDatabase

use crate::storage::dump::{DatabaseInterface, DatabaseRestoreInterface, IndexMetadata, RestoreValidation};
use crate::{EmbeddedDatabase, Error, Result, Schema, Tuple, Value};

/// How many offending keys a single constraint contributes to the restore
/// validation error. Enough to diagnose, bounded so a wholly-invalid table
/// cannot produce a megabyte-long message.
const MAX_REPORTED_VIOLATIONS: usize = 10;

thread_local! {
    /// Non-fatal notes from a restore's constraint phases, drained into
    /// `RestoreReport::validation_warnings` by `take_restore_warnings`.
    ///
    /// A thread-local rather than a field on `EmbeddedDatabase`: a restore runs
    /// start to finish on the calling thread (`DumpManager::restore_from_dump`
    /// drives phases A, B and C inline), so notes can never cross between
    /// concurrent restores on different threads, and the database handle keeps
    /// its shape.
    static RESTORE_WARNINGS: std::cell::RefCell<Vec<String>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// Record a non-fatal restore note.
fn note_restore_warning(message: String) {
    // Logged once, by `DumpManager::restore_from_dump_stats`, when it drains
    // these into the report.
    RESTORE_WARNINGS.with(|w| w.borrow_mut().push(message));
}

/// `column = value, …` for the FK columns of one offending row.
fn fk_key_repr(columns: &[String], values: &[Value]) -> String {
    columns
        .iter()
        .zip(values.iter())
        .map(|(c, v)| format!("{c}={v}"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// A short identifier for one offending row: its PRIMARY KEY columns, or the
/// first column when the table has no primary key.
fn row_key_repr(schema: &Schema, row: &Tuple) -> String {
    let mut key_columns: Vec<usize> = schema
        .columns
        .iter()
        .enumerate()
        .filter(|(_, c)| c.primary_key)
        .map(|(i, _)| i)
        .collect();
    if key_columns.is_empty() {
        key_columns.push(0);
    }
    key_columns
        .into_iter()
        .filter_map(|i| {
            schema
                .columns
                .get(i)
                .zip(row.values.get(i))
                .map(|(c, v)| format!("{}={}", c.name, v))
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// `1 row` / `N rows`.
fn rows_phrase(n: u64) -> String {
    if n == 1 {
        "1 row".to_string()
    } else {
        format!("{n} rows")
    }
}

/// The verb that agrees with [`rows_phrase`]: "1 row REFERENCES", "2 rows
/// REFERENCE".
fn references_verb(n: u64) -> &'static str {
    if n == 1 {
        "references"
    } else {
        "reference"
    }
}

/// Why a violation found under `--no-validate` is only a warning.
const LENIENT_REASON: &str =
    "the restore was asked not to validate constraints (--no-validate), so the rows were kept as they are";

/// Why an FK violation does NOT fail the restore, or `None` when it does.
///
/// These are exactly the non-fatal cases `check_fk_constraints_on_write`
/// honours on the write path (`lib.rs`, the `Audit` / `LockFree` arm that calls
/// `record_fk_violation` and continues), plus the operator's explicit
/// `--no-validate`. Without them a database that legitimately holds violating
/// rows — `audit` mode is a documented, shipped mode
/// (docs/guides/fk_validation_modes.md: "accept writes, log violations") —
/// could be dumped but never restored, which is the one outcome a backup
/// feature must not have.
fn fk_violation_is_non_fatal(
    fk: &crate::sql::ForeignKeyConstraint,
    mode: crate::FkValidationMode,
    validation: RestoreValidation,
) -> Option<&'static str> {
    if fk.enforcement == crate::sql::ConstraintEnforcement::LockFree {
        Some(
            "the constraint is LOCK-FREE, so the write path accepts such rows and records them in \
             pg_log_violations rather than rejecting them",
        )
    } else if mode == crate::FkValidationMode::Audit {
        Some(
            "helios.fk_validation = 'audit' on this session, so the write path accepts such rows and \
             records them in pg_log_violations rather than rejecting them",
        )
    } else if validation.is_lenient() {
        Some(LENIENT_REASON)
    } else {
        None
    }
}

/// The warning for a CHECK this build could not evaluate. Not fatal: the
/// constraint IS registered, and the same failure rejects new writes.
fn unevaluable_check_warning(table: &str, constraint: &str, reason: &str) -> String {
    format!(
        "{table}.{constraint}: the CHECK expression could not be evaluated during restore validation \
         ({reason}); the constraint was registered and still applies to new writes"
    )
}

/// How one FOREIGN KEY's parent side is probed during phase C — built ONCE per
/// constraint.
enum ParentProbe {
    /// The parent has a single-column ART index covering the referenced
    /// column: reuse the write path's own O(log N) lookup
    /// (`check_referencing_rows_exist`), deduplicated per distinct child key.
    WritePath,
    /// No usable index — every composite FK, and any single-column FK whose
    /// parent column is unindexed. The parent's referenced-column tuples are
    /// encoded ONCE into a set.
    ///
    /// This is the difference between a restore that finishes and one that does
    /// not: `check_referencing_rows_exist` takes its ART fast path only for a
    /// SINGLE-column key, so a composite FK fell back to a fresh full parent
    /// scan PER DISTINCT CHILD KEY (500 k distinct keys x a 500 k-row parent =
    /// 2.5e11 comparisons and 500 k full-table materialisations).
    ///
    /// Carries the parent SCHEMA (probe keys must be encoded through the same
    /// coercion the stored keys were) and one entry per parent row.
    Keys(Schema, std::collections::HashSet<Vec<u8>>),
}

impl EmbeddedDatabase {
    /// Build the phase-C probe for one FK. `Ok(None)` means the parent does not
    /// have the referenced columns at all, so its rows cannot be validated.
    fn build_parent_probe(&self, fk: &crate::sql::ForeignKeyConstraint) -> Result<Option<ParentProbe>> {
        // A degenerate record (no referenced columns, or a different arity from
        // the child columns) keeps the write path's own handling rather than
        // inventing a second interpretation of it.
        if fk.references_columns.is_empty() || fk.references_columns.len() != fk.columns.len() {
            return Ok(Some(ParentProbe::WritePath));
        }
        if let (1, Some(column)) = (fk.references_columns.len(), fk.references_columns.first()) {
            if self
                .storage
                .art_indexes()
                .find_column_index(&fk.references_table, column)
                .is_some()
            {
                // Cheaper than materialising the parent: the write path's own
                // index lookup, once per distinct child key.
                return Ok(Some(ParentProbe::WritePath));
            }
        }

        let parent_schema = self.storage.catalog().get_table_schema(&fk.references_table)?;
        let Some(positions) = fk
            .references_columns
            .iter()
            .map(|c| {
                parent_schema
                    .columns
                    .iter()
                    .position(|pc| pc.name.eq_ignore_ascii_case(c))
            })
            .collect::<Option<Vec<usize>>>()
        else {
            return Ok(None);
        };

        let parent_rows = self
            .storage
            .scan_table_with_schema(&fk.references_table, &parent_schema)?;
        let mut keys: std::collections::HashSet<Vec<u8>> = std::collections::HashSet::with_capacity(parent_rows.len());
        for row in &parent_rows {
            let mut values: Vec<Value> = Vec::with_capacity(positions.len());
            for &i in &positions {
                // A NULL parent key can never satisfy a probe — a child row
                // with a NULL in any FK column is skipped before it probes at
                // all (MATCH SIMPLE) — and leaving it out keeps a NULL from
                // ever encoding into the same bytes as some real value.
                match row.values.get(i) {
                    Some(Value::Null) | None => break,
                    Some(v) => values.push(v.clone()),
                }
            }
            if values.len() != positions.len() {
                continue;
            }
            let key = Self::encode_fk_key(&parent_schema, &fk.references_columns, &values);
            keys.insert(key);
        }
        Ok(Some(ParentProbe::Keys(parent_schema, keys)))
    }

    /// Encode one FK key the way BOTH sides of the probe must see it.
    ///
    /// The values are first coerced to the PARENT's declared column types with
    /// the same `coerce_fk_probe_values` the write path applies before its ART
    /// lookup — `encode_key` is type-width sensitive (Int4 is 4 bytes, Int8 is
    /// 8, NUMERIC is another encoding entirely), so a cross-type FK (int8 child
    /// to int4 parent) would otherwise report a phantom violation. Running the
    /// parent's own key values through the same function keeps the two sides
    /// consistent by construction instead of by inspection.
    fn encode_fk_key(parent_schema: &Schema, parent_columns: &[String], values: &[Value]) -> Vec<u8> {
        let normalised = Self::coerce_fk_probe_values(parent_schema, parent_columns, values);
        crate::storage::ArtIndexManager::encode_key(&normalised)
    }
}

impl DatabaseInterface for EmbeddedDatabase {
    fn list_tables(&self) -> Result<Vec<String>> {
        let catalog = self.storage.catalog();
        catalog.list_tables()
    }

    fn get_table_schema(&self, table: &str) -> Result<Schema> {
        let catalog = self.storage.catalog();
        catalog.get_table_schema(table)
    }

    fn scan_table(&self, table: &str) -> Result<Vec<Tuple>> {
        self.storage.scan_table(table)
    }

    fn get_table_indexes(&self, table: &str) -> Result<Vec<IndexMetadata>> {
        // Get vector indexes from the vector index manager
        let vector_indexes = self.storage.vector_indexes();
        let all_metadata = vector_indexes.list_all_metadata();

        // Filter indexes for this specific table and convert to IndexMetadata
        let indexes: Vec<IndexMetadata> = all_metadata
            .into_iter()
            .filter(|meta| meta.table_name == table)
            .map(|meta| {
                let index_type = match &meta.index_type {
                    crate::storage::VectorIndexType::Standard(_) => "hnsw",
                    crate::storage::VectorIndexType::Quantized(_) => "hnsw_pq",
                    crate::storage::VectorIndexType::Persistent(cfg) => {
                        if cfg.pq_enabled {
                            "persistent_hnsw_pq"
                        } else {
                            "persistent_hnsw"
                        }
                    }
                };
                IndexMetadata {
                    name: meta.name,
                    index_type: index_type.to_string(),
                    columns: vec![meta.column_name],
                    is_unique: false, // Vector indexes are not unique constraint indexes
                }
            })
            .collect();

        Ok(indexes)
    }

    /// The table's FK / CHECK / UNIQUE records — the side record `Schema` does
    /// not carry, and which a v1 dump therefore dropped entirely (HDB-003).
    fn get_table_constraints(&self, table: &str) -> Result<crate::sql::TableConstraints> {
        let catalog = self.storage.catalog();
        catalog.load_table_constraints(table)
    }
}

impl DatabaseRestoreInterface for EmbeddedDatabase {
    fn create_table(&mut self, name: &str, schema: Schema) -> Result<()> {
        let catalog = self.storage.catalog();
        catalog.create_table(name, schema)?;
        Ok(())
    }

    fn create_index(&mut self, table: &str, index: &IndexMetadata) -> Result<()> {
        // Build and execute CREATE INDEX SQL statement
        // Handle different index types (hnsw, btree, etc.)
        let using_clause = match index.index_type.as_str() {
            "hnsw" | "hnsw_pq" => "USING hnsw",
            "btree" => "", // Default type
            "hash" => "USING hash",
            "gin" => "USING gin",
            _ => "", // Default to btree
        };

        let columns = index.columns.join(", ");
        let unique_clause = if index.is_unique { "UNIQUE " } else { "" };

        let sql = format!(
            "CREATE {}INDEX {} ON {} {} ({})",
            unique_clause, index.name, table, using_clause, columns
        );

        // Execute the CREATE INDEX statement
        self.execute(&sql)?;
        Ok(())
    }

    fn insert_row(&mut self, table: &str, row: Tuple) -> Result<()> {
        self.storage.insert_tuple(table, row)?;
        Ok(())
    }

    /// Phase B — register the table's constraints through the SAME catalog
    /// funnels `CREATE TABLE` and `ALTER TABLE … ADD CONSTRAINT` use, so DDL
    /// and restore install identical enforcement.
    ///
    /// Order matters, and so does which helper does what:
    ///
    /// 1. CHECKs and the PRIMARY KEY records are saved directly: a CHECK needs
    ///    no index, and the PK's ART index was built by `catalog.create_table`
    ///    in phase A from the schema's column flags (so the rows inserted in
    ///    phase A are already in it).
    /// 2. Every non-PK UNIQUE goes through `alter_table_add_unique`, which
    ///    records the constraint, mints the enforcing ART index when the column
    ///    set has none — the composite `UNIQUE (a, b)` case, invisible to
    ///    `create_table`, which only sees `Schema` — and BACKFILLS it from the
    ///    rows already restored, failing closed on a duplicate. A
    ///    single-column UNIQUE that phase A already indexed from the column
    ///    flag is recorded without a second index (`constraint_owned_unique_index_on`).
    /// 3. Every FK goes through `catalog.add_foreign_key`, which appends it to
    ///    the saved record and creates the FK lookup ART index — and backfills
    ///    that index from the rows already in the table, which is exactly the
    ///    "FK added after the data" order a restore produces.
    fn restore_table_constraints(&mut self, table: &str, constraints: &crate::sql::TableConstraints) -> Result<()> {
        let mut base = crate::sql::TableConstraints::new();
        base.check_constraints = constraints.check_constraints.clone();
        base.unique_constraints = constraints
            .unique_constraints
            .iter()
            .filter(|uc| uc.is_primary_key)
            .cloned()
            .collect();
        self.storage.catalog().save_table_constraints(table, &base)?;

        for uc in &constraints.unique_constraints {
            if uc.is_primary_key || uc.columns.is_empty() {
                continue;
            }
            let _ = self.alter_table_add_unique(table, &Some(uc.name.clone()), &uc.columns)?;
        }

        for fk in &constraints.foreign_keys {
            // A partial restore can carry a child whose parent is in neither
            // the dump nor the target. `add_foreign_key` does not refuse a
            // dangling reference (only `CREATE TABLE` / `ALTER TABLE` validate
            // the target, through `validate_fk_reference`), so the constraint
            // is registered either way and starts enforcing as soon as the
            // parent appears; phase C skips it and says so.
            if !self
                .storage
                .catalog()
                .table_exists(&fk.references_table)
                .unwrap_or(false)
            {
                note_restore_warning(format!(
                    "{}.{}: referenced table \"{}\" is not in the dump and does not exist in the target; \
                     the constraint was registered but its rows could not be validated",
                    table, fk.name, fk.references_table
                ));
            }
            self.storage.catalog().add_foreign_key(fk.clone())?;
        }

        Ok(())
    }

    /// Phase C — check the restored ROWS against the constraint graph that
    /// phase B registered for every table.
    ///
    /// Every FK is probed with a key encoding identical to the write path's
    /// (`check_referencing_rows_exist` — both sides of the probe go through
    /// `coerce_fk_probe_values` + `ArtIndexManager::encode_key`), and every
    /// CHECK is evaluated with the same compiled expression the bulk write path
    /// uses (`compile_check_constraints`). Violations are collected — up to
    /// [`MAX_REPORTED_VIOLATIONS`] keys per constraint — into ONE error naming
    /// every constraint and its offending keys, because an operator restoring a
    /// bad dump wants the whole picture, not the first row of it. The manager
    /// aggregates the tables in turn, so the error names all of them.
    ///
    /// Not every violation is fatal. The write path has four non-fatal cases
    /// and this validator honours all of them, or a database that legitimately
    /// holds violating rows could be backed up but never restored:
    /// `NOT ENFORCED` (skipped outright), `LOCK-FREE`, a session in
    /// `helios.fk_validation = 'audit'`, and `fk_validation = 'off'` /
    /// `source = proxy`. [`RestoreValidation::Lenient`] (`--no-validate`) adds a
    /// fifth: the operator has asked for the data as it is. Every non-fatal
    /// case is recorded with [`note_restore_warning`] and reaches the caller in
    /// `RestoreReport::validation_warnings`.
    ///
    /// Memory: the table's rows are materialised once
    /// (`scan_table_with_schema` returns a `Vec<Tuple>`), and so are a parent
    /// table's FK keys. The engine's paged scan (`scan_table_with_offset_limit`)
    /// is deliberately NOT used here: it re-reads raw rows without the
    /// visibility handling `scan_table_with_schema` performs, and phase C must
    /// see exactly the rows the write path would.
    fn validate_restored_constraints(&mut self, table: &str, validation: RestoreValidation) -> Result<()> {
        let (constraints, schema) = {
            let catalog = self.storage.catalog();
            (catalog.load_table_constraints(table)?, catalog.get_table_schema(table)?)
        };
        if constraints.foreign_keys.is_empty() && constraints.check_constraints.is_empty() {
            return Ok(());
        }

        let rows = self.storage.scan_table_with_schema(table, &schema)?;
        let mut problems: Vec<String> = Vec::new();

        // The session's FK validation mode is an explicit operator opt-out of
        // FK enforcement; honour it here exactly as `check_fk_constraints_on_write`
        // does, rather than failing a restore the same session's writes would
        // have accepted.
        let fk_mode = *self.fk_validation_mode.read();
        let fk_checks_disabled = fk_mode == crate::FkValidationMode::Off
            || *self.fk_validation_source.read() == crate::FkValidationSource::Proxy;

        for fk in &constraints.foreign_keys {
            if fk.enforcement == crate::sql::ConstraintEnforcement::NotEnforced {
                continue;
            }
            if fk_checks_disabled {
                note_restore_warning(format!(
                    "{}.{}: foreign-key validation is disabled for this session; \
                     the constraint was registered but its rows were not validated",
                    table, fk.name
                ));
                continue;
            }
            if !self
                .storage
                .catalog()
                .table_exists(&fk.references_table)
                .unwrap_or(false)
            {
                // Already reported by phase B.
                continue;
            }
            let Some(positions) = fk
                .columns
                .iter()
                .map(|c| schema.columns.iter().position(|sc| sc.name.eq_ignore_ascii_case(c)))
                .collect::<Option<Vec<usize>>>()
            else {
                note_restore_warning(format!(
                    "{}.{}: the constraint names a column the restored schema does not have; \
                     its rows were not validated",
                    table, fk.name
                ));
                continue;
            };

            // ONE probe structure per FK, not one parent scan per distinct key.
            let Some(probe) = self.build_parent_probe(fk)? else {
                note_restore_warning(format!(
                    "{}.{}: the referenced table \"{}\" does not have the referenced column(s); \
                     its rows were not validated",
                    table, fk.name, fk.references_table
                ));
                continue;
            };

            let mut violations = 0u64;
            let mut samples: Vec<String> = Vec::new();
            // Used by the ART-index probe only: one lookup per DISTINCT key.
            // Deduped on the ART key encoding — a missed dedup (two equal
            // values of different declared types) only costs a lookup.
            let mut probed: std::collections::HashSet<Vec<u8>> = std::collections::HashSet::new();
            let mut missing: std::collections::HashSet<Vec<u8>> = std::collections::HashSet::new();

            for row in &rows {
                let mut values: Vec<Value> = Vec::with_capacity(positions.len());
                let mut any_null = false;
                for &i in &positions {
                    match row.values.get(i) {
                        Some(Value::Null) | None => {
                            any_null = true;
                            break;
                        }
                        Some(v) => values.push(v.clone()),
                    }
                }
                // PostgreSQL MATCH SIMPLE: a NULL in any FK column makes the
                // row trivially valid. Same rule as the write path.
                if any_null {
                    continue;
                }

                let exists = match &probe {
                    ParentProbe::Keys(parent_schema, keys) => {
                        let key = Self::encode_fk_key(parent_schema, &fk.references_columns, &values);
                        keys.contains(&key)
                    }
                    ParentProbe::WritePath => {
                        let key = crate::storage::ArtIndexManager::encode_key(&values);
                        if probed.contains(&key) {
                            !missing.contains(&key)
                        } else {
                            let exists = self.check_referencing_rows_exist(
                                &fk.references_table,
                                &fk.references_columns,
                                &values,
                                None,
                            )?;
                            probed.insert(key.clone());
                            if !exists {
                                missing.insert(key);
                            }
                            exists
                        }
                    }
                };

                if !exists {
                    violations += 1;
                    if samples.len() < MAX_REPORTED_VIOLATIONS {
                        samples.push(fk_key_repr(&fk.columns, &values));
                    }
                }
            }

            if violations > 0 {
                let detail = format!(
                    "{}.{}: {} {} missing {} rows ({})",
                    table,
                    fk.name,
                    rows_phrase(violations),
                    references_verb(violations),
                    fk.references_table,
                    samples.join("; ")
                );
                match fk_violation_is_non_fatal(fk, fk_mode, validation) {
                    Some(reason) => note_restore_warning(format!("{detail}; {reason}")),
                    None => problems.push(detail),
                }
            }
        }

        for check in &constraints.check_constraints {
            // FIX: compile ONCE per constraint — `compile_check_constraints` is
            // the same helper the bulk COPY path uses (one parse, one
            // `Evaluator::bind`, one `Arc<Schema>`), where the per-row
            // `evaluate_check_constraint` re-ran sqlparser AND cloned the
            // schema for every row of every table. Compiled per constraint
            // rather than for the whole set so one unevaluable expression is
            // attributed to its own constraint instead of hiding the others.
            let mut one = crate::sql::TableConstraints::new();
            one.check_constraints = vec![check.clone()];
            let compiled = match self.compile_check_constraints(&one, &schema) {
                Ok(compiled) => compiled,
                Err(e) => {
                    let reason = e.to_string();
                    note_restore_warning(unevaluable_check_warning(table, &check.name, &reason));
                    continue;
                }
            };
            let Some(bound) = compiled.checks.first() else {
                continue;
            };

            let mut violations = 0u64;
            let mut samples: Vec<String> = Vec::new();
            let mut unevaluable: Option<String> = None;

            for row in &rows {
                match compiled.evaluator.evaluate(&bound.expr, row) {
                    // SQL three-valued logic: NULL/UNKNOWN PASSES, exactly as
                    // `evaluate_check_constraint` and the write path do.
                    Ok(Value::Boolean(true)) | Ok(Value::Null) => {}
                    Ok(Value::Boolean(false)) => {
                        violations += 1;
                        if samples.len() < MAX_REPORTED_VIOLATIONS {
                            samples.push(row_key_repr(&schema, row));
                        }
                    }
                    Ok(_) => {
                        unevaluable = Some(format!(
                            "CHECK constraint expression '{}' did not evaluate to boolean",
                            check.expression
                        ));
                        break;
                    }
                    Err(e) => {
                        // The expression fails the same way for every row, so
                        // stop at the first. Not fatal: the constraint IS
                        // registered and the same failure will reject new
                        // writes, which is the fail-closed side.
                        unevaluable = Some(e.to_string());
                        break;
                    }
                }
            }

            if let Some(reason) = unevaluable {
                note_restore_warning(unevaluable_check_warning(table, &check.name, &reason));
                continue;
            }
            if violations > 0 {
                let detail = format!(
                    "{}.{}: {} {} the CHECK constraint; offending rows: {}",
                    table,
                    check.name,
                    rows_phrase(violations),
                    if violations == 1 { "violates" } else { "violate" },
                    samples.join("; ")
                );
                if validation.is_lenient() {
                    note_restore_warning(format!("{detail}; {LENIENT_REASON}"));
                } else {
                    problems.push(detail);
                }
            }
        }

        if problems.is_empty() {
            return Ok(());
        }
        // No "restore validation failed" prefix: the manager aggregates every
        // table's problems under one.
        Err(Error::constraint_violation(problems.join("; ")))
    }

    /// Refuse a restore that phase B could not finish: index state lives on
    /// main, so `alter_table_add_unique` (the funnel phase B registers every
    /// non-PK UNIQUE through) rejects a session with a branch checked out. Said
    /// here, the target is still untouched; said from inside phase B, it would
    /// arrive with every table already created and populated.
    fn begin_restore(&mut self) -> Result<()> {
        if self.storage.is_branch_active() {
            return Err(Error::query_execution(
                "restore must run on the main branch: registering the dump's UNIQUE and FOREIGN KEY \
                 constraints mints indexes, and index state lives on main. Run `USE BRANCH main;` \
                 and restore again.",
            ));
        }
        Ok(())
    }

    fn take_restore_warnings(&mut self) -> Vec<String> {
        RESTORE_WARNINGS.with(|w| std::mem::take(&mut *w.borrow_mut()))
    }
}
