//! Session state of a MySQL connection: `SET`, system and user variables,
//! `autocommit`, transaction characteristics, `SHOW VARIABLES` / `SHOW STATUS`.
//!
//! The store and the statement readers are `super::super::vars`; this file is
//! what a connection does with them.

use super::*;
use crate::protocol::mysql::accounts;
use crate::protocol::mysql::vars::{
    self, find_var_refs, normalize, parse_set, parse_show_filter, quote_alias, select_value, sql_literal, Assignment,
    RawValue, RefKind, Scope, ShowFilter, VarDef, VarError, VarRef,
};
use crate::session::IsolationLevel;

/// The session GUC that carries `foreign_key_checks = 0` to the engine.
pub(crate) const FOREIGN_KEY_CHECKS_GUC: &str = "mysql.foreign_key_checks";

/// A transaction control statement of the MySQL wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum TxnStatement {
    Begin { read_only: Option<bool> },
    Commit { chain: bool },
    Rollback { chain: bool },
}

/// Read a transaction control statement from a statement's code-only view.
/// `None`: something else (`ROLLBACK TO SAVEPOINT` included, which the engine
/// runs).
pub(super) fn classify_txn(view: &str) -> Option<std::result::Result<TxnStatement, VarError>> {
    let text = view.trim().trim_end_matches(';').trim();
    let words: Vec<&str> = text.split_whitespace().collect();
    let (first, rest) = words.split_first()?;
    match *first {
        "BEGIN" => match rest {
            [] | ["WORK"] => Some(Ok(TxnStatement::Begin { read_only: None })),
            _ => Some(Err(VarError::syntax(text))),
        },
        "START" => {
            let ["TRANSACTION", options @ ..] = rest else {
                return None;
            };
            let joined = options.join(" ");
            let mut read_only = None;
            for option in joined.split(',').map(str::trim).filter(|o| !o.is_empty()) {
                match option {
                    "WITH CONSISTENT SNAPSHOT" => {}
                    "READ ONLY" => read_only = Some(true),
                    "READ WRITE" => read_only = Some(false),
                    _ => return Some(Err(VarError::syntax(text))),
                }
            }
            Some(Ok(TxnStatement::Begin { read_only }))
        }
        "COMMIT" | "ROLLBACK" => {
            let rest = match rest {
                ["WORK", tail @ ..] => tail,
                other => other,
            };
            if *first == "ROLLBACK" && rest.first() == Some(&"TO") {
                return None;
            }
            let (chain, rest) = match rest {
                ["AND", "CHAIN", tail @ ..] => (true, tail),
                ["AND", "NO", "CHAIN", tail @ ..] => (false, tail),
                other => (false, other),
            };
            match rest {
                [] | ["NO", "RELEASE"] => {}
                ["RELEASE"] => return Some(Err(VarError::not_supported(&format!("{first} RELEASE")))),
                _ => return Some(Err(VarError::syntax(text))),
            }
            Some(Ok(if *first == "COMMIT" {
                TxnStatement::Commit { chain }
            } else {
                TxnStatement::Rollback { chain }
            }))
        }
        _ => None,
    }
}

/// Is this a `SET` that assigns something the listener itself keeps: one of
/// its system variables, a user variable, `NAMES`, transaction
/// characteristics, a password? (A `SET` of engine parameters only is not.)
pub(super) fn set_names_listener_variable(raw: &str) -> bool {
    let Some(body) = vars::strip_keyword(raw.trim(), "SET") else {
        return false;
    };
    parse_set(body).is_ok_and(|assignments| {
        assignments.iter().any(|assignment| match assignment {
            Assignment::System { name, .. } => vars::find(name).is_some(),
            _ => true,
        })
    })
}

/// Does the code of a statement hold `word` as a whole word?
pub(super) fn view_has_word(view: &str, word: &str) -> bool {
    view.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .any(|w| w == word)
}

/// The first keyword of a statement's code.
fn first_word(view: &str) -> &str {
    view.trim_start()
        .split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .next()
        .unwrap_or("")
}

