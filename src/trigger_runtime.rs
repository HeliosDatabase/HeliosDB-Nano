//! Triggers on the engine side: which fire, when, on which transaction, and
//! what becomes of the row their function returns.
//!
//! How a statement fires triggers:
//!
//! * Every statement of both executor families runs inside
//!   [`EmbeddedDatabase::with_trigger_statement`]. It opens a frame for the
//!   statement on the executing thread.
//! * A DML arm asks once for its table's [`TriggerGate`]
//!   ([`EmbeddedDatabase::trigger_statement`]). For a table without an
//!   enabled trigger for that event the answer is `None` (one atomic load
//!   when the database has no trigger that runs a function) and the arm does
//!   nothing more. Otherwise the BEFORE STATEMENT triggers fire there.
//! * Per row the arm calls [`EmbeddedDatabase::trigger_before_row`] (the
//!   function may change the row or skip it) and, once the row is staged,
//!   [`TriggerGate::after_row`], which queues the row.
//! * When the arm has finished, the frame fires the queued AFTER ROW
//!   triggers and then the AFTER STATEMENT triggers, as PostgreSQL does, on
//!   the statement's transaction and before it commits.
//!
//! A trigger function's SQL runs on the firing transaction through the same
//! entry point a procedure body uses, so its writes commit or roll back with
//! the statement that fired it, pass the same gates as any statement, and
//! fire the triggers of the tables they write to (nesting is bounded, see
//! [`crate::sql::trigger_exec::MAX_TRIGGER_NESTING`]).
//!
//! A write route that reaches a table with a trigger for its event without a
//! frame or without a transaction cannot fire it and is refused (0A000)
//! instead of writing past the trigger.

use crate::sql::logical_plan::{TriggerEvent, TriggerFor, TriggerTiming, TriggerType};
use crate::sql::procedural::ProceduralBlock;
use crate::sql::trigger_exec::{
    self, InlineTriggerBody, NestingGuard, TriggerCall, TriggerExecInfo, TriggerOp, TriggerReturn,
    INLINE_TRIGGER_FUNCTION, RECORD_NOT_ASSIGNED_MARKER, TRIGGER_ROUTE_MARKER, TRIGGER_UNSUPPORTED_MARKER,
};
use crate::sql::trigger_syntax::{decode_drop_function, TriggerTarget, ANY_TABLE};
use crate::sql::triggers::{FiringTrigger, TableTriggers, TriggerDefinition, TriggerRowContext};
use crate::{storage, EmbeddedDatabase, Error, Result, Schema, SessionNotice, Tuple, Value};
use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::Arc;

/// Marks the refusal to drop a function a trigger runs (SQLSTATE 2BP01).
pub const TRIGGER_DEPENDS_MARKER: &str = "cannot be dropped because some objects depend on it";

struct GateInner {
    table: String,
    schema: Arc<Schema>,
    op: TriggerOp,
    /// The columns an UPDATE assigns (for `UPDATE OF`).
    updated: Vec<String>,
    set: Arc<TableTriggers>,
}

/// A table's triggers for one statement.
pub(crate) struct TriggerGate {
    inner: Arc<GateInner>,
    /// A BEFORE ROW trigger fires for this statement.
    pub(crate) before_row: bool,
    /// An AFTER ROW trigger fires for this statement.
    pub(crate) after_row: bool,
}

struct AfterRow {
    gate: Arc<GateInner>,
    old: Option<Tuple>,
    new: Option<Tuple>,
}

#[derive(Default)]
struct Frame {
    /// The database had a trigger that runs a function when the statement began.
    armed: bool,
    after_rows: Vec<AfterRow>,
    after_statements: Vec<Arc<GateInner>>,
}

thread_local! {
    static FRAMES: RefCell<Vec<Frame>> = const { RefCell::new(Vec::new()) };
}

/// Pops the statement's frame if the statement unwinds.
struct FrameGuard {
    live: bool,
}

impl FrameGuard {
    fn open(armed: bool) -> Self {
        FRAMES.with(|frames| {
            frames.borrow_mut().push(Frame {
                armed,
                ..Frame::default()
            });
        });
        Self { live: true }
    }

    fn close(mut self) -> Option<Frame> {
        self.live = false;
        FRAMES.with(|frames| frames.borrow_mut().pop())
    }
}

impl Drop for FrameGuard {
    fn drop(&mut self) {
        if self.live {
            FRAMES.with(|frames| {
                frames.borrow_mut().pop();
            });
        }
    }
}

impl TriggerGate {
    /// Queue a staged row for the statement's AFTER ROW triggers.
    pub(crate) fn after_row(&self, old: Option<&Tuple>, new: Option<&Tuple>) {
        if !self.after_row {
            return;
        }
        FRAMES.with(|frames| {
            if let Some(frame) = frames.borrow_mut().last_mut() {
                frame.after_rows.push(AfterRow {
                    gate: Arc::clone(&self.inner),
                    old: old.cloned(),
                    new: new.cloned(),
                });
            }
        });
    }
}

fn fires(definition: &TriggerDefinition, op: TriggerOp, updated: &[String]) -> bool {
    definition.events.iter().any(|event| match (event, op) {
        (TriggerEvent::Insert, TriggerOp::Insert)
        | (TriggerEvent::Delete, TriggerOp::Delete)
        | (TriggerEvent::Truncate, TriggerOp::Truncate)
        | (TriggerEvent::Update(None), TriggerOp::Update) => true,
        (TriggerEvent::Update(Some(columns)), TriggerOp::Update) => columns
            .iter()
            .any(|c| updated.iter().any(|u| u.eq_ignore_ascii_case(c))),
        _ => false,
    })
}

fn route_error(table: &str, why: &str) -> Error {
    Error::query_execution(format!(
        "this write route {TRIGGER_ROUTE_MARKER} \"{table}\" ({why}); the statement was refused and nothing was \
         written. Write the rows with INSERT, UPDATE or DELETE, or disable the triggers first (ALTER TABLE {table} \
         DISABLE TRIGGER ALL)"
    ))
}

