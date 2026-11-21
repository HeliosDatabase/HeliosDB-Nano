//! Session variables of one MySQL connection.
//!
//! The listener used to answer `OK` to every `SET` and keep nothing. This module
//! is the store behind `SET`, `SELECT @@name`, `SHOW VARIABLES` and user
//! variables (`@name`): what a variable is called, what it starts as, which
//! values this server can honour, and how the statement text that names
//! variables is read. It holds no connection and runs no SQL; the handler
//! (`handler/session.rs`) applies the effects.
//!
//! Three kinds of entry:
//!
//! * a variable with an effect (`autocommit`, `sql_select_limit`, ...): the
//!   handler acts on the stored value;
//! * a variable that is stored and read back and changes nothing here (its
//!   [`VarDef::effect`] says "no effect"): a hint MySQL itself may ignore, or a
//!   setting whose only honoured value is the one this server already behaves as;
//! * a read-only variable.
//!
//! A value this server cannot honour is refused with MySQL's error 1231, never
//! stored.

use std::collections::HashMap;

use crate::Value;

/// Why a `SET` or a variable read was refused: the MySQL error number, its
/// SQLSTATE and the message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct VarError {
    pub code: u16,
    pub state: &'static str,
    pub message: String,
}

impl VarError {
    /// 1193: the name is not a system variable of this server.
    pub(crate) fn unknown(name: &str) -> Self {
        Self {
            code: 1193,
            state: "HY000",
            message: format!("Unknown system variable '{name}'"),
        }
    }

    /// 1231: the variable exists and this value cannot be honoured.
    pub(crate) fn wrong_value(name: &str, value: &str) -> Self {
        Self {
            code: 1231,
            state: "42000",
            message: format!("Variable '{name}' can't be set to the value of '{value}'"),
        }
    }

    /// 1231 with the reason this server gives.
    pub(crate) fn wrong_value_because(name: &str, value: &str, reason: &str) -> Self {
        Self {
            code: 1231,
            state: "42000",
            message: format!("Variable '{name}' can't be set to the value of '{value}': {reason}"),
        }
    }

    /// 1232: the value is of the wrong type.
    pub(crate) fn wrong_type(name: &str) -> Self {
        Self {
            code: 1232,
            state: "42000",
            message: format!("Incorrect argument type to variable '{name}'"),
        }
    }

    /// 1238: the variable cannot be set.
    pub(crate) fn read_only(name: &str) -> Self {
        Self {
            code: 1238,
            state: "HY000",
            message: format!("Variable '{name}' is a read only variable"),
        }
    }

    /// 1235: a form of the statement this server does not run.
    pub(crate) fn not_supported(what: &str) -> Self {
        Self {
            code: 1235,
            state: "42000",
            message: format!("This version of HeliosDB Nano doesn't yet support '{what}'"),
        }
    }

    /// 1064: the statement cannot be read.
    pub(crate) fn syntax(what: &str) -> Self {
        Self {
            code: 1064,
            state: "42000",
            message: format!("You have an error in your SQL syntax near '{what}'"),
        }
    }
}

/// Which values a variable takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Rule {
    /// `ON` / `OFF` (`1` / `0`, `TRUE` / `FALSE`).
    Bool,
    /// A boolean this server behaves as one value of: the other is refused.
    BoolFixed(bool),
    /// An integer in a range.
    Int { min: i64, max: i64 },
    /// An integer this server behaves as one value of.
    IntFixed(i64),
    /// Any text.
    Text,
    /// A character set: this server stores and sends UTF-8 only.
    Charset,
    /// A collation of a UTF-8 character set.
    Collation,
    /// A list of SQL modes.
    SqlMode,
    /// A time zone: this server's sessions run in UTC.
    TimeZone,
    /// A transaction isolation level.
    Isolation,
    /// One of a fixed list of spellings (compared without case).
    OneOf(&'static [&'static str]),
    /// Not settable.
    ReadOnly,
}

/// One system variable.
#[derive(Debug, Clone, Copy)]
pub(crate) struct VarDef {
    pub name: &'static str,
    pub default: &'static str,
    pub rule: Rule,
    /// What setting it does here, in one line (the documentation table is this text).
    pub effect: &'static str,
}

const NO_EFFECT: &str = "no effect in Nano: stored and read back";

const fn v(name: &'static str, default: &'static str, rule: Rule, effect: &'static str) -> VarDef {
    VarDef {
        name,
        default,
        rule,
        effect,
    }
}

const INT_ANY: Rule = Rule::Int {
    min: 0,
    max: i64::MAX,
};

/// The SQL modes a fresh session reports: the ones this server behaves as.
pub(crate) const DEFAULT_SQL_MODE: &str =
    "ONLY_FULL_GROUP_BY,STRICT_TRANS_TABLES,NO_ZERO_IN_DATE,NO_ZERO_DATE,ERROR_FOR_DIVISION_BY_ZERO,NO_ENGINE_SUBSTITUTION";

/// SQL modes that can be listed: the strict family (this server is always
/// strict, so removing one changes nothing and adding one is what it does
/// already) and the modes that only affect behaviour this server does not have.
const SQL_MODES_ACCEPTED: &[&str] = &[
    "ONLY_FULL_GROUP_BY",
    "STRICT_TRANS_TABLES",
    "STRICT_ALL_TABLES",
    "NO_ZERO_IN_DATE",
    "NO_ZERO_DATE",
    "ERROR_FOR_DIVISION_BY_ZERO",
    "NO_ENGINE_SUBSTITUTION",
    "NO_AUTO_VALUE_ON_ZERO",
    "NO_AUTO_CREATE_USER",
    "NO_DIR_IN_CREATE",
    "TRADITIONAL",
    "PIPES_AS_CONCAT",
    "NO_KEY_OPTIONS",
    "NO_TABLE_OPTIONS",
    "NO_FIELD_OPTIONS",
];

/// SQL modes that change how a valid statement is read or what it returns, in
/// ways this server does not implement.
const SQL_MODES_REFUSED: &[&str] = &[
    "ANSI",
    "ANSI_QUOTES",
    "NO_BACKSLASH_ESCAPES",
    "HIGH_NOT_PRECEDENCE",
    "IGNORE_SPACE",
    "REAL_AS_FLOAT",
    "PAD_CHAR_TO_FULL_LENGTH",
    "NO_UNSIGNED_SUBTRACTION",
    "ALLOW_INVALID_DATES",
    "TIME_TRUNCATE_FRACTIONAL",
    "ORACLE",
    "POSTGRESQL",
    "MSSQL",
    "DB2",
    "MAXDB",
    "MYSQL323",
    "MYSQL40",
];