/// May this statement run in a `READ ONLY` transaction?
fn allowed_when_read_only(view: &str) -> bool {
    match first_word(view) {
        "SELECT" | "TABLE" | "VALUES" | "SHOW" | "DESCRIBE" | "DESC" | "EXPLAIN" | "SET" | "USE" | "SAVEPOINT"
        | "RELEASE" | "ROLLBACK" | "COMMIT" => !view_has_word(view, "INTO"),
        "WITH" => !["INSERT", "UPDATE", "DELETE", "MERGE"]
            .iter()
            .any(|word| view_has_word(view, word)),
        "" => true,
        _ => false,
    }
}

fn isolation_level(name: &str) -> IsolationLevel {
    match name {
        "REPEATABLE-READ" => IsolationLevel::RepeatableRead,
        "SERIALIZABLE" => IsolationLevel::Serializable,
        _ => IsolationLevel::ReadCommitted,
    }
}

fn db_error(error: &crate::Error) -> VarError {
    let text = error.to_string();
    let (code, state) = map_error_code(&text);
    VarError {
        code,
        state,
        message: text,
    }
}

impl<S: AsyncRead + AsyncWrite + Unpin + Send> MySqlHandler<S> {
    pub(super) async fn send_var_error(&mut self, error: &VarError) -> Result<()> {
        self.send_error(error.code, error.state, &error.message).await
    }

    fn scoped_state(&self) -> Option<Arc<crate::session::scoped::SessionScopedState>> {
        self.database
            .session_manager
            .get_session(self.session_id)
            .ok()
            .map(|session| Arc::clone(&session.read().scoped))
    }

    // ------------------------------------------------------------------
    // Transactions
    // ------------------------------------------------------------------

    /// The handler's record that the engine transaction is over.
    pub(super) fn note_transaction_ended(&mut self) {
        self.in_transaction = false;
        self.txn_read_only = false;
        self.status_flags.clear(StatusFlags::SERVER_STATUS_IN_TRANS);
    }

    /// Open an engine transaction with this session's characteristics.
    pub(super) fn begin_engine_transaction(&mut self, read_only: Option<bool>) -> std::result::Result<(), crate::Error> {
        let session_level = self
            .vars
            .get_named("transaction_isolation")
            .unwrap_or_else(|| "READ-COMMITTED".to_string());
        let level = self.next_txn_isolation.take().unwrap_or(session_level);
        let next_read_only = self.next_txn_read_only.take();
        if level != self.applied_isolation {
            self.database
                .set_session_isolation(self.session_id, isolation_level(&level))?;
            self.applied_isolation = level;
        }
        self.database.begin_transaction_for_session(self.session_id)?;
        self.in_transaction = true;
        self.status_flags.set(StatusFlags::SERVER_STATUS_IN_TRANS);
        self.txn_read_only = read_only
            .or(next_read_only)
            .unwrap_or_else(|| self.vars.is_on("transaction_read_only"));
        Ok(())
    }

    /// `autocommit = 0`: the first statement that reads or writes a table
    /// opens the transaction it and its successors run in.
    pub(super) fn ensure_implicit_transaction(&mut self) -> std::result::Result<(), crate::Error> {
        if self.in_transaction || self.vars.is_on("autocommit") {
            return Ok(());
        }
        self.begin_engine_transaction(None)
    }

    /// Commit the open transaction because the next statement implies it.
    pub(super) fn implicit_commit(&mut self) -> std::result::Result<(), crate::Error> {
        if !self.in_transaction {
            return Ok(());
        }
        let outcome = self.database.commit_transaction_for_session(self.session_id);
        if outcome.is_err() {
            self.sync_transaction_state_after_error();
        } else {
            self.note_transaction_ended();
        }
        outcome
    }

    pub(super) async fn handle_txn_statement(&mut self, statement: TxnStatement) -> Result<()> {
        match statement {
            TxnStatement::Begin { read_only } => {
                // MySQL: a BEGIN inside a transaction commits it first.
                self.implicit_commit()?;
                self.begin_engine_transaction(read_only)?;
            }
            TxnStatement::Commit { chain } | TxnStatement::Rollback { chain } => {
                let was_open = self.in_transaction;
                if was_open {
                    let outcome = if matches!(statement, TxnStatement::Commit { .. }) {
                        self.database.commit_transaction_for_session(self.session_id)
                    } else {
                        self.database.rollback_transaction_for_session(self.session_id)
                    };
                    if let Err(e) = outcome {
                        self.sync_transaction_state_after_error();
                        return Err(e.into());
                    }
                    self.note_transaction_ended();
                }
                if chain && was_open {
                    self.begin_engine_transaction(None)?;
                }
            }
        }
        self.send_ok(0, 0).await
    }