/// Parsed trigger function bodies, by body text (a pure function of it).
fn block_for(body: &str) -> Result<Arc<ProceduralBlock>> {
    static BLOCKS: std::sync::OnceLock<parking_lot::Mutex<HashMap<String, Arc<ProceduralBlock>>>> =
        std::sync::OnceLock::new();
    let blocks = BLOCKS.get_or_init(|| parking_lot::Mutex::new(HashMap::new()));
    if let Some(hit) = blocks.lock().get(body) {
        return Ok(Arc::clone(hit));
    }
    let block = Arc::new(trigger_exec::prepare_block(body)?);
    let mut guard = blocks.lock();
    if guard.len() >= 512 {
        guard.clear();
    }
    guard.insert(body.to_string(), Arc::clone(&block));
    Ok(block)
}

impl EmbeddedDatabase {
    /// Run one statement with a trigger frame: after `run` succeeded, fire the
    /// AFTER ROW triggers it queued and the AFTER STATEMENT triggers it
    /// announced, on `txn`.
    pub(crate) fn with_trigger_statement<T>(
        &self,
        txn: Option<&storage::Transaction>,
        run: impl FnOnce() -> Result<T>,
    ) -> Result<T> {
        let guard = FrameGuard::open(self.trigger_registry.has_exec_triggers());
        let result = run();
        let frame = guard.close();
        let out = result?;
        let Some(frame) = frame else { return Ok(out) };
        if frame.after_rows.is_empty() && frame.after_statements.is_empty() {
            return Ok(out);
        }
        // A session-less statement inside an embedded BEGIN ran its arm on the
        // global transaction; the triggers run on it too.
        let global_guard = match txn {
            None if !crate::GLOBAL_TXN_LOCK_HELD.with(|held| held.get()) => self.lock_owned_global_txn(),
            _ => None,
        };
        let txn = txn.or_else(|| global_guard.as_ref().and_then(|guard| guard.as_ref()));
        let Some(txn) = txn else {
            let table = frame
                .after_rows
                .first()
                .map(|row| row.gate.table.clone())
                .or_else(|| frame.after_statements.first().map(|gate| gate.table.clone()))
                .unwrap_or_default();
            return Err(route_error(&table, "the statement has no transaction to run them in"));
        };
        for row in &frame.after_rows {
            for trigger in &row.gate.set.triggers {
                let definition = &trigger.definition;
                if definition.timing != TriggerTiming::After
                    || definition.for_each != TriggerFor::Row
                    || !fires(definition, row.gate.op, &row.gate.updated)
                    || !Self::trigger_when(trigger, &row.gate, row.old.as_ref(), row.new.as_ref())?
                {
                    continue;
                }
                self.fire_trigger(trigger, &row.gate, row.old.as_ref(), row.new.as_ref(), txn)?;
            }
        }
        for gate in &frame.after_statements {
            for trigger in &gate.set.triggers {
                let definition = &trigger.definition;
                if definition.timing == TriggerTiming::After
                    && definition.for_each == TriggerFor::Statement
                    && fires(definition, gate.op, &gate.updated)
                {
                    self.fire_trigger(trigger, gate, None, None, txn)?;
                }
            }
        }
        Ok(out)
    }

    /// The triggers of `table` for one statement of kind `op`; fires its
    /// BEFORE STATEMENT triggers. `None`: no enabled trigger of the table
    /// fires for this statement.
    pub(crate) fn trigger_statement(
        &self,
        table: &str,
        op: TriggerOp,
        updated: &[String],
        schema: &Schema,
        txn: Option<&storage::Transaction>,
    ) -> Result<Option<TriggerGate>> {
        let Some(set) = self.trigger_registry.firing_triggers(table) else {
            return Ok(None);
        };
        let mut before_row = false;
        let mut after_row = false;
        let mut before_statement = false;
        let mut after_statement = false;
        for trigger in &set.triggers {
            let definition = &trigger.definition;
            if !fires(definition, op, updated) {
                continue;
            }
            match (&definition.timing, &definition.for_each) {
                (TriggerTiming::Before, TriggerFor::Row) => before_row = true,
                (TriggerTiming::After, TriggerFor::Row) => after_row = true,
                (TriggerTiming::Before, TriggerFor::Statement) => before_statement = true,
                (TriggerTiming::After, TriggerFor::Statement) => after_statement = true,
                _ => {}
            }
        }
        if !(before_row || after_row || before_statement || after_statement) {
            return Ok(None);
        }
        // A statement that began before the database had any trigger does not
        // fire one created while it ran.
        let framed = FRAMES.with(|frames| frames.borrow().last().map(|frame| frame.armed));
        match framed {
            Some(true) => {}
            Some(false) => return Ok(None),
            None => return Err(route_error(table, "it does not run as a statement")),
        }
        let Some(txn) = txn else {
            return Err(route_error(table, "the statement has no transaction to run them in"));
        };
        let inner = Arc::new(GateInner {
            table: table.to_string(),
            schema: Arc::new(schema.clone()),
            op,
            updated: updated.to_vec(),
            set,
        });
        if before_statement {
            for trigger in &inner.set.triggers {
                let definition = &trigger.definition;
                if definition.timing == TriggerTiming::Before
                    && definition.for_each == TriggerFor::Statement
                    && fires(definition, op, updated)
                {
                    self.fire_trigger(trigger, &inner, None, None, txn)?;
                }
            }
        }
        if after_statement {
            FRAMES.with(|frames| {
                if let Some(frame) = frames.borrow_mut().last_mut() {
                    frame.after_statements.push(Arc::clone(&inner));
                }
            });
        }
        Ok(Some(TriggerGate {
            inner,
            before_row,
            after_row,
        }))
    }