/// Every system variable the MySQL listener knows, sorted by name.
pub(crate) static VARS: &[VarDef] = &[
    v("auto_increment_increment", "1", Rule::IntFixed(1), "only 1: generated keys step by one"),
    v("auto_increment_offset", "1", Rule::IntFixed(1), "only 1: generated keys start at one"),
    v("autocommit", "ON", Rule::Bool, "OFF: the first statement that reads or writes a table opens a transaction that stays open until COMMIT or ROLLBACK"),
    v("big_tables", "OFF", Rule::Bool, NO_EFFECT),
    v("bulk_insert_buffer_size", "8388608", INT_ANY, NO_EFFECT),
    v("character_set_client", "utf8mb4", Rule::Charset, "UTF-8 character sets only; statements are read as UTF-8"),
    v("character_set_connection", "utf8mb4", Rule::Charset, "UTF-8 character sets only"),
    v("character_set_database", "utf8mb4", Rule::Charset, "UTF-8 character sets only"),
    v("character_set_results", "utf8mb4", Rule::Charset, "UTF-8 character sets only (NULL and binary are accepted: results are sent as stored)"),
    v("character_set_server", "utf8mb4", Rule::ReadOnly, "read only"),
    v("character_set_system", "utf8mb3", Rule::ReadOnly, "read only"),
    v("collation_connection", "utf8mb4_general_ci", Rule::Collation, "a UTF-8 collation is stored; comparisons keep this server's rules"),
    v("collation_database", "utf8mb4_general_ci", Rule::Collation, "a UTF-8 collation is stored; comparisons keep this server's rules"),
    v("collation_server", "utf8mb4_general_ci", Rule::ReadOnly, "read only"),
    v("default_storage_engine", "InnoDB", Rule::Text, NO_EFFECT),
    v("default_week_format", "0", Rule::IntFixed(0), "only 0"),
    v("div_precision_increment", "4", Rule::IntFixed(4), "only 4"),
    v("explicit_defaults_for_timestamp", "ON", Rule::BoolFixed(true), "only ON"),
    v("foreign_key_checks", "ON", Rule::Bool, "OFF: this session's statements skip foreign-key checking"),
    v("group_concat_max_len", "1024", INT_ANY, "no effect in Nano: GROUP_CONCAT results are never truncated"),
    v("have_openssl", "NO", Rule::ReadOnly, "read only: YES when the listener has TLS configured"),
    v("have_ssl", "NO", Rule::ReadOnly, "read only: YES when the listener has TLS configured"),
    v("hostname", "localhost", Rule::ReadOnly, "read only"),
    v("identity", "0", Rule::ReadOnly, "read only: the last generated key of this session"),
    v("information_schema_stats_expiry", "86400", INT_ANY, NO_EFFECT),
    v("init_connect", "", Rule::ReadOnly, "read only"),
    v("innodb_strict_mode", "ON", Rule::Bool, NO_EFFECT),
    v("interactive_timeout", "28800", Rule::Int { min: 1, max: 31_536_000 }, "no effect in Nano: wait_timeout is the idle limit of every connection"),
    v("join_buffer_size", "262144", INT_ANY, NO_EFFECT),
    v("last_insert_id", "0", Rule::ReadOnly, "read only: the last generated key of this session"),
    v("lc_time_names", "en_US", Rule::OneOf(&["en_US"]), "only en_US"),
    v("license", "Apache-2.0", Rule::ReadOnly, "read only"),
    v("long_query_time", "10", INT_ANY, NO_EFFECT),
    v("low_priority_updates", "OFF", Rule::Bool, NO_EFFECT),
    v("lower_case_table_names", "0", Rule::ReadOnly, "read only"),
    v("max_allowed_packet", "67108864", Rule::ReadOnly, "read only"),
    v("max_execution_time", "0", Rule::Int { min: 0, max: 4_294_967_295 }, "milliseconds; this session's statement timeout (every statement, not only SELECT)"),
    v("max_heap_table_size", "16777216", INT_ANY, NO_EFFECT),
    v("max_join_size", "18446744073709551615", Rule::Text, NO_EFFECT),
    v("net_buffer_length", "16384", Rule::ReadOnly, "read only"),
    v("net_read_timeout", "30", Rule::Int { min: 1, max: 31_536_000 }, NO_EFFECT),
    v("net_write_timeout", "60", Rule::Int { min: 1, max: 31_536_000 }, NO_EFFECT),
    v("optimizer_search_depth", "62", INT_ANY, NO_EFFECT),
    v("optimizer_switch", "", Rule::Text, NO_EFFECT),
    v("performance_schema", "OFF", Rule::ReadOnly, "read only"),
    v("profiling", "OFF", Rule::BoolFixed(false), "only OFF"),
    v("protocol_version", "10", Rule::ReadOnly, "read only"),
    v("pseudo_thread_id", "0", Rule::ReadOnly, "read only: this connection's id"),
    v("query_cache_size", "0", Rule::ReadOnly, "read only"),
    v("query_cache_type", "OFF", Rule::ReadOnly, "read only"),
    v("read_buffer_size", "131072", INT_ANY, NO_EFFECT),
    v("session_track_schema", "ON", Rule::Bool, NO_EFFECT),
    v("session_track_state_change", "OFF", Rule::Bool, NO_EFFECT),
    v("session_track_system_variables", "", Rule::Text, NO_EFFECT),
    v("sort_buffer_size", "262144", INT_ANY, NO_EFFECT),
    v("sql_auto_is_null", "OFF", Rule::BoolFixed(false), "only OFF: `col IS NULL` never finds the last inserted row"),
    v("sql_big_selects", "ON", Rule::Bool, NO_EFFECT),
    v("sql_buffer_result", "OFF", Rule::Bool, NO_EFFECT),
    v("sql_log_bin", "OFF", Rule::BoolFixed(false), "only OFF: there is no binary log"),
    v("sql_mode", DEFAULT_SQL_MODE, Rule::SqlMode, "this server is always strict; modes that change how a statement is read are refused"),
    v("sql_notes", "ON", Rule::Bool, NO_EFFECT),
    v("sql_quote_show_create", "ON", Rule::Bool, "no effect in Nano: SHOW CREATE TABLE always quotes names"),
    v("sql_require_primary_key", "OFF", Rule::BoolFixed(false), "only OFF"),
    v("sql_safe_updates", "OFF", Rule::Bool, "ON: an UPDATE or DELETE without WHERE and without LIMIT is refused (error 1175)"),
    v("sql_select_limit", "18446744073709551615", Rule::Text, "a SELECT without its own LIMIT returns at most this many rows"),
    v("sql_warnings", "OFF", Rule::Bool, NO_EFFECT),
    v("ssl_kx_group", "", Rule::ReadOnly, "read only: the TLS key-exchange group of this connection"),
    v("system_time_zone", "UTC", Rule::ReadOnly, "read only"),
    v("time_zone", "SYSTEM", Rule::TimeZone, "UTC only (SYSTEM, UTC, +00:00): sessions run in UTC"),
    v("tmp_table_size", "16777216", INT_ANY, NO_EFFECT),
    v("transaction_isolation", "READ-COMMITTED", Rule::Isolation, "the isolation level of this session's next transactions"),
    v("transaction_read_only", "OFF", Rule::Bool, "ON: this session's next transactions refuse writes (error 1792)"),
    v("unique_checks", "ON", Rule::Bool, "no effect in Nano: unique constraints are always checked"),
    v("version", "", Rule::ReadOnly, "read only"),
    v("version_comment", "HeliosDB Nano", Rule::ReadOnly, "read only"),
    v("version_compile_os", "Linux", Rule::ReadOnly, "read only"),
    v("wait_timeout", "28800", Rule::Int { min: 1, max: 31_536_000 }, "seconds; this connection is closed after being idle that long"),
];

/// Old names MySQL still accepts.
fn canonical_name(name: &str) -> &str {
    match name {
        "tx_isolation" => "transaction_isolation",
        "tx_read_only" => "transaction_read_only",
        "storage_engine" => "default_storage_engine",
        other => other,
    }
}