    /// What stops this statement before it runs: a write in a `READ ONLY`
    /// transaction, or an unkeyed `UPDATE` / `DELETE` under `sql_safe_updates`.
    pub(super) fn statement_refusal(&self, view: &str) -> Option<VarError> {
        // Outside a transaction the statement is its own (autocommit) or opens
        // one (autocommit off): either way under the characteristics in force.
        let read_only = if self.in_transaction {
            self.txn_read_only
        } else {
            self.next_txn_read_only
                .unwrap_or_else(|| self.vars.is_on("transaction_read_only"))
        };
        if read_only && !allowed_when_read_only(view) {
            return Some(VarError {
                code: 1792,
                state: "25006",
                message: "Cannot execute statement in a READ ONLY transaction.".to_string(),
            });
        }
        if self.vars.is_on("sql_safe_updates")
            && matches!(first_word(view), "UPDATE" | "DELETE")
            && !view_has_word(view, "WHERE")
            && !view_has_word(view, "LIMIT")
        {
            return Some(VarError {
                code: 1175,
                state: "HY000",
                message: "You are using safe update mode and you tried to update a table without a WHERE that \
                          uses a KEY column."
                    .to_string(),
            });
        }
        None
    }

    /// `sql_select_limit` for a SELECT that has no LIMIT of its own.
    pub(super) fn select_row_limit(&self, view: &str) -> Option<usize> {
        if !self.vars.is_set("sql_select_limit") || view_has_word(view, "LIMIT") {
            return None;
        }
        let limit: u64 = self.vars.get_named("sql_select_limit")?.parse().ok()?;
        usize::try_from(limit).ok()
    }

    // ------------------------------------------------------------------
    // Reading variables
    // ------------------------------------------------------------------

    /// The text `SHOW VARIABLES` prints for a known variable.
    fn system_text(&self, def: &VarDef, global: bool) -> String {
        match def.name {
            "version" => SERVER_VERSION.to_string(),
            "have_ssl" | "have_openssl" => self.have_ssl_value().to_string(),
            "ssl_kx_group" => self.ssl_kx_group_value(),
            "last_insert_id" | "identity" => self.last_insert_id.to_string(),
            "pseudo_thread_id" => self.connection_id.to_string(),
            "wait_timeout" => self.session_timeout_secs().to_string(),
            // The statement timeout in force, whichever name set it.
            "max_execution_time" if !global => self
                .scoped_state()
                .and_then(|state| state.statement_timeout_ms())
                .unwrap_or(0)
                .to_string(),
            "interactive_timeout" | "net_read_timeout" | "net_write_timeout" if !self.vars.is_set(def.name) => {
                self.session_timeout_secs().to_string()
            }
            _ if global => def.default.to_string(),
            _ => self.vars.get(def),
        }
    }

    /// This session's lock wait limit in seconds (`innodb_lock_wait_timeout`).
    fn lock_wait_seconds(&self) -> i64 {
        let ms = self
            .scoped_state()
            .and_then(|state| state.lock_timeout_ms())
            .filter(|ms| *ms > 0)
            .unwrap_or(self.database.storage.config().session.lock_timeout_ms);
        i64::try_from(ms.div_ceil(1000)).unwrap_or(i64::MAX).max(1)
    }

    /// The value of an engine parameter for this session, as `SHOW` prints it.
    fn engine_parameter(&self, name: &str) -> Option<String> {
        if crate::sql::params::find(name).is_none() && !crate::sql::is_registered(name) {
            return None;
        }
        let (rows, _) = self
            .database
            .query_with_columns_for_session(self.session_id, &format!("SHOW {name}"))
            .ok()?;
        rows.first()
            .and_then(|row| row.values.first())
            .map(value_to_mysql_string)
    }