    /// Queue an inserted row for the AFTER INSERT row triggers, as it was
    /// stored: with the key the write filled in.
    pub(crate) fn trigger_after_insert(
        &self,
        gate: Option<&TriggerGate>,
        table: &str,
        schema: &Schema,
        tuple: &Tuple,
        row_id: u64,
    ) -> Result<()> {
        let Some(gate) = gate.filter(|g| g.after_row) else {
            return Ok(());
        };
        let mut stored = tuple.clone();
        for (value, column) in stored.values.iter_mut().zip(&schema.columns) {
            if column.primary_key && matches!(value, Value::Null) {
                *value = crate::pk_autofill_value(table, column, row_id)?;
            }
        }
        stored.row_id = Some(row_id);
        gate.after_row(None, Some(&stored));
        Ok(())
    }

    /// [`Self::trigger_after_insert`] for rows staged as a chunk: `pending`
    /// holds them in the order of `ids`.
    pub(crate) fn trigger_after_insert_chunk(
        &self,
        gate: Option<&TriggerGate>,
        table: &str,
        schema: &Schema,
        pending: &mut Vec<Tuple>,
        ids: &[u64],
    ) -> Result<()> {
        for (tuple, row_id) in pending.drain(..).zip(ids) {
            self.trigger_after_insert(gate, table, schema, &tuple, *row_id)?;
        }
        Ok(())
    }

    /// Fire the row triggers of a child table for one row a foreign-key
    /// action deletes (`new` is `None`) or updates. `false`: a BEFORE trigger
    /// returned NULL and the action leaves this row alone, as in PostgreSQL.
    /// The child's statement-level triggers are not fired by an action.
    pub(crate) fn trigger_fk_child_row(
        &self,
        txn: &storage::Transaction,
        table: &str,
        schema: &Schema,
        fk_columns: &[String],
        old: &Tuple,
        new: Option<&mut Tuple>,
    ) -> Result<bool> {
        let Some(set) = self.trigger_registry.firing_triggers(table) else {
            return Ok(true);
        };
        let (op, updated) = if new.is_some() {
            (TriggerOp::Update, fk_columns.to_vec())
        } else {
            (TriggerOp::Delete, Vec::new())
        };
        let row_trigger = |timing: TriggerTiming| {
            set.triggers.iter().any(|t| {
                t.definition.timing == timing
                    && t.definition.for_each == TriggerFor::Row
                    && fires(&t.definition, op, &updated)
            })
        };
        let gate = TriggerGate {
            before_row: row_trigger(TriggerTiming::Before),
            after_row: row_trigger(TriggerTiming::After),
            inner: Arc::new(GateInner {
                table: table.to_string(),
                schema: Arc::new(schema.clone()),
                op,
                updated,
                set,
            }),
        };
        if !(gate.before_row || gate.after_row) {
            return Ok(true);
        }
        if FRAMES.with(|frames| frames.borrow().is_empty()) {
            return Err(route_error(table, "a foreign-key action outside a statement"));
        }
        match new {
            Some(new) => {
                if !self.trigger_before_row(&gate, Some(old), Some(&mut *new), txn)? {
                    return Ok(false);
                }
                gate.after_row(Some(old), Some(new));
            }
            None => {
                if !self.trigger_before_row(&gate, Some(old), None, txn)? {
                    return Ok(false);
                }
                gate.after_row(Some(old), None);
            }
        }
        Ok(true)
    }

    /// Does `table` have an enabled trigger that runs a function? One atomic
    /// load when the database has none.
    #[inline]
    pub(crate) fn table_fires_triggers(&self, table: &str) -> bool {
        self.trigger_registry.firing_triggers(table).is_some()
    }

    /// Refuse a write route that cannot fire `table`'s triggers for `op`.
    pub(crate) fn refuse_route_past_triggers(&self, table: &str, op: TriggerOp, route: &str) -> Result<()> {
        let Some(set) = self.trigger_registry.firing_triggers(table) else {
            return Ok(());
        };
        let any_update = |definition: &TriggerDefinition| {
            op == TriggerOp::Update && definition.events.iter().any(|e| matches!(e, TriggerEvent::Update(_)))
        };
        if set
            .triggers
            .iter()
            .any(|t| fires(&t.definition, op, &[]) || any_update(&t.definition))
        {
            return Err(route_error(table, route));
        }
        Ok(())
    }

    /// Fire the BEFORE ROW triggers for one row. `new` is the row about to be
    /// written (INSERT, UPDATE) and takes what the functions return; `false`
    /// means a function returned NULL and the row is skipped.
    pub(crate) fn trigger_before_row(
        &self,
        gate: &TriggerGate,
        old: Option<&Tuple>,
        mut new: Option<&mut Tuple>,
        txn: &storage::Transaction,
    ) -> Result<bool> {
        if !gate.before_row {
            return Ok(true);
        }
        let inner = &gate.inner;
        for trigger in &inner.set.triggers {
            let definition = &trigger.definition;
            if definition.timing != TriggerTiming::Before
                || definition.for_each != TriggerFor::Row
                || !fires(definition, inner.op, &inner.updated)
                || !Self::trigger_when(trigger, inner, old, new.as_deref())?
            {
                continue;
            }
            match self.fire_trigger(trigger, inner, old, new.as_deref(), txn)? {
                TriggerReturn::Null => return Ok(false),
                TriggerReturn::New(mut values, assigned) => {
                    let Some(row) = new.as_deref_mut() else {
                        continue;
                    };
                    let evaluator = crate::sql::Evaluator::new(Arc::new(Schema { columns: vec![] }));
                    for ((value, column), assigned) in values.iter_mut().zip(&inner.schema.columns).zip(&assigned) {
                        if *assigned && !matches!(value, Value::Null) {
                            *value = evaluator.cast_value(std::mem::replace(value, Value::Null), &column.data_type)?;
                        }
                    }
                    row.values = values;
                }
                TriggerReturn::Old => match (inner.op, old, new.as_deref_mut()) {
                    (TriggerOp::Update, Some(old), Some(row)) => row.values = old.values.clone(),
                    (TriggerOp::Delete, _, _) => {}
                    _ => {
                        return Err(Error::query_execution(format!(
                            "record \"old\" {RECORD_NOT_ASSIGNED_MARKER}: trigger \"{}\" returned OLD for an INSERT",
                            definition.name
                        )))
                    }
                },
            }
        }
        Ok(true)
    }