/// The definition of `name` (lower case; old spellings included).
pub(crate) fn find(name: &str) -> Option<&'static VarDef> {
    let name = canonical_name(name);
    VARS.binary_search_by(|def| def.name.cmp(name))
        .ok()
        .and_then(|i| VARS.get(i))
}

/// A value as written in a `SET`.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum RawValue {
    /// `DEFAULT`.
    Default,
    /// `NULL`.
    Null,
    /// A literal, a bare word or the text an expression evaluated to.
    Text(String),
}

fn parse_bool(text: &str) -> Option<bool> {
    match text.to_ascii_uppercase().as_str() {
        "1" | "ON" | "TRUE" => Some(true),
        "0" | "OFF" | "FALSE" => Some(false),
        _ => None,
    }
}

fn on_off(on: bool) -> String {
    if on { "ON" } else { "OFF" }.to_string()
}

/// A MySQL isolation level spelling (`READ-COMMITTED`, `read committed`, ...) in
/// the `@@transaction_isolation` form.
pub(crate) fn normalize_isolation(text: &str) -> Option<&'static str> {
    let folded: String = text
        .trim()
        .chars()
        .map(|c| if c == '-' || c == '_' { ' ' } else { c.to_ascii_uppercase() })
        .collect();
    let words: Vec<&str> = folded.split_whitespace().collect();
    match words.as_slice() {
        ["READ", "UNCOMMITTED"] => Some("READ-UNCOMMITTED"),
        ["READ", "COMMITTED"] => Some("READ-COMMITTED"),
        ["REPEATABLE", "READ"] => Some("REPEATABLE-READ"),
        ["SERIALIZABLE"] => Some("SERIALIZABLE"),
        _ => None,
    }
}

fn is_utf8_charset(name: &str) -> bool {
    matches!(name, "utf8mb4" | "utf8" | "utf8mb3" | "ascii" | "us-ascii")
}

fn normalize_sql_mode(name: &str, text: &str) -> Result<String, VarError> {
    let mut modes: Vec<String> = Vec::new();
    for raw in text.split(',') {
        let mode = raw.trim().to_ascii_uppercase();
        if mode.is_empty() {
            continue;
        }
        if SQL_MODES_REFUSED.contains(&mode.as_str()) {
            return Err(VarError::wrong_value_because(
                name,
                &mode,
                "this mode changes how statements are read or evaluated, which HeliosDB Nano does not implement",
            ));
        }
        if !SQL_MODES_ACCEPTED.contains(&mode.as_str()) {
            return Err(VarError::wrong_value(name, &mode));
        }
        if !modes.contains(&mode) {
            modes.push(mode);
        }
    }
    Ok(modes.join(","))
}

/// Check `raw` against the variable's rule and return the text to store.
pub(crate) fn normalize(def: &VarDef, raw: &RawValue) -> Result<String, VarError> {
    let name = def.name;
    let text = match raw {
        RawValue::Default => {
            return if def.rule == Rule::ReadOnly {
                Err(VarError::read_only(name))
            } else {
                Ok(def.default.to_string())
            }
        }
        RawValue::Null => {
            // MySQL's connectors send `SET character_set_results = NULL`: no
            // conversion of results, which is what this server does anyway.
            return if name == "character_set_results" {
                Ok("utf8mb4".to_string())
            } else if def.rule == Rule::ReadOnly {
                Err(VarError::read_only(name))
            } else {
                Err(VarError::wrong_value(name, "NULL"))
            };
        }
        RawValue::Text(text) => text.trim(),
    };
    match def.rule {
        Rule::ReadOnly => Err(VarError::read_only(name)),
        Rule::Bool => parse_bool(text)
            .map(on_off)
            .ok_or_else(|| VarError::wrong_value(name, text)),
        Rule::BoolFixed(only) => match parse_bool(text) {
            Some(value) if value == only => Ok(on_off(value)),
            Some(_) => Err(VarError::wrong_value_because(
                name,
                text,
                &format!("HeliosDB Nano only behaves as {}", on_off(only)),
            )),
            None => Err(VarError::wrong_value(name, text)),
        },
        Rule::Int { min, max } => {
            let value: i64 = text.parse().map_err(|_| VarError::wrong_type(name))?;
            if value < min || value > max {
                return Err(VarError::wrong_value(name, text));
            }
            Ok(value.to_string())
        }
        Rule::IntFixed(only) => {
            let value: i64 = text.parse().map_err(|_| VarError::wrong_type(name))?;
            if value != only {
                return Err(VarError::wrong_value_because(
                    name,
                    text,
                    &format!("HeliosDB Nano only behaves as {only}"),
                ));
            }
            Ok(value.to_string())
        }
        Rule::Text => {
            if name == "sql_select_limit" {
                // An unsigned count; MySQL's own "no limit" is 2^64-1.
                let _: u64 = text.parse().map_err(|_| VarError::wrong_type(name))?;
            }
            Ok(text.to_string())
        }
        Rule::Charset => {
            let lower = text.to_ascii_lowercase();
            if is_utf8_charset(&lower) || (name == "character_set_results" && lower == "binary") {
                Ok(lower)
            } else {
                Err(VarError::wrong_value_because(
                    name,
                    text,
                    "HeliosDB Nano stores and sends UTF-8 only",
                ))
            }
        }
        Rule::Collation => {
            let lower = text.to_ascii_lowercase();
            let charset = lower.split('_').next().unwrap_or("");
            if is_utf8_charset(charset) || lower == "binary" {
                Ok(lower)
            } else {
                Err(VarError::wrong_value_because(
                    name,
                    text,
                    "HeliosDB Nano stores and sends UTF-8 only",
                ))
            }
        }
        Rule::SqlMode => normalize_sql_mode(name, text),
        Rule::TimeZone => {
            let upper = text.to_ascii_uppercase();
            match upper.as_str() {
                "SYSTEM" => Ok("SYSTEM".to_string()),
                "UTC" | "GMT" | "ETC/UTC" | "+00:00" | "-00:00" | "+0:00" => Ok(text.to_string()),
                _ => Err(VarError::wrong_value_because(
                    name,
                    text,
                    "HeliosDB Nano sessions run in UTC",
                )),
            }
        }
        Rule::Isolation => normalize_isolation(text)
            .map(str::to_string)
            .ok_or_else(|| VarError::wrong_value(name, text)),
        Rule::OneOf(list) => list
            .iter()
            .find(|allowed| allowed.eq_ignore_ascii_case(text))
            .map(|allowed| (*allowed).to_string())
            .ok_or_else(|| VarError::wrong_value(name, text)),
    }
}

/// The variables one connection has set, and its user variables.
#[derive(Debug, Default)]
pub(crate) struct SessionVars {
    values: HashMap<&'static str, String>,
    user: HashMap<String, Value>,
}

impl SessionVars {
    /// The stored value of a known variable: what the session set, else the default.
    pub(crate) fn get(&self, def: &VarDef) -> String {
        self.values
            .get(def.name)
            .cloned()
            .unwrap_or_else(|| def.default.to_string())
    }

    /// The stored value by name; `None` for a name this module does not know.
    pub(crate) fn get_named(&self, name: &str) -> Option<String> {
        find(name).map(|def| self.get(def))
    }