    /// The value of `@@name` as a result cell.
    fn system_value(&self, name: &str, global: bool) -> std::result::Result<Value, VarError> {
        if let Some(def) = vars::find(name) {
            return Ok(select_value(def, &self.system_text(def, global)));
        }
        if matches!(name, "innodb_lock_wait_timeout" | "lock_wait_timeout") {
            return Ok(Value::Int8(self.lock_wait_seconds()));
        }
        self.engine_parameter(name)
            .map(Value::String)
            .ok_or_else(|| VarError::unknown(name))
    }

    fn ref_value(&self, var: &VarRef) -> std::result::Result<Value, VarError> {
        match &var.kind {
            RefKind::System { global, name } => self.system_value(name, *global),
            RefKind::User { name } => Ok(self.vars.user(name)),
        }
    }

    /// Evaluate an expression that may name variables: one variable is read
    /// directly, anything else is computed by the engine.
    fn eval_expression(&mut self, expr: &str) -> std::result::Result<Value, VarError> {
        let expr = expr.trim();
        let view = super::super::translator::code_view(expr);
        let refs = find_var_refs(expr, &view);
        if let [only] = refs.as_slice() {
            if only.start == 0 && only.end == expr.len() {
                return self.ref_value(only);
            }
        }
        let mut text = String::with_capacity(expr.len());
        let mut cursor = 0usize;
        for var in &refs {
            text.push_str(expr.get(cursor..var.start).unwrap_or(""));
            text.push_str(&sql_literal(&self.ref_value(var)?));
            cursor = var.end;
        }
        text.push_str(expr.get(cursor..).unwrap_or(""));
        let translated = super::super::translator::translate(&format!("SELECT {text}"));
        let (rows, _) = self
            .database
            .query_with_columns_for_session(self.session_id, &translated)
            .map_err(|e| db_error(&e))?;
        Ok(rows
            .into_iter()
            .next()
            .and_then(|row| row.values.into_iter().next())
            .unwrap_or(Value::Null))
    }