    fn trigger_when(
        trigger: &FiringTrigger,
        gate: &GateInner,
        old: Option<&Tuple>,
        new: Option<&Tuple>,
    ) -> Result<bool> {
        let Some(condition) = &trigger.definition.when_condition else {
            return Ok(true);
        };
        let context = match (old, new) {
            (Some(old), Some(new)) => TriggerRowContext::for_update(old.clone(), new.clone()),
            (None, Some(new)) => TriggerRowContext::for_insert(new.clone()),
            (Some(old), None) => TriggerRowContext::for_delete(old.clone()),
            (None, None) => return Ok(true),
        };
        context
            .evaluate_when_condition(condition, Arc::clone(&gate.schema))
            .map_err(|e| {
                Error::query_execution(format!(
                    "the WHEN condition of trigger \"{}\" failed: {e}",
                    trigger.definition.name
                ))
            })
    }

    /// Run one trigger's function for one firing, on `txn`.
    fn fire_trigger(
        &self,
        trigger: &FiringTrigger,
        gate: &GateInner,
        old: Option<&Tuple>,
        new: Option<&Tuple>,
        txn: &storage::Transaction,
    ) -> Result<TriggerReturn> {
        let definition = &trigger.definition;
        let _nesting = NestingGuard::enter(&definition.name)?;
        let block = self.trigger_block(&trigger.exec, &definition.name)?;
        let (table_schema, table_name) = crate::sql::Planner::split_schema_key(&gate.table);
        let row_level = definition.for_each == TriggerFor::Row;
        let call = TriggerCall {
            trigger_name: &definition.name,
            table_name: &table_name,
            table_schema: &table_schema,
            op: gate.op,
            when: match definition.timing {
                TriggerTiming::Before => "BEFORE",
                TriggerTiming::After => "AFTER",
                TriggerTiming::InsteadOf => "INSTEAD OF",
            },
            level: if row_level { "ROW" } else { "STATEMENT" },
            args: &trigger.exec.args,
            schema: &gate.schema,
            new: new.filter(|_| row_level).map(|t| t.values.clone()),
            old: old.filter(|_| row_level).map(|t| t.values.clone()),
            implicit_return: trigger.exec.inline.is_some(),
        };
        // Inside an embedded BEGIN the caller (the text entry point, a params
        // arm, or the statement frame) holds the global transaction slot while
        // the function runs: its statements run on `txn` and must not go for
        // the slot again.
        let _slot_held = self.global_txn_visible().then(crate::GlobalTxnLockMarker::set);
        // Spec R5.6: a trigger function may run anything a procedure body may.
        storage::layout_epochs::note_side_effect("a trigger function ran");
        let mut run_sql = |sql: &str| self.execute_routine_body_in_transaction(sql, txn);
        let mut notice = |level: &str, message: &str| {
            match level {
                "WARNING" => tracing::warn!("[RAISE WARNING] {}", message),
                "DEBUG" | "LOG" => tracing::debug!("[RAISE {}] {}", level, message),
                _ => tracing::info!("[RAISE {}] {}", level, message),
            }
            if matches!(level, "WARNING" | "NOTICE" | "INFO") {
                if let Some(state) = crate::session_scoped_state_tls() {
                    state.push_notice(SessionNotice {
                        severity: level.to_string(),
                        code: if level == "WARNING" { "01000" } else { "00000" }.to_string(),
                        message: message.to_string(),
                    });
                }
            }
        };
        trigger_exec::run_trigger_function(
            &block,
            call,
            self.function_registry.procedural_limits(),
            &mut run_sql,
            &mut notice,
        )
        .map_err(|e| match e {
            Error::QueryExecution(message) if !message.contains("trigger \"") => Error::QueryExecution(format!(
                "{message} (trigger \"{}\" on table \"{}\")",
                definition.name, gate.table
            )),
            other => other,
        })
    }

    /// The parsed body a trigger runs: its inline body, or its function's as
    /// that function is defined now (so a replaced function takes effect and
    /// a dropped one is reported).
    fn trigger_block(&self, exec: &TriggerExecInfo, trigger: &str) -> Result<Arc<ProceduralBlock>> {
        if let Some(inline) = &exec.inline {
            return block_for(&inline.block);
        }
        let Some(name) = exec.function.as_deref() else {
            return Err(Error::query_execution(format!(
                "trigger \"{trigger}\" has no function recorded; re-create it"
            )));
        };
        let Some(function) = self.function_registry.get_function(name) else {
            self.function_registry.function_exists_checked(name)?;
            return Err(Error::query_execution(format!(
                "function {name}() does not exist (trigger \"{trigger}\" runs it)"
            )));
        };
        Self::require_trigger_language(&function.language, name)?;
        block_for(&function.body)
    }

    fn require_trigger_language(language: &str, function: &str) -> Result<()> {
        if language.eq_ignore_ascii_case("plpgsql") {
            return Ok(());
        }
        Err(Error::query_execution(format!(
            "function {function}() is written in LANGUAGE {language}; a trigger function in a language other \
             than plpgsql: this {TRIGGER_UNSUPPORTED_MARKER}"
        )))
    }

    // ------------------------------------------------------------------ DDL