    /// Is the boolean variable `name` on?
    pub(crate) fn is_on(&self, name: &str) -> bool {
        self.get_named(name).is_some_and(|value| value == "ON")
    }

    /// Store an already normalised value.
    pub(crate) fn store(&mut self, def: &'static VarDef, value: String) {
        if value == def.default {
            self.values.remove(def.name);
        } else {
            self.values.insert(def.name, value);
        }
    }

    /// Has the session set `name` to something other than its default?
    pub(crate) fn is_set(&self, name: &str) -> bool {
        self.values.contains_key(canonical_name(name))
    }

    /// A user variable (`@name`); one never assigned is NULL.
    pub(crate) fn user(&self, name: &str) -> Value {
        self.user
            .get(&name.to_ascii_lowercase())
            .cloned()
            .unwrap_or(Value::Null)
    }

    /// Assign a user variable.
    pub(crate) fn set_user(&mut self, name: &str, value: Value) {
        self.user.insert(name.to_ascii_lowercase(), value);
    }

    /// Forget everything (`COM_RESET_CONNECTION`, `COM_CHANGE_USER`).
    pub(crate) fn reset(&mut self) {
        self.values.clear();
        self.user.clear();
    }
}

/// How a stored value is sent as a `SELECT @@name` cell.
pub(crate) fn select_value(def: &VarDef, stored: &str) -> Value {
    match def.rule {
        Rule::Bool | Rule::BoolFixed(_) => Value::Int8(i64::from(stored == "ON")),
        Rule::Int { .. } | Rule::IntFixed(_) => stored.parse().map_or_else(|_| text_value(stored), Value::Int8),
        _ => match def.name {
            // Read-only variables that are numbers or booleans.
            "last_insert_id" | "identity" | "lower_case_table_names" | "max_allowed_packet" | "net_buffer_length"
            | "protocol_version" | "pseudo_thread_id" | "query_cache_size" => {
                stored.parse().map_or_else(|_| text_value(stored), Value::Int8)
            }
            "performance_schema" => Value::Int8(i64::from(stored == "ON")),
            "sql_select_limit" | "max_join_size" => stored
                .parse::<i64>()
                .map_or_else(|_| text_value(stored), Value::Int8),
            _ => text_value(stored),
        },
    }
}

fn text_value(text: &str) -> Value {
    Value::String(text.to_string())
}

/// A value as a SQL literal the engine reads back as the same value.
pub(crate) fn sql_literal(value: &Value) -> String {
    match value {
        Value::Null => "NULL".to_string(),
        Value::Boolean(b) => if *b { "1" } else { "0" }.to_string(),
        Value::Int2(n) => n.to_string(),
        Value::Int4(n) => n.to_string(),
        Value::Int8(n) => n.to_string(),
        Value::Float4(n) if n.is_finite() => format!("{n:?}"),
        Value::Float8(n) if n.is_finite() => format!("{n:?}"),
        Value::Numeric(n) => n.to_string(),
        Value::String(s) => quote_literal(s),
        other => quote_literal(&super::handler::value_to_mysql_string(other)),
    }
}

/// `'text'` with embedded quotes doubled.
pub(crate) fn quote_literal(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('\'');
    for ch in text.chars() {
        if ch == '\'' {
            out.push('\'');
        }
        out.push(ch);
    }
    out.push('\'');
    out
}

// ---------------------------------------------------------------------------
// Reading `SET`
// ---------------------------------------------------------------------------

/// Whose value a `SET` names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Scope {
    Session,
    Global,
}

/// One thing a `SET` statement does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Assignment {
    /// `[GLOBAL | SESSION] name = expr` / `@@[scope.]name = expr`.
    System { scope: Scope, name: String, expr: String },
    /// `@name = expr` / `@name := expr`.
    User { name: String, expr: String },
    /// `NAMES charset [COLLATE collation]`; `None` is `DEFAULT`.
    Names {
        charset: Option<String>,
        collation: Option<String>,
    },
    /// `CHARACTER SET charset` / `CHARSET charset`; `None` is `DEFAULT`.
    CharacterSet { charset: Option<String> },
    /// `[GLOBAL | SESSION] TRANSACTION ...`. `session` is false for the form
    /// that applies to the next transaction only.
    Transaction {
        scope: Scope,
        session: bool,
        isolation: Option<&'static str>,
        read_only: Option<bool>,
    },
    /// `PASSWORD [FOR account] = 'text'`.
    Password { account: Option<String>, expr: String },
}

/// Split `text` at the top-level occurrences of `separator`: outside quotes,
/// backticks and parentheses.
pub(crate) fn split_top_level(text: &str, separator: char) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut depth = 0usize;
    let mut quote: Option<char> = None;
    let mut start = 0usize;
    let mut chars = text.char_indices().peekable();
    while let Some((at, ch)) = chars.next() {
        match quote {
            Some(open) => {
                if ch == '\\' && open != '`' {
                    chars.next();
                } else if ch == open {
                    if chars.peek().is_some_and(|(_, next)| *next == open) {
                        chars.next();
                    } else {
                        quote = None;
                    }
                }
            }
            None => match ch {
                '\'' | '"' | '`' => quote = Some(ch),
                '(' => depth += 1,
                ')' => depth = depth.saturating_sub(1),
                c if c == separator && depth == 0 => {
                    parts.push(text.get(start..at).unwrap_or(""));
                    start = at + ch.len_utf8();
                }
                _ => {}
            },
        }
    }
    parts.push(text.get(start..).unwrap_or(""));
    parts
}

/// Strip a leading keyword (whole word, any case) and the space after it.
pub(crate) fn strip_keyword<'a>(text: &'a str, keyword: &str) -> Option<&'a str> {
    let head = text.get(..keyword.len())?;
    if !head.eq_ignore_ascii_case(keyword) {
        return None;
    }
    let rest = text.get(keyword.len()..)?;
    match rest.chars().next() {
        None => Some(rest),
        Some(c) if c.is_ascii_alphanumeric() || c == '_' => None,
        Some(_) => Some(rest.trim_start()),
    }
}

/// The text of a quoted word or a bare one: `'utf8mb4'`, `utf8mb4`, `` `x` ``.
pub(crate) fn unquote_word(text: &str) -> String {
    let text = text.trim();
    let bytes = text.as_bytes();
    match (bytes.first(), bytes.last()) {
        (Some(open @ (b'\'' | b'"' | b'`')), Some(close)) if text.len() >= 2 && open == close => {
            unescape_literal(text.get(1..text.len() - 1).unwrap_or(""), char::from(*open))
        }
        _ => text.to_string(),
    }
}