    fn resolve_set_value(&mut self, expr: &str) -> std::result::Result<RawValue, VarError> {
        let text = expr.trim();
        if text.eq_ignore_ascii_case("DEFAULT") {
            return Ok(RawValue::Default);
        }
        if text.eq_ignore_ascii_case("NULL") {
            return Ok(RawValue::Null);
        }
        if vars::is_single_literal(text) {
            return Ok(RawValue::Text(vars::unquote_word(text)));
        }
        if text
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '+' | ':' | '-' | '/'))
        {
            return Ok(RawValue::Text(text.to_string()));
        }
        Ok(match self.eval_expression(text)? {
            Value::Null => RawValue::Null,
            Value::Boolean(b) => RawValue::Text(if b { "1" } else { "0" }.to_string()),
            value => RawValue::Text(value_to_mysql_string(&value)),
        })
    }

    fn eval_user_value(&mut self, expr: &str) -> std::result::Result<Value, VarError> {
        let text = expr.trim();
        if text.eq_ignore_ascii_case("NULL") {
            return Ok(Value::Null);
        }
        if vars::is_single_literal(text) {
            return Ok(Value::String(vars::unquote_word(text)));
        }
        if let Ok(number) = text.parse::<i64>() {
            return Ok(Value::Int8(number));
        }
        self.eval_expression(text)
    }

    // ------------------------------------------------------------------
    // SET
    // ------------------------------------------------------------------

    /// `SET ...`, from the statement as the client wrote it.
    pub(super) async fn handle_set(&mut self, raw: &str) -> Result<()> {
        let body = vars::strip_keyword(raw.trim(), "SET").unwrap_or("");
        let assignments = match parse_set(body) {
            Ok(assignments) => assignments,
            Err(e) => return self.send_var_error(&e).await,
        };
        for assignment in assignments {
            if let Err(e) = self.apply_assignment(assignment) {
                return self.send_var_error(&e).await;
            }
        }
        self.send_ok(0, 0).await
    }

    fn apply_assignment(&mut self, assignment: Assignment) -> std::result::Result<(), VarError> {
        match assignment {
            Assignment::System {
                scope: Scope::Global, ..
            }
            | Assignment::Transaction {
                scope: Scope::Global, ..
            } => Err(VarError::not_supported(
                "SET GLOBAL (set the variable for the session, or the server's configuration)",
            )),
            Assignment::System { name, expr, .. } => self.set_system(&name, &expr),
            Assignment::User { name, expr } => {
                let value = self.eval_user_value(&expr)?;
                self.vars.set_user(&name, value);
                Ok(())
            }
            Assignment::Names { charset, collation } => {
                let charset = charset.unwrap_or_else(|| "utf8mb4".to_string());
                let stored = self.store_checked("character_set_client", &charset)?;
                self.store_checked("character_set_connection", &stored)?;
                self.store_checked("character_set_results", &stored)?;
                let collation = collation.unwrap_or_else(|| format!("{stored}_general_ci"));
                self.store_checked("collation_connection", &collation)?;
                Ok(())
            }
            Assignment::CharacterSet { charset } => {
                let charset = charset.unwrap_or_else(|| "utf8mb4".to_string());
                let stored = self.store_checked("character_set_client", &charset)?;
                self.store_checked("character_set_results", &stored)?;
                let database_charset = self
                    .vars
                    .get_named("character_set_database")
                    .unwrap_or_else(|| "utf8mb4".to_string());
                self.store_checked("character_set_connection", &database_charset)?;
                Ok(())
            }
            Assignment::Transaction {
                session,
                isolation,
                read_only,
                ..
            } => {
                if session {
                    if let Some(level) = isolation {
                        self.set_system("transaction_isolation", level)?;
                    }
                    if let Some(read_only) = read_only {
                        self.set_system("transaction_read_only", if read_only { "ON" } else { "OFF" })?;
                    }
                    return Ok(());
                }
                if self.in_transaction {
                    return Err(VarError {
                        code: 1568,
                        state: "25001",
                        message: "Transaction characteristics can't be changed while a transaction is in progress"
                            .to_string(),
                    });
                }
                if let Some(level) = isolation {
                    // Dirty reads do not exist here: the nearest level is used.
                    let level = if level == "READ-UNCOMMITTED" { "READ-COMMITTED" } else { level };
                    self.next_txn_isolation = Some(level.to_string());
                }
                if read_only.is_some() {
                    self.next_txn_read_only = read_only;
                }
                Ok(())
            }
            Assignment::Password { account, expr } => {
                let statement = accounts::set_password_statement(account.as_deref(), &expr)?;
                self.run_account_statement(statement)
            }
        }
    }

    fn store_checked(&mut self, name: &str, value: &str) -> std::result::Result<String, VarError> {
        let def = vars::find(name).ok_or_else(|| VarError::unknown(name))?;
        let stored = normalize(def, &RawValue::Text(value.to_string()))?;
        self.vars.store(def, stored.clone());
        Ok(stored)
    }

    fn engine_set(&self, statement: &str, name: &str, value: &str) -> std::result::Result<(), VarError> {
        self.database
            .try_handle_session_guc(self.session_id, statement)
            .map(|_| ())
            .map_err(|e| VarError::wrong_value_because(name, value, &e.to_string()))
    }

    fn set_system(&mut self, name: &str, expr: &str) -> std::result::Result<(), VarError> {
        let name = name.to_ascii_lowercase();
        let Some(def) = vars::find(&name) else {
            // A parameter of the engine: the session-settable ones are set on
            // this session, the server-level ones cannot be set from here.
            let statement = format!("SET {name} = {expr}");
            if EmbeddedDatabase::is_application_name_statement(&statement) {
                return self
                    .database
                    .try_handle_session_application_name(self.session_id, &statement)
                    .map(|_| ())
                    .map_err(|e| VarError::wrong_value_because(&name, expr, &e.to_string()));
            }
            if EmbeddedDatabase::is_session_guc_statement(&statement) {
                return self.engine_set(&statement, &name, expr);
            }
            if crate::sql::params::find(&name).is_some() || crate::sql::is_registered(&name) {
                return Err(VarError::read_only(&name));
            }
            return Err(VarError::unknown(&name));
        };
        let raw = self.resolve_set_value(expr)?;
        let mut stored = normalize(def, &raw)?;
        match def.name {
            "autocommit" => {
                if stored == "ON" && !self.vars.is_on("autocommit") {
                    // MySQL: turning autocommit on commits the open transaction.
                    self.implicit_commit().map_err(|e| db_error(&e))?;
                }
                if stored == "ON" {
                    self.status_flags.set(StatusFlags::SERVER_STATUS_AUTOCOMMIT);
                } else {
                    self.status_flags.clear(StatusFlags::SERVER_STATUS_AUTOCOMMIT);
                }
            }
            "transaction_isolation" => {
                if stored == "READ-UNCOMMITTED" {
                    // Dirty reads do not exist here: the nearest level is used and reported.
                    stored = "READ-COMMITTED".to_string();
                }
            }
            "foreign_key_checks" => {
                if let Some(state) = self.scoped_state() {
                    if stored == "OFF" {
                        state.set_guc(FOREIGN_KEY_CHECKS_GUC, crate::sql::SettingValue::Boolean(false));
                    } else {
                        state.reset_guc(FOREIGN_KEY_CHECKS_GUC);
                    }
                }
            }
            "max_execution_time" => {
                let statement = if raw == RawValue::Default {
                    "RESET statement_timeout".to_string()
                } else {
                    format!("SET statement_timeout = {stored}")
                };
                self.engine_set(&statement, def.name, &stored)?;
            }
            "wait_timeout" => {
                self.timeouts.idle_session_timeout = if raw == RawValue::Default {
                    self.listener_timeouts.idle_session_timeout
                } else {
                    std::time::Duration::from_secs(stored.parse().unwrap_or(28_800))
                };
                // Read back from the policy in force, not from the store.
                return Ok(());
            }
            _ => {}
        }
        self.vars.store(def, stored);
        Ok(())
    }

    /// Forget every session setting: `COM_RESET_CONNECTION`, `COM_CHANGE_USER`.
    pub(super) fn reset_session_state(&mut self) {
        if self.database.session_in_transaction(self.session_id) {
            let _ = self.database.rollback_transaction_for_session(self.session_id);
        }
        self.note_transaction_ended();
        self.database.reset_session_branch(self.session_id);
        if let Some(state) = self.scoped_state() {
            state.reset_all_gucs();
        }
        self.vars.reset();
        self.timeouts = self.listener_timeouts.clone();
        self.next_txn_isolation = None;
        self.next_txn_read_only = None;
        self.prepared_statements.clear();
        self.last_insert_id = 0;
        self.last_row_count = 0;
        self.status_flags = StatusFlags::default_flags();
    }

    // ------------------------------------------------------------------
    // Variables inside other statements
    // ------------------------------------------------------------------

    /// Answer or rewrite a statement that names variables.
    ///
    /// `Ok(None)`: the statement was answered here (a SELECT of variables
    /// only). `Ok(Some(sql))`: run this text, in which every variable is
    /// replaced by its value. `Err`: the refusal to send.
    pub(super) async fn resolve_variables(&mut self, sql: &str) -> Result<std::result::Result<Option<String>, VarError>> {
        let mut text = sql.to_string();
        // `SELECT @a := expr`: assignments are carried out left to right, each
        // one replaced by the value it assigned.
        for _ in 0..64 {
            let view = super::super::translator::code_view(&text);
            let refs = find_var_refs(&text, &view);
            let Some((var, name, expr_start)) = refs.iter().find_map(|var| {
                let RefKind::User { name } = &var.kind else {
                    return None;
                };
                let after = view.get(var.end..)?;
                let gap = after.len() - after.trim_start().len();
                after
                    .trim_start()
                    .starts_with(":=")
                    .then(|| (var.clone(), name.clone(), var.end + gap + 2))
            }) else {
                break;
            };
            if view_has_word(&view, "FROM") || first_word(&view) != "SELECT" {
                return Ok(Err(VarError::not_supported(
                    "assigning a user variable (:=) inside a statement that reads a table",
                )));
            }
            let tail_view = view.get(expr_start..).unwrap_or("");
            let expr_len = vars::split_top_level(tail_view, ',')
                .first()
                .map_or(tail_view.len(), |piece| piece.len());
            let expr_end = expr_start + expr_len;
            let expr = text.get(expr_start..expr_end).unwrap_or("").trim_end_matches(';').to_string();
            let value = match self.eval_expression(&expr) {
                Ok(value) => value,
                Err(e) => return Ok(Err(e)),
            };
            self.vars.set_user(&name, value.clone());
            let shown = text.get(var.start..expr_end).unwrap_or("").trim().to_string();
            let mut replaced = String::with_capacity(text.len());
            replaced.push_str(text.get(..var.start).unwrap_or(""));
            replaced.push_str(&sql_literal(&value));
            if vars::is_bare_select_item(&view, var.start, expr_end) {
                replaced.push_str(" AS ");
                replaced.push_str(&quote_alias(&shown));
            }
            replaced.push_str(text.get(expr_end..).unwrap_or(""));
            text = replaced;
        }

        let view = super::super::translator::code_view(&text);
        let refs = find_var_refs(&text, &view);
        if refs.is_empty() {
            return Ok(Ok(Some(text)));
        }
        if let Some(var) = refs.iter().find(|var| {
            view.get(..var.start)
                .is_some_and(|before| before.trim_end().ends_with(" INTO") || before.trim_end() == "INTO")
        }) {
            let _ = var;
            return Ok(Err(VarError::not_supported("SELECT ... INTO @variable")));
        }
        let mut values = Vec::with_capacity(refs.len());
        for var in &refs {
            match self.ref_value(var) {
                Ok(value) => values.push(value),
                Err(e) => return Ok(Err(e)),
            }
        }
        if let Some(items) = vars::variables_only_select(&text, &view, &refs) {
            let mut columns = Vec::with_capacity(items.len());
            let mut cells = Vec::with_capacity(items.len());
            for (index, name) in items {
                columns.push(name);
                cells.push(values.get(index).cloned().unwrap_or(Value::Null));
            }
            self.send_result_set(&columns, &[Tuple::new(cells)]).await?;
            return Ok(Ok(None));
        }
        let mut out = String::with_capacity(text.len());
        let mut cursor = 0usize;
        for (var, value) in refs.iter().zip(values.iter()) {
            out.push_str(text.get(cursor..var.start).unwrap_or(""));
            out.push_str(&sql_literal(value));
            if vars::is_bare_select_item(&view, var.start, var.end) {
                out.push_str(" AS ");
                out.push_str(&quote_alias(text.get(var.start..var.end).unwrap_or("")));
            }
            cursor = var.end;
        }
        out.push_str(text.get(cursor..).unwrap_or(""));
        Ok(Ok(Some(out)))
    }

    // ------------------------------------------------------------------
    // SHOW VARIABLES / SHOW STATUS
    // ------------------------------------------------------------------

    pub(super) async fn show_variables(&mut self, global: bool, filter_text: &str) -> Result<()> {
        let filter = match parse_show_filter(filter_text) {
            Ok(filter) => filter,
            Err(e) => return self.send_var_error(&e).await,
        };
        let mut rows: Vec<(String, String)> = vars::VARS
            .iter()
            .filter(|def| filter.matches(def.name))
            .map(|def| (def.name.to_string(), self.system_text(def, global)))
            .collect();
        for name in ["innodb_lock_wait_timeout", "lock_wait_timeout"] {
            if filter.matches(name) {
                rows.push((name.to_string(), self.lock_wait_seconds().to_string()));
            }
        }
        for old in ["tx_isolation", "tx_read_only"] {
            if filter.matches(old) {
                if let Some(def) = vars::find(old) {
                    rows.push((old.to_string(), self.system_text(def, global)));
                }
            }
        }
        // The engine's own parameters, under their own names.
        if let Ok((engine_rows, _)) = self
            .database
            .query_with_columns_for_session(self.session_id, "SHOW ALL")
        {
            for row in engine_rows {
                let (Some(Value::String(name)), Some(value)) = (row.values.first(), row.values.get(1)) else {
                    continue;
                };
                if vars::find(name).is_none() && filter.matches(name) {
                    rows.push((name.clone(), value_to_mysql_string(value)));
                }
            }
        }
        rows.sort();
        rows.dedup_by(|a, b| a.0 == b.0);
        self.send_name_value_rows(rows).await
    }

    pub(super) async fn show_status(&mut self, filter_text: &str) -> Result<()> {
        let filter = match parse_show_filter(filter_text) {
            Ok(filter) => filter,
            Err(e) => return self.send_var_error(&e).await,
        };
        let live = crate::session::scoped::snapshot_live_backends(self.database.storage.instance_id()).len();
        let auth = self.auth.as_ref();
        let status: Vec<(&str, String)> = vec![
            (
                "Aborted_connects",
                auth.map_or(0, |a| a.failed_logins()).to_string(),
            ),
            ("Questions", self.questions.to_string()),
            ("Ssl_cipher", String::new()),
            ("Ssl_kx_group", self.ssl_kx_group_value()),
            ("Threads_connected", live.to_string()),
            ("Threads_running", "1".to_string()),
            ("Uptime", super::super::listener_uptime_secs().to_string()),
        ];
        let rows = status
            .into_iter()
            .filter(|(name, _)| matches!(&filter, ShowFilter::All) || filter.matches(name))
            .map(|(name, value)| (name.to_string(), value))
            .collect();
        self.send_name_value_rows(rows).await
    }

    async fn send_name_value_rows(&mut self, rows: Vec<(String, String)>) -> Result<()> {
        let cols = vec!["Variable_name".to_string(), "Value".to_string()];
        let tuples: Vec<Tuple> = rows
            .into_iter()
            .map(|(name, value)| Tuple::new(vec![Value::String(name), Value::String(value)]))
            .collect();
        self.send_result_set(&cols, &tuples).await
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]
mod tests {
    use super::*;