    /// `CREATE [OR REPLACE] TRIGGER` for a statement that went through
    /// `trigger_syntax::preprocess` (every statement the parser sees does).
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn create_trigger_with_target(
        &self,
        name: &str,
        table_name: &str,
        timing: &TriggerTiming,
        events: &[TriggerEvent],
        for_each: &TriggerFor,
        when_condition: &Option<Box<crate::sql::LogicalExpr>>,
        referencing: &[crate::sql::logical_plan::TransitionTable],
        characteristics: &crate::sql::TriggerCharacteristics,
        trigger_type: &TriggerType,
        from_constraint: &Option<String>,
        target: TriggerTarget,
    ) -> Result<u64> {
        let refuse = |what: &str| -> Result<u64> {
            Err(Error::query_execution(format!(
                "CREATE TRIGGER \"{name}\": {what}: this {TRIGGER_UNSUPPORTED_MARKER}, and a trigger that would \
                 never fire is not created"
            )))
        };
        if matches!(timing, TriggerTiming::InsteadOf) {
            return refuse("INSTEAD OF triggers (on views)");
        }
        if matches!(trigger_type, TriggerType::Constraint)
            || characteristics.deferrable
            || characteristics.initially_deferred
        {
            return refuse("constraint and deferrable triggers");
        }
        if !referencing.is_empty() {
            return refuse("REFERENCING transition tables");
        }
        // The table must exist (a view has no row to fire for).
        let schema = self.storage.catalog().get_table_schema(table_name)?;
        if let Some(condition) = when_condition {
            if matches!(for_each, TriggerFor::Statement) {
                let text = format!("{condition:?}");
                if text.contains("NEW") || text.contains("OLD") || text.contains("Column") {
                    return refuse("a WHEN condition that reads the row on a statement-level trigger");
                }
            }
        }
        for event in events {
            if let TriggerEvent::Update(Some(columns)) = event {
                for column in columns {
                    if !schema.columns.iter().any(|c| c.name.eq_ignore_ascii_case(column)) {
                        return Err(Error::query_execution(format!(
                            "column \"{column}\" of relation \"{table_name}\" does not exist"
                        )));
                    }
                }
            }
        }

        let info = if target.function == INLINE_TRIGGER_FUNCTION {
            let mut args = target.args.into_iter();
            let (Some(dialect), Some(source), Some(block)) = (args.next(), args.next(), args.next()) else {
                return Err(Error::internal("inline trigger body is incomplete"));
            };
            trigger_exec::prepare_block(&block)?;
            TriggerExecInfo {
                function: None,
                args: Vec::new(),
                inline: Some(InlineTriggerBody { dialect, source, block }),
            }
        } else {
            let function = target.function.to_lowercase();
            if !self.function_registry.function_exists_checked(&function)? {
                return Err(Error::query_execution(format!("function {function}() does not exist")));
            }
            if let Some(stored) = self.function_registry.get_function(&function) {
                Self::require_trigger_language(&stored.language, &function)?;
                trigger_exec::prepare_block(&stored.body)?;
            }
            TriggerExecInfo {
                function: Some(function),
                args: target.args,
                inline: None,
            }
        };

        let existing = self.trigger_registry.get_trigger(table_name, name)?;
        let existing_exec = self.trigger_registry.exec_info(table_name, name);
        if existing.is_some() {
            if target.if_not_exists {
                return Ok(0);
            }
            if !target.or_replace {
                return Err(Error::query_execution(format!(
                    "Trigger '{}' already exists on table '{}'",
                    name, table_name
                )));
            }
        }

        let definition = TriggerDefinition {
            name: name.to_string(),
            table_name: table_name.to_string(),
            timing: timing.clone(),
            events: events.to_vec(),
            for_each: for_each.clone(),
            when_condition: when_condition.clone(),
            body: Vec::new(),
            enabled: true,
            created_at: Self::routine_created_at_millis(),
            referencing: Vec::new(),
            characteristics: characteristics.clone(),
            trigger_type: trigger_type.clone(),
            from_constraint: from_constraint.clone(),
        };

        let registry = &self.trigger_registry;
        let mut ddl = storage::DdlBatch::new(&self.storage);
        if existing.is_some() {
            // OR REPLACE: the old trigger (and a rewrite recipe an older build
            // left for it) goes in the same write group the new one arrives in.
            storage::Catalog::stage_trigger_drop(table_name, name, &mut ddl);
            registry.drop_trigger(table_name, name)?;
            registry.drop_row_mutation(table_name, name);
        }
        registry.register_trigger(definition.clone())?;
        {
            let table = table_name.to_string();
            let trigger = name.to_string();
            ddl.on_abort(move || {
                let _ = registry.drop_trigger(&table, &trigger);
                if let Some(old) = existing {
                    let _ = registry.register_trigger(old);
                    if let Some(exec) = existing_exec {
                        registry.register_exec(&table, &trigger, (*exec).clone());
                    }
                }
            });
        }
        self.storage.catalog().stage_trigger(&definition, None, &mut ddl)?;
        storage::Catalog::stage_trigger_exec(table_name, name, &info, &mut ddl)?;
        {
            let table = table_name.to_string();
            let trigger = name.to_string();
            ddl.on_commit(move || registry.register_exec(&table, &trigger, info));
        }
        ddl.commit()?.wait_replicated();
        Ok(0)
    }

    /// The table `DROP TRIGGER name` (no `ON`) means: `Ok(None)` when no
    /// table has the trigger and IF EXISTS was given.
    pub(crate) fn trigger_drop_table(
        &self,
        name: &str,
        table: &Option<String>,
        if_exists: bool,
    ) -> Result<Option<Option<String>>> {
        match table {
            Some(given) if given == ANY_TABLE || given.ends_with(&format!(".{ANY_TABLE}")) => {
                let mut tables: Vec<String> = self
                    .trigger_registry
                    .list_all_triggers()?
                    .into_iter()
                    .filter(|t| t.name == name)
                    .map(|t| t.table_name)
                    .collect();
                tables.sort();
                match (tables.len(), tables.pop()) {
                    (1, Some(table)) => Ok(Some(Some(table))),
                    (0, _) if if_exists => Ok(None),
                    (0, _) => Err(Error::query_execution(format!("Trigger '{name}' does not exist"))),
                    _ => Err(Error::query_execution(format!(
                        "trigger name \"{name}\" is used on more than one table; name the table: DROP TRIGGER {name} \
                         ON <table>"
                    ))),
                }
            }
            other => Ok(Some(other.clone())),
        }
    }