/// The contents of a MySQL string literal with its escapes resolved.
pub(crate) fn unescape_literal(inner: &str, quote: char) -> String {
    let mut out = String::with_capacity(inner.len());
    let mut chars = inner.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\\' && quote != '`' {
            match chars.next() {
                Some('n') => out.push('\n'),
                Some('t') => out.push('\t'),
                Some('r') => out.push('\r'),
                Some('0') => out.push('\0'),
                Some('b') => out.push('\u{8}'),
                Some('Z') => out.push('\u{1a}'),
                Some(other) => out.push(other),
                None => out.push('\\'),
            }
        } else if ch == quote && chars.peek() == Some(&quote) {
            chars.next();
            out.push(quote);
        } else {
            out.push(ch);
        }
    }
    out
}

/// Is `text` one complete quoted literal (`'...'` or `"..."`)?
pub(crate) fn is_single_literal(text: &str) -> bool {
    let text = text.trim();
    let Some(open) = text.chars().next().filter(|c| *c == '\'' || *c == '"') else {
        return false;
    };
    let mut chars = text.char_indices().skip(1).peekable();
    while let Some((at, ch)) = chars.next() {
        if ch == '\\' {
            chars.next();
        } else if ch == open {
            if chars.peek().is_some_and(|(_, next)| *next == open) {
                chars.next();
            } else {
                return at + ch.len_utf8() == text.len();
            }
        }
    }
    false
}

fn parse_scope(text: &str) -> Option<(Scope, &str)> {
    for (keyword, scope) in [
        ("GLOBAL", Scope::Global),
        ("PERSIST_ONLY", Scope::Global),
        ("PERSIST", Scope::Global),
        ("SESSION", Scope::Session),
        ("LOCAL", Scope::Session),
    ] {
        if let Some(rest) = strip_keyword(text, keyword) {
            // `SET session = 1` is not a scope word followed by a name.
            if !rest.starts_with('=') && !rest.starts_with(":=") && !rest.is_empty() {
                return Some((scope, rest));
            }
        }
    }
    None
}

fn parse_transaction(scope: Scope, session: bool, rest: &str) -> Result<Assignment, VarError> {
    let mut isolation = None;
    let mut read_only = None;
    for part in split_top_level(rest, ',') {
        let part = part.trim();
        if let Some(level) = strip_keyword(part, "ISOLATION").and_then(|r| strip_keyword(r, "LEVEL")) {
            isolation = Some(normalize_isolation(level).ok_or_else(|| VarError::syntax(part))?);
        } else if let Some(mode) = strip_keyword(part, "READ") {
            read_only = Some(match mode.trim().to_ascii_uppercase().as_str() {
                "ONLY" => true,
                "WRITE" => false,
                _ => return Err(VarError::syntax(part)),
            });
        } else {
            return Err(VarError::syntax(part));
        }
    }
    if isolation.is_none() && read_only.is_none() {
        return Err(VarError::syntax(rest));
    }
    Ok(Assignment::Transaction {
        scope,
        session,
        isolation,
        read_only,
    })
}

/// Where the assignment operator of `piece` is: `(index, operator length)`.
fn find_assign(piece: &str) -> Option<(usize, usize)> {
    let mut quote: Option<char> = None;
    let mut chars = piece.char_indices().peekable();
    while let Some((at, ch)) = chars.next() {
        match quote {
            Some(open) => {
                if ch == '\\' && open != '`' {
                    chars.next();
                } else if ch == open {
                    quote = None;
                }
            }
            None => match ch {
                '\'' | '"' | '`' => quote = Some(ch),
                ':' if chars.peek().is_some_and(|(_, next)| *next == '=') => return Some((at, 2)),
                '=' => return Some((at, 1)),
                _ => {}
            },
        }
    }
    None
}

/// Read the body of a `SET` statement (the text after `SET`).
pub(crate) fn parse_set(body: &str) -> Result<Vec<Assignment>, VarError> {
    let body = body.trim().trim_end_matches(';').trim();
    if body.is_empty() {
        return Err(VarError::syntax("SET"));
    }

    // Whole-statement forms.
    if let Some(rest) = strip_keyword(body, "NAMES") {
        let mut words = rest.split_whitespace();
        let charset = words.next().map(unquote_word).ok_or_else(|| VarError::syntax(body))?;
        let collation = match words.next() {
            None => None,
            Some(word) if word.eq_ignore_ascii_case("COLLATE") => {
                Some(words.next().map(unquote_word).ok_or_else(|| VarError::syntax(body))?)
            }
            Some(_) => return Err(VarError::syntax(body)),
        };
        if words.next().is_some() {
            return Err(VarError::syntax(body));
        }
        let charset = (!charset.eq_ignore_ascii_case("DEFAULT")).then_some(charset);
        return Ok(vec![Assignment::Names { charset, collation }]);
    }
    let charset_rest = strip_keyword(body, "CHARSET")
        .or_else(|| strip_keyword(body, "CHARACTER").and_then(|rest| strip_keyword(rest, "SET")));
    if let Some(rest) = charset_rest {
        if !rest.starts_with('=') {
            let charset = unquote_word(rest);
            if charset.is_empty() || charset.contains(char::is_whitespace) {
                return Err(VarError::syntax(body));
            }
            let charset = (!charset.eq_ignore_ascii_case("DEFAULT")).then_some(charset);
            return Ok(vec![Assignment::CharacterSet { charset }]);
        }
    }
    if let Some(rest) = strip_keyword(body, "PASSWORD") {
        let (account, rest) = match strip_keyword(rest, "FOR") {
            Some(after_for) => {
                let (at, len) = find_assign(after_for).ok_or_else(|| VarError::syntax(body))?;
                (
                    Some(after_for.get(..at).unwrap_or("").trim().to_string()),
                    after_for.get(at + len..).unwrap_or(""),
                )
            }
            None => {
                let rest = rest.trim_start();
                let rest = rest.strip_prefix(":=").or_else(|| rest.strip_prefix('=')).ok_or_else(|| VarError::syntax(body))?;
                (None, rest)
            }
        };
        return Ok(vec![Assignment::Password {
            account,
            expr: rest.trim().to_string(),
        }]);
    }
    {
        let (scope, session, rest) = match parse_scope(body) {
            Some((scope, rest)) => (scope, true, rest),
            None => (Scope::Session, false, body),
        };
        if let Some(characteristics) = strip_keyword(rest, "TRANSACTION") {
            if !characteristics.starts_with('=') {
                return parse_transaction(scope, session, characteristics).map(|a| vec![a]);
            }
        }
    }

    let mut out = Vec::new();
    let mut scope = Scope::Session;
    for piece in split_top_level(body, ',') {
        let mut piece = piece.trim();
        if piece.is_empty() {
            return Err(VarError::syntax(body));
        }
        if let Some((named, rest)) = parse_scope(piece) {
            scope = named;
            piece = rest;
        }
        let (at, len) = find_assign(piece).ok_or_else(|| VarError::syntax(piece))?;
        let target = piece.get(..at).unwrap_or("").trim();
        let expr = piece.get(at + len..).unwrap_or("").trim().to_string();
        if expr.is_empty() || target.is_empty() {
            return Err(VarError::syntax(piece));
        }
        if let Some(system) = target.strip_prefix("@@") {
            let lower = system.to_ascii_lowercase();
            let (var_scope, name) = if let Some(name) = lower.strip_prefix("global.") {
                (Scope::Global, name)
            } else if let Some(name) = lower.strip_prefix("persist.") {
                (Scope::Global, name)
            } else if let Some(name) = lower.strip_prefix("session.").or_else(|| lower.strip_prefix("local.")) {
                (Scope::Session, name)
            } else {
                (scope, lower.as_str())
            };
            out.push(Assignment::System {
                scope: var_scope,
                name: name.to_string(),
                expr,
            });
        } else if let Some(user) = target.strip_prefix('@') {
            out.push(Assignment::User {
                name: unquote_word(user),
                expr,
            });
        } else {
            if !target
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.' || c == '`')
            {
                return Err(VarError::syntax(piece));
            }
            out.push(Assignment::System {
                scope,
                name: target.trim_matches('`').to_ascii_lowercase(),
                expr,
            });
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Variables named inside a statement
// ---------------------------------------------------------------------------

/// A variable a statement names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RefKind {
    System { global: bool, name: String },
    User { name: String },
}