    fn classify(sql: &str) -> Option<std::result::Result<TxnStatement, VarError>> {
        classify_txn(&crate::protocol::mysql::translator::code_view(sql))
    }

    #[test]
    fn transaction_control_is_classified_in_mysql_spellings() {
        assert_eq!(classify("begin"), Some(Ok(TxnStatement::Begin { read_only: None })));
        assert_eq!(classify("BEGIN WORK;"), Some(Ok(TxnStatement::Begin { read_only: None })));
        assert_eq!(
            classify("START TRANSACTION WITH CONSISTENT SNAPSHOT, READ ONLY"),
            Some(Ok(TxnStatement::Begin { read_only: Some(true) }))
        );
        assert_eq!(
            classify("start transaction read write"),
            Some(Ok(TxnStatement::Begin { read_only: Some(false) }))
        );
        assert_eq!(classify("COMMIT /* done */"), Some(Ok(TxnStatement::Commit { chain: false })));
        assert_eq!(classify("COMMIT WORK AND CHAIN"), Some(Ok(TxnStatement::Commit { chain: true })));
        assert_eq!(
            classify("ROLLBACK AND NO CHAIN NO RELEASE"),
            Some(Ok(TxnStatement::Rollback { chain: false }))
        );
        assert_eq!(classify("ROLLBACK TO SAVEPOINT a"), None);
        assert_eq!(classify("ROLLBACK WORK TO a"), None);
        assert_eq!(classify("SELECT 1"), None);
        assert_eq!(classify("COMMIT RELEASE").unwrap().unwrap_err().code, 1235);
        assert_eq!(classify("BEGIN NOT ATOMIC").unwrap().unwrap_err().code, 1064);
        assert_eq!(classify("START TRANSACTION ISOLATION LEVEL X").unwrap().unwrap_err().code, 1064);
    }

    #[test]
    fn a_read_only_transaction_allows_reads_only() {
        let allowed = |sql: &str| allowed_when_read_only(&crate::protocol::mysql::translator::code_view(sql));
        assert!(allowed("SELECT * FROM t WHERE a = 'INSERT INTO'"));
        assert!(allowed("WITH c AS (SELECT 1) SELECT * FROM c"));
        assert!(allowed("SHOW TABLES"));
        assert!(!allowed("INSERT INTO t VALUES (1)"));
        assert!(!allowed("WITH c AS (SELECT 1) DELETE FROM t"));
        assert!(!allowed("CREATE TABLE t (a int)"));
        assert!(!allowed("SELECT * INTO OUTFILE 'x' FROM t"));
    }
}