    /// The triggers that run `function`, as `(table, trigger)`.
    fn triggers_running(&self, function: &str) -> Vec<(String, String)> {
        let function = function.to_lowercase();
        let mut found: Vec<(String, String)> = self
            .trigger_registry
            .list_exec()
            .into_iter()
            .filter(|(_, _, info)| info.function.as_deref() == Some(function.as_str()))
            .map(|(table, trigger, _)| (table, trigger))
            .collect();
        found.sort();
        found
    }

    /// `DROP FUNCTION`: a function a trigger runs is not dropped (2BP01)
    /// unless CASCADE was given, which drops those triggers first. Returns
    /// the function's name as written.
    pub(crate) fn admit_drop_function(&self, planned_name: &str) -> Result<String> {
        let (name, cascade) = decode_drop_function(planned_name);
        if !self.trigger_registry.has_exec_triggers() {
            return Ok(name);
        }
        let dependents = self.triggers_running(&name);
        if dependents.is_empty() {
            return Ok(name);
        }
        if !cascade {
            let list: Vec<String> = dependents
                .iter()
                .map(|(table, trigger)| format!("trigger \"{trigger}\" on table \"{table}\""))
                .collect();
            return Err(Error::query_execution(format!(
                "function {name}() {TRIGGER_DEPENDS_MARKER}: {} runs it. Drop the trigger first, or use DROP \
                 FUNCTION {name} CASCADE to drop it too",
                list.join(", ")
            )));
        }
        for (table, trigger) in dependents {
            self.execute_drop_trigger_plan(&trigger, &Some(table), true)?;
        }
        Ok(name)
    }

    /// `ALTER TABLE … ENABLE | DISABLE TRIGGER` and `ALTER TRIGGER … RENAME`.
    pub(crate) fn execute_trigger_admin(&self, args: &[crate::sql::LogicalExpr]) -> Result<u64> {
        let text = |index: usize| -> String {
            match args.get(index) {
                Some(crate::sql::LogicalExpr::Literal(Value::String(s))) => s.clone(),
                _ => String::new(),
            }
        };
        let (action, given_table, name, to) = (text(0), text(1), text(2), text(3));
        let catalog = self.storage.catalog();
        let bare = given_table.strip_prefix("public.").unwrap_or(&given_table).to_string();
        let table = [given_table.clone(), bare]
            .into_iter()
            .find(|candidate| catalog.get_table_schema(candidate).is_ok())
            .ok_or_else(|| Error::query_execution(format!("relation \"{given_table}\" does not exist")))?;
        let registry = &self.trigger_registry;
        let missing =
            |name: &str| Error::query_execution(format!("trigger \"{name}\" for table \"{table}\" does not exist"));
        match action.as_str() {
            "enable" | "disable" => {
                let enabled = action == "enable";
                let targets: Vec<TriggerDefinition> = if name == "*" {
                    registry.get_triggers_for_table(&table)?
                } else {
                    vec![registry.get_trigger(&table, &name)?.ok_or_else(|| missing(&name))?]
                };
                let mut ddl = storage::DdlBatch::new(&self.storage);
                let mut changed = Vec::new();
                for mut definition in targets {
                    if definition.enabled == enabled {
                        continue;
                    }
                    definition.enabled = enabled;
                    catalog.stage_trigger(&definition, None, &mut ddl)?;
                    changed.push(definition);
                }
                if changed.is_empty() {
                    return Ok(0);
                }
                ddl.on_commit(move || {
                    for definition in changed {
                        let _ = registry.replace_trigger(definition);
                    }
                });
                ddl.commit()?.wait_replicated();
                Ok(0)
            }
            "rename" => {
                let mut definition = registry.get_trigger(&table, &name)?.ok_or_else(|| missing(&name))?;
                if registry.get_trigger(&table, &to)?.is_some() {
                    return Err(Error::query_execution(format!(
                        "Trigger '{}' already exists on table '{}'",
                        to, table
                    )));
                }
                let exec = registry.exec_info(&table, &name);
                let recipe = catalog.load_trigger_row_mutation(&table, &name)?;
                definition.name = to.clone();
                let mut ddl = storage::DdlBatch::new(&self.storage);
                storage::Catalog::stage_trigger_drop(&table, &name, &mut ddl);
                catalog.stage_trigger(&definition, recipe.as_ref(), &mut ddl)?;
                if let Some(exec) = &exec {
                    storage::Catalog::stage_trigger_exec(&table, &to, exec, &mut ddl)?;
                }
                let table_for_commit = table.clone();
                ddl.on_commit(move || {
                    let _ = registry.drop_trigger(&table_for_commit, &name);
                    registry.drop_row_mutation(&table_for_commit, &name);
                    let _ = registry.register_trigger(definition);
                    if let Some(exec) = exec {
                        registry.register_exec(&table_for_commit, &to, (*exec).clone());
                    }
                    if let Some(recipe) = recipe {
                        registry.register_row_mutation(&table_for_commit, &to, recipe);
                    }
                });
                ddl.commit()?.wait_replicated();
                Ok(0)
            }
            other => Err(Error::internal(format!(
                "unknown trigger administration action '{other}'"
            ))),
        }
    }

    // ------------------------------------------------------------- loading