/// One `@@name` or `@name` in a statement's code, as a byte range.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct VarRef {
    pub start: usize,
    pub end: usize,
    pub kind: RefKind,
}

fn is_name_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'$'
}

/// Every variable named in the code of `sql`. `view` is the statement's
/// code-only view (`translator::code_view`): text inside literals, quoted
/// names and comments is blank there, so it never matches.
///
/// An `@` directly after a name, a quote or another `@` is not a variable
/// (`'user'@'host'`, PostgreSQL's `@>` and `<@` operators, an e-mail address
/// in an unquoted position).
pub(crate) fn find_var_refs(sql: &str, view: &str) -> Vec<VarRef> {
    let bytes = view.as_bytes();
    let mut refs = Vec::new();
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes.get(i) != Some(&b'@') {
            i += 1;
            continue;
        }
        let before_ok = i == 0
            || bytes
                .get(i - 1)
                .is_none_or(|b| !(is_name_byte(*b) || matches!(b, b'\'' | b'"' | b'`' | b'@' | b'<' | b'-')));
        if !before_ok {
            i += 1;
            continue;
        }
        if bytes.get(i + 1) == Some(&b'@') {
            // System variable: @@[scope.]name
            let mut j = i + 2;
            let name_start = j;
            while bytes.get(j).is_some_and(|b| is_name_byte(*b) || *b == b'.') {
                j += 1;
            }
            let text = view.get(name_start..j).unwrap_or("").to_ascii_lowercase();
            let starts_like_a_name = text.as_bytes().first().is_some_and(|b| b.is_ascii_alphabetic() || *b == b'_');
            if !starts_like_a_name {
                i += 2;
                continue;
            }
            let text = text.trim_end_matches('.');
            let end = name_start + text.len();
            // `a @@to_tsquery('x')`: the text-search operator before a function
            // call, not a variable.
            if bytes.get(j) == Some(&b'(') {
                i = end;
                continue;
            }
            let (global, name) = if let Some(name) = text.strip_prefix("global.") {
                (true, name)
            } else if let Some(name) = text.strip_prefix("session.").or_else(|| text.strip_prefix("local.")) {
                (false, name)
            } else {
                (false, text)
            };
            refs.push(VarRef {
                start: i,
                end,
                kind: RefKind::System {
                    global,
                    name: name.to_string(),
                },
            });
            i = end;
            continue;
        }
        // User variable: @name, @'name', @"name", @`name`
        match bytes.get(i + 1) {
            Some(open @ (b'\'' | b'"' | b'`')) => {
                // The view keeps the delimiters and blanks the interior.
                let mut j = i + 2;
                while bytes.get(j).is_some_and(|b| b != open) {
                    j += 1;
                }
                if bytes.get(j) == Some(open) {
                    let name = sql.get(i + 2..j).unwrap_or("").to_string();
                    refs.push(VarRef {
                        start: i,
                        end: j + 1,
                        kind: RefKind::User { name },
                    });
                    i = j + 1;
                    continue;
                }
                i += 1;
            }
            Some(b) if is_name_byte(*b) => {
                let mut j = i + 1;
                while bytes.get(j).is_some_and(|b| is_name_byte(*b) || *b == b'.') {
                    j += 1;
                }
                let name = sql.get(i + 1..j).unwrap_or("").trim_end_matches('.').to_string();
                let end = i + 1 + name.len();
                refs.push(VarRef {
                    start: i,
                    end,
                    kind: RefKind::User { name },
                });
                i = end;
            }
            _ => i += 1,
        }
    }
    refs
}

/// Is the text between `SELECT`/`,` and `,`/`FROM`/end exactly this reference:
/// a select item of its own, which MySQL names after the variable?
pub(crate) fn is_bare_select_item(view: &str, start: usize, end: usize) -> bool {
    let before = view.get(..start).unwrap_or("").trim_end();
    let before_ok = before.ends_with(',')
        || (before.ends_with("SELECT")
            && !before
                .as_bytes()
                .get(before.len().wrapping_sub(7))
                .is_some_and(|b| is_name_byte(*b)));
    if !before_ok {
        return false;
    }
    let after = view.get(end..).unwrap_or("").trim_start();
    after.is_empty()
        || after.starts_with(',')
        || after.starts_with(';')
        || strip_keyword(after, "FROM").is_some()
        || strip_keyword(after, "LIMIT").is_some()
        || strip_keyword(after, "UNION").is_some()
}

/// `"name"` as an identifier the engine keeps verbatim.
pub(crate) fn quote_alias(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// The items of a statement that only selects variables,
/// `SELECT @@a [AS x], @b [AS y] [LIMIT 1]`: `(index into refs, column name)`
/// per item. `None` for anything else, which the engine runs after the
/// variables are replaced by their values.
pub(crate) fn variables_only_select(sql: &str, view: &str, refs: &[VarRef]) -> Option<Vec<(usize, String)>> {
    let trimmed_view = view.trim_start();
    let offset = view.len() - trimmed_view.len();
    let rest_view = strip_keyword(trimmed_view, "SELECT")?;
    let list_start = offset + (trimmed_view.len() - rest_view.len());
    let mut list_end = view.trim_end().trim_end_matches(';').trim_end().len();
    // `SELECT @@x LIMIT 1` (mysqlclient's connectors append it).
    let body = view.get(list_start..list_end)?;
    if let Some(at) = body.rfind("LIMIT") {
        let tail = body.get(at + 5..)?.trim();
        if !tail.is_empty() && tail.bytes().all(|b| b.is_ascii_digit()) && tail != "0" {
            list_end = list_start + at;
        }
    }
    let list_view = view.get(list_start..list_end)?;
    let mut items = Vec::new();
    let mut cursor = list_start;
    for part in split_top_level(list_view, ',') {
        let part_start = cursor;
        cursor += part.len() + 1;
        let lead = part.len() - part.trim_start().len();
        let item_start = part_start + lead;
        let (index, var) = refs.iter().enumerate().find(|(_, r)| r.start == item_start)?;
        let tail_start = var.end;
        let tail_end = part_start + part.len();
        let tail_sql = sql.get(tail_start..tail_end)?.trim();
        let name = if tail_sql.is_empty() {
            sql.get(var.start..var.end)?.to_string()
        } else {
            let alias = strip_keyword(tail_sql, "AS").unwrap_or(tail_sql).trim();
            let plain = alias
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$');
            let quoted = alias.len() >= 2
                && matches!(alias.as_bytes().first(), Some(b'\'' | b'"' | b'`'))
                && alias.as_bytes().first() == alias.as_bytes().last()
                && !alias
                    .get(1..alias.len() - 1)
                    .is_some_and(|inner| inner.contains(['\'', '"', '`']));
            if alias.is_empty() || !(plain || quoted) {
                return None;
            }
            unquote_word(alias)
        };
        items.push((index, name));
    }
    (!items.is_empty()).then_some(items)
}

/// A `LIKE` pattern (`%` and `_`, backslash escapes) matched without case,
/// as `SHOW VARIABLES LIKE` matches names.
pub(crate) fn like_match(value: &str, pattern: &str) -> bool {
    fn step(value: &[char], pattern: &[char]) -> bool {
        match pattern.split_first() {
            None => value.is_empty(),
            Some(('%', rest)) => (0..=value.len()).any(|skip| step(value.get(skip..).unwrap_or(&[]), rest)),
            Some(('_', rest)) => value.split_first().is_some_and(|(_, tail)| step(tail, rest)),
            Some(('\\', rest)) => match (rest.split_first(), value.split_first()) {
                (Some((escaped, rest)), Some((first, tail))) => escaped == first && step(tail, rest),
                _ => false,
            },
            Some((ch, rest)) => value
                .split_first()
                .is_some_and(|(first, tail)| first == ch && step(tail, rest)),
        }
    }
    let value: Vec<char> = value.to_lowercase().chars().collect();
    let pattern: Vec<char> = pattern.to_lowercase().chars().collect();
    step(&value, &pattern)
}

/// The filter of `SHOW VARIABLES` / `SHOW STATUS`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ShowFilter {
    All,
    /// `LIKE 'pattern'`.
    Like(String),
    /// `WHERE Variable_name = 'a' [OR ...]` / `IN ('a', 'b')`.
    Names(Vec<String>),
}