    /// Load what each trigger runs, after the definitions are registered, and
    /// name the triggers an older build created without that record.
    pub(crate) fn load_trigger_exec_records(&self) {
        let registry = &self.trigger_registry;
        match self.storage.catalog().load_all_trigger_exec() {
            Ok(records) => {
                for (table, trigger, info) in records {
                    if matches!(registry.get_trigger(&table, &trigger), Ok(Some(_))) {
                        registry.register_exec(&table, &trigger, info);
                    }
                }
            }
            Err(e) => tracing::warn!("failed to restore what the triggers run: {}", e),
        }
        let legacy = self.legacy_triggers();
        if !legacy.is_empty() {
            let list: Vec<String> = legacy
                .iter()
                .map(|(table, trigger, recipe)| {
                    format!(
                        "{trigger} on {table} ({})",
                        if *recipe {
                            "keeps its BEFORE INSERT row rewrite"
                        } else {
                            "does not fire"
                        }
                    )
                })
                .collect();
            tracing::warn!(
                "{} trigger(s) were created by an older build that did not record the function they run: {}. They \
                 behave exactly as before the upgrade. Re-create each one (CREATE OR REPLACE TRIGGER … EXECUTE \
                 FUNCTION …) to make it run its function",
                legacy.len(),
                list.join("; ")
            );
        }
    }

    /// Triggers without a record of what they run, as `(table, trigger, has a
    /// rewrite recipe)`.
    pub(crate) fn legacy_triggers(&self) -> Vec<(String, String, bool)> {
        let registry = &self.trigger_registry;
        let mut out: Vec<(String, String, bool)> = registry
            .list_all_triggers()
            .unwrap_or_default()
            .into_iter()
            .filter(|t| registry.exec_info(&t.table_name, &t.name).is_none())
            .map(|t| {
                let recipe = self
                    .storage
                    .catalog()
                    .load_trigger_row_mutation(&t.table_name, &t.name)
                    .ok()
                    .flatten()
                    .is_some();
                (t.table_name, t.name, recipe)
            })
            .collect();
        out.sort();
        out
    }

    /// Reload what one trigger runs from the store (a physical standby after
    /// its applier changed the record).
    #[cfg_attr(not(feature = "ha-tier1"), allow(dead_code))]
    pub(crate) fn reload_trigger_exec(&self, table: &str, name: &str) {
        let registry = &self.trigger_registry;
        if !matches!(registry.get_trigger(table, name), Ok(Some(_))) {
            registry.drop_exec(table, name);
            return;
        }
        match self.storage.catalog().load_trigger_exec(table, name) {
            Ok(Some(info)) => registry.register_exec(table, name, info),
            Ok(None) => registry.drop_exec(table, name),
            Err(e) => {
                tracing::warn!(
                    "physical standby: what trigger '{}.{}' runs could not be reloaded: {}",
                    table,
                    name,
                    e
                );
                registry.drop_exec(table, name);
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]
mod tests {
    use super::*;

    fn copy_rows(specs: &[&[&str]]) -> Vec<Vec<Option<String>>> {
        specs
            .iter()
            .map(|row| row.iter().map(|field| Some((*field).to_string())).collect())
            .collect()
    }

    /// The performance guard: a table without a trigger keeps its fast paths
    /// and asks nothing of the trigger machinery, also while another table of
    /// the same database has a trigger.
    #[test]
    fn a_table_without_a_trigger_keeps_its_fast_path() {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        db.execute("CREATE TABLE plain (id INT PRIMARY KEY, note TEXT)")
            .unwrap();
        db.execute("CREATE TABLE watched (id INT PRIMARY KEY, note TEXT)")
            .unwrap();
        let schema = db.storage.catalog().get_table_schema("plain").unwrap();

        // No trigger anywhere: one atomic load answers, and the bulk path is taken.
        assert!(!db.trigger_registry.has_exec_triggers());
        assert!(!db.table_fires_triggers("plain"));
        let rows = copy_rows(&[&["1", "a"], &["2", "b"]]);
        assert_eq!(db.copy_bulk_insert("plain", &[], &rows).expect("fast path").unwrap(), 2);

        db.execute("CREATE FUNCTION stamp() RETURNS TRIGGER AS $$ BEGIN NEW.note := 'seen'; RETURN NEW; END $$ LANGUAGE plpgsql")
            .unwrap();
        db.execute("CREATE TRIGGER watched_stamp BEFORE INSERT ON watched FOR EACH ROW EXECUTE FUNCTION stamp()")
            .unwrap();

        // The other table has a trigger now; this one still has none.
        assert!(db.trigger_registry.has_exec_triggers());
        assert!(!db.table_fires_triggers("plain"));
        assert!(db.table_fires_triggers("watched"));
        let gate = db
            .with_trigger_statement(None, || {
                db.trigger_statement("plain", TriggerOp::Insert, &[], &schema, None)
            })
            .unwrap();
        assert!(
            gate.is_none(),
            "no gate, so no per-row work, for a table without a trigger"
        );
        let rows = copy_rows(&[&["3", "c"]]);
        assert_eq!(
            db.copy_bulk_insert("plain", &[], &rows)
                .expect("still the fast path")
                .unwrap(),
            1
        );

        // The table with a trigger leaves the bulk path, which cannot fire it:
        // the caller then writes the rows as INSERT statements, which do.
        let rows = copy_rows(&[&["1", "a"]]);
        assert!(db.copy_bulk_insert("watched", &[], &rows).is_none());
        db.execute("INSERT INTO watched VALUES (1, 'a')").unwrap();
        let stored = db.query("SELECT note FROM watched", &[]).unwrap();
        assert_eq!(stored[0].values[0], Value::String("seen".to_string()));

        // Disabled, the trigger costs the table nothing again.
        db.execute("ALTER TABLE watched DISABLE TRIGGER ALL").unwrap();
        assert!(!db.table_fires_triggers("watched"));
    }

    /// A write route with no statement around it does not write past a trigger.
    #[test]
    fn a_route_without_a_statement_is_refused_for_a_table_with_a_trigger() {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        db.execute("CREATE TABLE watched (id INT PRIMARY KEY, note TEXT)")
            .unwrap();
        db.execute("CREATE FUNCTION stamp() RETURNS TRIGGER AS $$ BEGIN RETURN NEW; END $$ LANGUAGE plpgsql")
            .unwrap();
        db.execute("CREATE TRIGGER watched_stamp AFTER INSERT ON watched FOR EACH ROW EXECUTE FUNCTION stamp()")
            .unwrap();
        let schema = db.storage.catalog().get_table_schema("watched").unwrap();
        let err = db
            .trigger_statement("watched", TriggerOp::Insert, &[], &schema, None)
            .err()
            .expect("no frame")
            .to_string();
        assert!(err.contains(TRIGGER_ROUTE_MARKER), "{err}");
        assert_eq!(trigger_exec::sqlstate_for_message(&err), Some("0A000"));
        assert!(db
            .refuse_route_past_triggers("watched", TriggerOp::Insert, "a test route")
            .is_err());
        assert!(db
            .refuse_route_past_triggers("watched", TriggerOp::Delete, "a test route")
            .is_ok());
    }

    /// A trigger as a build before the function record created it: a
    /// definition, and for the BEFORE INSERT row form a rewrite recipe.
    fn create_legacy_trigger(db: &EmbeddedDatabase, name: &str, timing: TriggerTiming, function: &str) {
        db.execute_create_trigger_plan(
            name,
            "old_t",
            &timing,
            &[TriggerEvent::Insert],
            &TriggerFor::Row,
            &None,
            &[],
            false,
            &[],
            &crate::sql::TriggerCharacteristics::default(),
            &TriggerType::default(),
            &None,
            &Some(function.to_string()),
        )
        .unwrap();
    }

    fn notes(db: &EmbeddedDatabase) -> Vec<Value> {
        db.query("SELECT note FROM old_t ORDER BY id", &[])
            .unwrap()
            .into_iter()
            .filter_map(|row| row.values.into_iter().next())
            .collect()
    }

    /// Triggers created before the upgrade behave exactly as before it, also
    /// after a restart, are listed, and run their function once re-created.
    #[test]
    fn triggers_from_an_older_build_keep_their_behaviour_until_recreated() {
        let dir = std::env::temp_dir().join(format!(
            "nano_trigger_legacy_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        {
            let db = EmbeddedDatabase::new(&dir).unwrap();
            db.execute("CREATE TABLE old_t (id INT PRIMARY KEY, note TEXT)")
                .unwrap();
            db.execute("CREATE TABLE old_log (line TEXT)").unwrap();
            db.execute("CREATE FUNCTION old_stamp() RETURNS TRIGGER AS $$ BEGIN NEW.note = 'recipe'; RETURN NEW; END $$ LANGUAGE plpgsql")
                .unwrap();
            db.execute("CREATE FUNCTION old_audit() RETURNS TRIGGER AS $$ BEGIN INSERT INTO old_log VALUES ('fired'); RETURN NEW; END $$ LANGUAGE plpgsql")
                .unwrap();
            create_legacy_trigger(&db, "old_before", TriggerTiming::Before, "old_stamp");
            create_legacy_trigger(&db, "old_after", TriggerTiming::After, "old_audit");
            assert_eq!(
                db.legacy_triggers(),
                vec![
                    ("old_t".to_string(), "old_after".to_string(), false),
                    ("old_t".to_string(), "old_before".to_string(), true),
                ]
            );
        }
        let db = EmbeddedDatabase::new(&dir).unwrap();
        assert_eq!(db.legacy_triggers().len(), 2, "still listed after the restart");
        db.execute("INSERT INTO old_t VALUES (1, 'x')").unwrap();
        db.execute_params("INSERT INTO old_t VALUES (2, 'x')", &[]).unwrap();
        assert_eq!(
            notes(&db),
            vec![Value::String("recipe".into()), Value::String("recipe".into())],
            "the row rewrite still applies on both families"
        );
        assert!(
            db.query("SELECT * FROM old_log", &[]).unwrap().is_empty(),
            "the AFTER trigger did not fire before the upgrade and does not fire now"
        );
        // Replacing the function does not change a trigger that never recorded it.
        db.execute("CREATE OR REPLACE FUNCTION old_stamp() RETURNS TRIGGER AS $$ BEGIN NEW.note = 'function'; RETURN NEW; END $$ LANGUAGE plpgsql")
            .unwrap();
        db.execute("INSERT INTO old_t VALUES (3, 'x')").unwrap();
        assert_eq!(notes(&db).last(), Some(&Value::String("recipe".into())));

        // Re-created, each one runs its function.
        db.execute(
            "CREATE OR REPLACE TRIGGER old_before BEFORE INSERT ON old_t FOR EACH ROW EXECUTE FUNCTION old_stamp()",
        )
        .unwrap();
        db.execute(
            "CREATE OR REPLACE TRIGGER old_after AFTER INSERT ON old_t FOR EACH ROW EXECUTE FUNCTION old_audit()",
        )
        .unwrap();
        assert!(db.legacy_triggers().is_empty());
        db.execute("INSERT INTO old_t VALUES (4, 'x')").unwrap();
        assert_eq!(notes(&db).last(), Some(&Value::String("function".into())));
        assert_eq!(db.query("SELECT * FROM old_log", &[]).unwrap().len(), 1);
        drop(db);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn update_of_fires_only_for_its_columns() {
        let definition = TriggerDefinition::new(
            "g".to_string(),
            "t".to_string(),
            TriggerTiming::Before,
            vec![TriggerEvent::Update(Some(vec!["a".to_string()])), TriggerEvent::Delete],
            TriggerFor::Row,
            None,
            vec![],
        );
        assert!(fires(&definition, TriggerOp::Update, &["A".to_string()]));
        assert!(!fires(&definition, TriggerOp::Update, &["b".to_string()]));
        assert!(fires(&definition, TriggerOp::Delete, &[]));
        assert!(!fires(&definition, TriggerOp::Insert, &[]));
    }
}