impl ShowFilter {
    pub(crate) fn matches(&self, name: &str) -> bool {
        match self {
            Self::All => true,
            Self::Like(pattern) => like_match(name, pattern),
            Self::Names(names) => names.iter().any(|n| n.eq_ignore_ascii_case(name)),
        }
    }
}

/// Read the filter that follows `SHOW ... VARIABLES` / `STATUS`.
pub(crate) fn parse_show_filter(rest: &str) -> Result<ShowFilter, VarError> {
    let rest = rest.trim().trim_end_matches(';').trim();
    if rest.is_empty() {
        return Ok(ShowFilter::All);
    }
    if let Some(pattern) = strip_keyword(rest, "LIKE") {
        if !is_single_literal(pattern) {
            return Err(VarError::syntax(rest));
        }
        return Ok(ShowFilter::Like(unquote_word(pattern)));
    }
    if let Some(condition) = strip_keyword(rest, "WHERE") {
        // The forms connectors send: comparisons of Variable_name with
        // literals, joined by OR, or one IN list, or one LIKE.
        let view = super::translator::code_view(condition);
        let words: Vec<&str> = view
            .split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
            .filter(|w| !w.is_empty())
            .collect();
        let only_known = words
            .iter()
            .all(|w| matches!(*w, "VARIABLE_NAME" | "OR" | "IN" | "LIKE"));
        let has_like = words.contains(&"LIKE");
        let literals = literals_of(condition);
        if !only_known || literals.is_empty() || (has_like && literals.len() != 1) || view.contains('!') || view.contains('<') {
            return Err(VarError::not_supported("SHOW ... WHERE with this condition"));
        }
        return Ok(match (has_like, literals.into_iter().next()) {
            (true, Some(pattern)) => ShowFilter::Like(pattern),
            _ => ShowFilter::Names(literals_of(condition)),
        });
    }
    Err(VarError::syntax(rest))
}

/// The string literals of `text`, in order.
fn literals_of(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut chars = text.char_indices().peekable();
    while let Some((at, ch)) = chars.next() {
        if ch != '\'' && ch != '"' {
            continue;
        }
        let mut end = None;
        while let Some((j, c)) = chars.next() {
            if c == '\\' {
                chars.next();
            } else if c == ch {
                if chars.peek().is_some_and(|(_, next)| *next == ch) {
                    chars.next();
                } else {
                    end = Some(j);
                    break;
                }
            }
        }
        if let Some(end) = end {
            out.push(unescape_literal(text.get(at + 1..end).unwrap_or(""), ch));
        }
    }
    out
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::protocol::mysql::translator::code_view;

    #[test]
    fn the_table_is_sorted_and_every_variable_says_what_it_does() {
        for pair in VARS.windows(2) {
            assert!(pair[0].name < pair[1].name, "{} before {}", pair[0].name, pair[1].name);
        }
        for def in VARS {
            assert!(!def.effect.is_empty(), "{} has no effect text", def.name);
            if def.rule != Rule::ReadOnly {
                // The default is a value the variable's own rule accepts.
                let stored = normalize(def, &RawValue::Text(def.default.to_string()));
                assert_eq!(stored.as_deref(), Ok(def.default), "{}", def.name);
            }
        }
    }

    #[test]
    fn the_compatibility_page_lists_every_variable_with_its_effect() {
        let page = include_str!("../../../docs/compatibility/mysql.md");
        for def in VARS {
            let row = page
                .lines()
                .find(|line| line.starts_with(&format!("| `{}` |", def.name)))
                .unwrap_or_else(|| panic!("docs/compatibility/mysql.md has no row for {}", def.name));
            assert!(row.contains(def.effect), "the row of {} says something else: {row}", def.name);
        }
    }

    #[test]
    fn values_that_cannot_be_honoured_are_refused_with_1231() {
        let sql_mode = find("sql_mode").unwrap();
        for bad in ["ANSI_QUOTES", "STRICT_TRANS_TABLES,NO_BACKSLASH_ESCAPES", "ansi"] {
            let err = normalize(sql_mode, &RawValue::Text(bad.into())).unwrap_err();
            assert_eq!(err.code, 1231, "{bad}");
        }
        assert_eq!(
            normalize(sql_mode, &RawValue::Text("no_auto_value_on_zero".into())).unwrap(),
            "NO_AUTO_VALUE_ON_ZERO"
        );
        assert_eq!(normalize(sql_mode, &RawValue::Text(String::new())).unwrap(), "");
        assert_eq!(normalize(sql_mode, &RawValue::Text("NOT_A_MODE".into())).unwrap_err().code, 1231);

        let tz = find("time_zone").unwrap();
        assert_eq!(normalize(tz, &RawValue::Text("+00:00".into())).unwrap(), "+00:00");
        assert_eq!(normalize(tz, &RawValue::Text("Europe/Madrid".into())).unwrap_err().code, 1231);

        let cs = find("character_set_client").unwrap();
        assert_eq!(normalize(cs, &RawValue::Text("UTF8".into())).unwrap(), "utf8");
        assert_eq!(normalize(cs, &RawValue::Text("latin1".into())).unwrap_err().code, 1231);
        let results = find("character_set_results").unwrap();
        assert_eq!(normalize(results, &RawValue::Null).unwrap(), "utf8mb4");

        let auto_null = find("sql_auto_is_null").unwrap();
        assert_eq!(normalize(auto_null, &RawValue::Text("0".into())).unwrap(), "OFF");
        assert_eq!(normalize(auto_null, &RawValue::Text("1".into())).unwrap_err().code, 1231);

        assert_eq!(normalize(find("version").unwrap(), &RawValue::Text("9".into())).unwrap_err().code, 1238);
        assert_eq!(normalize(find("wait_timeout").unwrap(), &RawValue::Text("x".into())).unwrap_err().code, 1232);
        assert_eq!(normalize(find("wait_timeout").unwrap(), &RawValue::Text("0".into())).unwrap_err().code, 1231);
    }

    #[test]
    fn old_names_find_the_same_variable() {
        assert_eq!(find("tx_isolation").unwrap().name, "transaction_isolation");
        assert_eq!(find("tx_read_only").unwrap().name, "transaction_read_only");
        assert!(find("no_such_variable").is_none());
    }

    #[test]
    fn set_is_read_into_assignments() {
        assert_eq!(
            parse_set("autocommit = 0").unwrap(),
            vec![Assignment::System {
                scope: Scope::Session,
                name: "autocommit".into(),
                expr: "0".into()
            }]
        );
        assert_eq!(
            parse_set("@OLD_SQL_MODE=@@SQL_MODE, SQL_MODE='NO_AUTO_VALUE_ON_ZERO'").unwrap(),
            vec![
                Assignment::User {
                    name: "OLD_SQL_MODE".into(),
                    expr: "@@SQL_MODE".into()
                },
                Assignment::System {
                    scope: Scope::Session,
                    name: "sql_mode".into(),
                    expr: "'NO_AUTO_VALUE_ON_ZERO'".into()
                },
            ]
        );
        assert_eq!(
            parse_set("@@session.sql_mode := 'a,b', GLOBAL wait_timeout = 5, x = 1").unwrap(),
            vec![
                Assignment::System {
                    scope: Scope::Session,
                    name: "sql_mode".into(),
                    expr: "'a,b'".into()
                },
                Assignment::System {
                    scope: Scope::Global,
                    name: "wait_timeout".into(),
                    expr: "5".into()
                },
                Assignment::System {
                    scope: Scope::Global,
                    name: "x".into(),
                    expr: "1".into()
                },
            ]
        );
        assert_eq!(
            parse_set("NAMES utf8mb4 COLLATE utf8mb4_unicode_ci").unwrap(),
            vec![Assignment::Names {
                charset: Some("utf8mb4".into()),
                collation: Some("utf8mb4_unicode_ci".into())
            }]
        );
        assert_eq!(
            parse_set("CHARACTER SET 'utf8'").unwrap(),
            vec![Assignment::CharacterSet {
                charset: Some("utf8".into())
            }]
        );
        assert_eq!(
            parse_set("SESSION TRANSACTION ISOLATION LEVEL REPEATABLE READ, READ ONLY").unwrap(),
            vec![Assignment::Transaction {
                scope: Scope::Session,
                session: true,
                isolation: Some("REPEATABLE-READ"),
                read_only: Some(true)
            }]
        );
        assert_eq!(
            parse_set("TRANSACTION READ WRITE").unwrap(),
            vec![Assignment::Transaction {
                scope: Scope::Session,
                session: false,
                isolation: None,
                read_only: Some(false)
            }]
        );
        assert_eq!(
            parse_set("PASSWORD FOR 'u'@'%' = 'p=1'").unwrap(),
            vec![Assignment::Password {
                account: Some("'u'@'%'".into()),
                expr: "'p=1'".into()
            }]
        );
        assert_eq!(parse_set("autocommit").unwrap_err().code, 1064);
        assert_eq!(parse_set("TRANSACTION ISOLATION LEVEL CHAOS").unwrap_err().code, 1064);
    }

    fn refs_of(sql: &str) -> Vec<VarRef> {
        find_var_refs(sql, &code_view(sql))
    }

    #[test]
    fn variables_are_found_in_code_and_not_in_literals() {
        let sql = "SELECT @@session.sql_mode, @@GLOBAL.wait_timeout, @x, '@@not', \"@y\" FROM t WHERE a @> b AND c = @`my var`";
        let refs = refs_of(sql);
        let kinds: Vec<RefKind> = refs.iter().map(|r| r.kind.clone()).collect();
        assert_eq!(
            kinds,
            vec![
                RefKind::System {
                    global: false,
                    name: "sql_mode".into()
                },
                RefKind::System {
                    global: true,
                    name: "wait_timeout".into()
                },
                RefKind::User { name: "x".into() },
                RefKind::User { name: "my var".into() },
            ]
        );
        assert_eq!(&sql[refs[0].start..refs[0].end], "@@session.sql_mode");
        // An account name and a PostgreSQL operator are not variables.
        assert!(refs_of("CREATE USER 'u'@'localhost' IDENTIFIED BY 'p'").is_empty());
        assert!(refs_of("SELECT a @@ b, c <@ d, e @> f FROM t").is_empty());
        assert!(refs_of("SELECT v @@to_tsquery('x') FROM t").is_empty());
    }

    #[test]
    fn a_select_of_variables_only_is_recognised_with_its_column_names() {
        let sql = "SELECT @@session.auto_increment_increment AS auto_increment_increment, @@version, @x `v` LIMIT 1";
        let view = code_view(sql);
        let refs = find_var_refs(sql, &view);
        let items = variables_only_select(sql, &view, &refs).unwrap();
        let names: Vec<&str> = items.iter().map(|(_, n)| n.as_str()).collect();
        assert_eq!(names, ["auto_increment_increment", "@@version", "v"]);

        for other in ["SELECT @@version, 1", "SELECT @@version FROM t", "SELECT CONCAT(@@version, 'x')", "INSERT INTO t VALUES (@x)"] {
            let view = code_view(other);
            let refs = find_var_refs(other, &view);
            assert!(variables_only_select(other, &view, &refs).is_none(), "{other}");
        }
    }

    #[test]
    fn like_is_case_insensitive_with_both_wildcards() {
        assert!(like_match("wait_timeout", "WAIT%"));
        assert!(like_match("wait_timeout", "wait_timeou_"));
        assert!(like_match("wait_timeout", "wait\\_timeout"));
        assert!(!like_match("waitXtimeout", "wait\\_timeout"));
        assert!(!like_match("wait_timeout", "timeout"));
        assert!(like_match("x", "%"));
    }

    #[test]
    fn show_filters_are_read() {
        assert_eq!(parse_show_filter("").unwrap(), ShowFilter::All);
        assert_eq!(parse_show_filter("LIKE 'sql\\_mode'").unwrap(), ShowFilter::Like("sql_mode".into()));
        assert_eq!(
            parse_show_filter("WHERE Variable_name = 'a' OR variable_name='b'").unwrap(),
            ShowFilter::Names(vec!["a".into(), "b".into()])
        );
        assert_eq!(
            parse_show_filter("WHERE Variable_name IN ('a','b')").unwrap(),
            ShowFilter::Names(vec!["a".into(), "b".into()])
        );
        assert_eq!(
            parse_show_filter("WHERE Variable_name LIKE 'char%'").unwrap(),
            ShowFilter::Like("char%".into())
        );
        assert_eq!(parse_show_filter("WHERE Value > 3").unwrap_err().code, 1235);
        assert_eq!(parse_show_filter("FROM x").unwrap_err().code, 1064);
    }

    #[test]
    fn literals_round_trip() {
        assert_eq!(sql_literal(&Value::String("it's".into())), "'it''s'");
        assert_eq!(sql_literal(&Value::Null), "NULL");
        assert_eq!(sql_literal(&Value::Int8(-3)), "-3");
        assert_eq!(unquote_word("'a\\'b''c'"), "a'b'c");
        assert!(is_single_literal("'a''b'"));
        assert!(!is_single_literal("'a' || 'b'"));
    }
}
