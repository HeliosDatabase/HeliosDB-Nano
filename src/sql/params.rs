//! ONE parameter registry behind every way of reading a setting.
//!
//! `SHOW <name>`, `SHOW ALL`, `pg_settings`, `current_setting('<name>')` and the
//! PostgreSQL startup `ParameterStatus` burst used to answer from five separate
//! tables, and they disagreed: `server_version` alone had three answers
//! (`16.0 (HeliosDB Nano x)`, `17.0` and the crate version). Every reader now
//! goes through [`PARAMS`] for the list of names and [`current_value`] for the
//! value, so a name is known everywhere or nowhere and has one value.
//!
//! Where a value is STORED is unchanged:
//!
//! * a user-settable GUC lives on the session (`SessionScopedState`), read
//!   through [`crate::session_guc_tls`], falling back to the registry default;
//! * a server-level parameter lives in the handle's [`SessionSettings`], which
//!   the engine publishes here by storage-engine instance so the storage-less
//!   evaluator can read it ([`publish_server_settings`]);
//! * a handful of compatibility names have a fixed answer, because this engine
//!   does not vary them (it always speaks UTF8, reads ISO dates, runs READ
//!   COMMITTED). `SET` of those is accepted and shadowed, exactly as before.

use super::settings::{default_value, is_server_level, is_user_settable, render_value, SessionSettings};
use std::collections::HashMap;
use std::sync::{OnceLock, RwLock, Weak};

/// The PostgreSQL major version this server reports, on every surface.
pub const PG_COMPAT_VERSION: &str = "16.0";
/// `server_version_num` for [`PG_COMPAT_VERSION`].
pub const PG_COMPAT_VERSION_NUM: &str = "160000";

/// `server_version` as every surface reports it: `16.0 (HeliosDB Nano 4.42.0)`.
pub fn pg_server_version_string() -> String {
    format!("{PG_COMPAT_VERSION} (HeliosDB Nano {})", env!("CARGO_PKG_VERSION"))
}

/// `version()`: `PostgreSQL 16.0 (HeliosDB Nano 4.42.0)`.
pub fn pg_version_function_string() -> String {
    format!("PostgreSQL {}", pg_server_version_string())
}

/// Who may change a parameter, in `pg_settings.context` terms.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParamContext {
    /// Set per session by any client.
    User,
    /// Set for the whole server; this engine accepts `SET` of it on the
    /// embedded path but applies it to every session of the handle.
    Superuser,
    /// Fixed when the server starts.
    Postmaster,
    /// A compile-time or computed fact; cannot be set.
    Internal,
}

impl ParamContext {
    /// The `pg_settings.context` spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            ParamContext::User => "user",
            ParamContext::Superuser => "superuser",
            ParamContext::Postmaster => "postmaster",
            ParamContext::Internal => "internal",
        }
    }
}

/// One known parameter.
#[derive(Debug, Clone, Copy)]
pub struct ParamDef {
    /// Lower-case name.
    pub name: &'static str,
    /// `pg_settings.category`.
    pub category: &'static str,
    /// `pg_settings.short_desc` and the `SHOW ALL` description. A parameter that
    /// is stored but read by nothing says so here.
    pub short_desc: &'static str,
    /// `pg_settings.context`.
    pub context: ParamContext,
    /// `pg_settings.vartype`: bool, integer, string, enum.
    pub vartype: &'static str,
    /// `pg_settings.unit`.
    pub unit: Option<&'static str>,
}

const fn p(
    name: &'static str,
    category: &'static str,
    short_desc: &'static str,
    context: ParamContext,
    vartype: &'static str,
    unit: Option<&'static str>,
) -> ParamDef {
    ParamDef {
        name,
        category,
        short_desc,
        context,
        vartype,
        unit,
    }
}

use ParamContext::{Internal, Postmaster, Superuser, User};

const NO_EFFECT: &str = " (accepted; nothing in this engine reads it yet)";

/// Every parameter this server knows, sorted by name. `SHOW ALL` and
/// `pg_settings` list exactly these rows.
pub static PARAMS: &[ParamDef] = &[
    p(
        "application_name",
        "Reporting and Logging / What to Log",
        "Sets the application name to be reported in statistics and logs.",
        User,
        "string",
        None,
    ),
    p(
        "authentication_timeout",
        "Connections and Authentication / Authentication",
        "Sets the maximum allowed time to complete client authentication.",
        Postmaster,
        "integer",
        Some("ms"),
    ),
    p(
        "bulk_load_mode",
        "HeliosDB",
        "Defers index and constraint maintenance for bulk inserts on this session.",
        User,
        "bool",
        None,
    ),
    p(
        "client_encoding",
        "Client Connection Defaults / Locale and Formatting",
        "Sets the client's character set encoding (always UTF8 here).",
        User,
        "string",
        None,
    ),
    p(
        "compression_level",
        "HeliosDB",
        "Compression level of the default codec.",
        Superuser,
        "integer",
        None,
    ),
    p(
        "datestyle",
        "Client Connection Defaults / Locale and Formatting",
        "Sets the display format for date and time values (always ISO, MDY here).",
        User,
        "string",
        None,
    ),
    p(
        "default_compression",
        "HeliosDB",
        "Default on-disk compression codec.",
        Superuser,
        "enum",
        None,
    ),
    p(
        "default_transaction_isolation",
        "Client Connection Defaults / Statement Behavior",
        "Sets the transaction isolation level of each new transaction.",
        User,
        "enum",
        None,
    ),
    p(
        "default_transaction_read_only",
        "Client Connection Defaults / Statement Behavior",
        "Sets the default read-only status of new transactions.",
        User,
        "bool",
        None,
    ),
    p(
        "enable_hashjoin",
        "Query Tuning / Planner Method Configuration",
        "Enables the planner's use of hash join plans.",
        User,
        "bool",
        None,
    ),
    p(
        "enable_indexscan",
        "Query Tuning / Planner Method Configuration",
        "Enables the planner's use of index-scan plans.",
        User,
        "bool",
        None,
    ),
    p(
        "enable_mergejoin",
        "Query Tuning / Planner Method Configuration",
        "Enables the planner's use of merge join plans.",
        User,
        "bool",
        None,
    ),
    p(
        "enable_nestloop",
        "Query Tuning / Planner Method Configuration",
        "Enables the planner's use of nested-loop join plans.",
        User,
        "bool",
        None,
    ),
    p(
        "enable_seqscan",
        "Query Tuning / Planner Method Configuration",
        "Enables the planner's use of sequential-scan plans.",
        User,
        "bool",
        None,
    ),
    p(
        "hnsw_ef_construction",
        "HeliosDB",
        "HNSW build-time candidate list size.",
        User,
        "integer",
        None,
    ),
    p(
        "hnsw_m",
        "HeliosDB",
        "HNSW maximum neighbours per node.",
        User,
        "integer",
        None,
    ),
    p(
        "idle_in_transaction_session_timeout",
        "Client Connection Defaults / Statement Behavior",
        "Sets the maximum allowed idle time between queries, when in a transaction.",
        User,
        "integer",
        Some("ms"),
    ),
    p(
        "idle_session_timeout",
        "Client Connection Defaults / Statement Behavior",
        "Sets the maximum allowed idle time between queries, when not in a transaction.",
        User,
        "integer",
        Some("ms"),
    ),
    p(
        "in_hot_standby",
        "Preset Options",
        "Shows whether hot standby is currently active.",
        Internal,
        "bool",
        None,
    ),
    p(
        "integer_datetimes",
        "Preset Options",
        "Shows whether datetimes are integer based.",
        Internal,
        "bool",
        None,
    ),
    p(
        "is_superuser",
        "Preset Options",
        "Shows whether the current user is a superuser.",
        Internal,
        "bool",
        None,
    ),
    p(
        "lc_collate",
        "Preset Options",
        "Shows the collation order locale.",
        Internal,
        "string",
        None,
    ),
    p(
        "lc_ctype",
        "Preset Options",
        "Shows the character classification and case conversion locale.",
        Internal,
        "string",
        None,
    ),
    p(
        "lc_messages",
        "Client Connection Defaults / Locale and Formatting",
        "Sets the language in which messages are displayed (always C here).",
        User,
        "string",
        None,
    ),
    p(
        "lc_monetary",
        "Client Connection Defaults / Locale and Formatting",
        "Sets the locale for formatting monetary amounts (always C here).",
        User,
        "string",
        None,
    ),
    p(
        "lc_numeric",
        "Client Connection Defaults / Locale and Formatting",
        "Sets the locale for formatting numbers (always C here).",
        User,
        "string",
        None,
    ),
    p(
        "lc_time",
        "Client Connection Defaults / Locale and Formatting",
        "Sets the locale for formatting date and time values (always C here).",
        User,
        "string",
        None,
    ),
    p(
        "lock_timeout",
        "Client Connection Defaults / Statement Behavior",
        "Sets the maximum allowed duration of any wait for a lock (0 = the server's bound).",
        User,
        "integer",
        Some("ms"),
    ),
    p(
        "max_connections",
        "Connections and Authentication / Connection Settings",
        "Sets the maximum number of concurrent connections.",
        Postmaster,
        "integer",
        None,
    ),
    p(
        "mv_auto_refresh",
        "HeliosDB",
        "Materialized-view background refresh switch.",
        Superuser,
        "bool",
        None,
    ),
    p(
        "mv_max_cpu_percent",
        "HeliosDB",
        "CPU budget of the materialized-view refresher.",
        Superuser,
        "integer",
        None,
    ),
    p(
        "optimizer",
        "Query Tuning",
        "Enables the cost-based optimizer.",
        User,
        "bool",
        None,
    ),
    p(
        "query_timeout",
        "Client Connection Defaults / Statement Behavior",
        "Sets the maximum allowed duration of any statement (alias of statement_timeout).",
        User,
        "integer",
        Some("ms"),
    ),
    p(
        "row_security",
        "Client Connection Defaults / Statement Behavior",
        "Enables row security; off makes a query that a policy would filter fail instead.",
        User,
        "bool",
        None,
    ),
    p(
        "search_path",
        "Client Connection Defaults / Statement Behavior",
        "Sets the schema search order for names that are not schema-qualified.",
        User,
        "string",
        None,
    ),
    p(
        "server_encoding",
        "Preset Options",
        "Shows the server (database) character set encoding.",
        Internal,
        "string",
        None,
    ),
    p(
        "server_version",
        "Preset Options",
        "Shows the server version.",
        Internal,
        "string",
        None,
    ),
    p(
        "server_version_num",
        "Preset Options",
        "Shows the server version as an integer.",
        Internal,
        "integer",
        None,
    ),
    p(
        "session_authorization",
        "Preset Options",
        "Shows the session user name.",
        Internal,
        "string",
        None,
    ),
    p(
        "shared_buffers",
        "Resource Usage / Memory",
        "Sets the number of shared memory buffers used by the server.",
        Postmaster,
        "integer",
        Some("kB"),
    ),
    p(
        "smfi_bulk_load_threshold",
        "HeliosDB",
        "Row count above which SMFI treats an insert as a bulk load.",
        Superuser,
        "integer",
        None,
    ),
    p(
        "smfi_delta_threshold",
        "HeliosDB",
        "SMFI delta size that triggers a merge.",
        Superuser,
        "integer",
        None,
    ),
    p(
        "smfi_enabled",
        "HeliosDB",
        "Self-maintaining filter index switch.",
        Superuser,
        "bool",
        None,
    ),
    p(
        "smfi_max_cpu_percent",
        "HeliosDB",
        "CPU budget of the SMFI maintainer.",
        Superuser,
        "integer",
        None,
    ),
    p(
        "smfi_max_workers",
        "HeliosDB",
        "Worker count of the SMFI maintainer.",
        Superuser,
        "integer",
        None,
    ),
    p(
        "smfi_parallel_enabled",
        "HeliosDB",
        "SMFI parallel maintenance switch.",
        Superuser,
        "bool",
        None,
    ),
    p(
        "smfi_parallel_threshold",
        "HeliosDB",
        "Row count above which SMFI maintains in parallel.",
        Superuser,
        "integer",
        None,
    ),
    p(
        "smfi_tracking_enabled",
        "HeliosDB",
        "SMFI change tracking switch.",
        Superuser,
        "bool",
        None,
    ),
    p(
        "standard_conforming_strings",
        "Version and Platform Compatibility",
        "Causes '...' strings to treat backslashes literally.",
        Internal,
        "bool",
        None,
    ),
    p(
        "statement_timeout",
        "Client Connection Defaults / Statement Behavior",
        "Sets the maximum allowed duration of any statement.",
        User,
        "integer",
        Some("ms"),
    ),
    p(
        "time_travel_enabled",
        "HeliosDB",
        "Version-history retention switch.",
        Superuser,
        "bool",
        None,
    ),
    p(
        "timezone",
        "Client Connection Defaults / Locale and Formatting",
        "Sets the time zone for displaying and interpreting time stamps (always UTC here).",
        User,
        "string",
        None,
    ),
    p(
        "transaction_isolation",
        "Client Connection Defaults / Statement Behavior",
        "Sets the current transaction's isolation level.",
        User,
        "enum",
        None,
    ),
    p(
        "transaction_read_only",
        "Client Connection Defaults / Statement Behavior",
        "Sets the current transaction's read-only status.",
        User,
        "bool",
        None,
    ),
    p(
        "vector_index_type",
        "HeliosDB",
        "Default vector index type.",
        User,
        "enum",
        None,
    ),
    p(
        "work_mem",
        "Resource Usage / Memory",
        "Sets the maximum memory to be used for query workspaces.",
        User,
        "integer",
        Some("kB"),
    ),
];

/// Server-level parameters that are stored and shown but that no engine code
/// reads. `pg_settings.short_desc` and `SHOW ALL` say so instead of implying an
/// effect.
pub fn has_no_effect(name: &str) -> bool {
    matches!(
        name,
        "shared_buffers"
            | "compression_level"
            | "default_compression"
            | "time_travel_enabled"
            | "mv_auto_refresh"
            | "mv_max_cpu_percent"
            | "smfi_enabled"
            | "smfi_tracking_enabled"
            | "smfi_bulk_load_threshold"
            | "smfi_parallel_enabled"
            | "smfi_max_cpu_percent"
            | "smfi_delta_threshold"
            | "smfi_parallel_threshold"
            | "smfi_max_workers"
            | "hnsw_ef_construction"
            | "hnsw_m"
            | "vector_index_type"
            | "work_mem"
            | "enable_hashjoin"
            | "enable_indexscan"
            | "enable_mergejoin"
            | "enable_nestloop"
            | "enable_seqscan"
    )
}

/// The definition of `name`, if this server knows it.
pub fn find(name: &str) -> Option<&'static ParamDef> {
    PARAMS
        .binary_search_by(|def| def.name.cmp(name))
        .ok()
        .and_then(|i| PARAMS.get(i))
}

/// The description shown by `SHOW ALL` and `pg_settings.short_desc`.
pub fn description(def: &ParamDef) -> String {
    if has_no_effect(def.name) {
        format!("{}{NO_EFFECT}", def.short_desc)
    } else {
        def.short_desc.to_string()
    }
}

/// Answers that do not depend on any session or server state: the version,
/// the encodings, the locale and the other compatibility names whose value this
/// engine does not vary. These win over a session `SET` of the same name, which
/// is accepted and shadowed (reporting the shadow as though it were in force
/// would be a lie).
pub fn fixed_value(name: &str) -> Option<String> {
    let value = match name {
        "server_version" => return Some(pg_server_version_string()),
        "server_version_num" => PG_COMPAT_VERSION_NUM,
        "server_encoding" | "client_encoding" => "UTF8",
        "lc_collate" | "lc_ctype" | "lc_messages" | "lc_monetary" | "lc_numeric" | "lc_time" => "C",
        "datestyle" => "ISO, MDY",
        "timezone" => "UTC",
        "standard_conforming_strings" | "integer_datetimes" | "is_superuser" => "on",
        "transaction_isolation" | "default_transaction_isolation" => "read committed",
        "transaction_read_only" | "default_transaction_read_only" => {
            return Some(on_off(
                crate::standby_posture::in_recovery(None) || crate::standby_posture::is_fenced(),
            ))
        }
        "in_hot_standby" => return Some(on_off(crate::standby_posture::in_recovery(None))),
        _ => return None,
    };
    Some(value.to_string())
}

fn on_off(on: bool) -> String {
    if on { "on" } else { "off" }.to_string()
}

// ---------------------------------------------------------------------------
// Server-level values, by storage-engine instance
// ---------------------------------------------------------------------------

type ServerMap = RwLock<HashMap<u64, Weak<RwLock<HashMap<String, super::settings::SettingValue>>>>>;

fn server_map() -> &'static ServerMap {
    static MAP: OnceLock<ServerMap> = OnceLock::new();
    MAP.get_or_init(|| RwLock::new(HashMap::new()))
}

/// Publish the handle's server-level registry for the engine `instance`, so the
/// storage-less evaluator (`current_setting`) and the system views read the
/// value `SHOW` reads. Held weakly: a closed database leaves nothing behind but
/// a dead entry, pruned on the next publish.
pub fn publish_server_settings(instance: u64, settings: &SessionSettings) {
    if let Ok(mut map) = server_map().write() {
        map.retain(|_, weak| weak.strong_count() > 0);
        map.insert(instance, settings.downgrade());
    }
}

/// The server-level value of `name` for the engine the calling statement runs
/// on, else the registry default.
fn server_value(name: &str) -> Option<super::settings::SettingValue> {
    let instance = crate::session_scoped_state_tls().map(|state| state.engine_instance());
    if let Some(instance) = instance {
        let strong = server_map()
            .read()
            .ok()
            .and_then(|map| map.get(&instance).and_then(Weak::upgrade));
        if let Some(strong) = strong {
            if let Some(value) = strong.read().ok().and_then(|m| m.get(name).cloned()) {
                return Some(value);
            }
        }
    }
    default_value(name)
}

/// Was `name` changed from its default for the calling statement (`pg_settings.source`)?
fn source_of(name: &str) -> &'static str {
    if is_user_settable(name) && crate::session_guc_tls(name).is_some() {
        return "session";
    }
    if is_server_level(name) {
        if let (Some(now), Some(default)) = (server_value(name), default_value(name)) {
            if now != default {
                return "override";
            }
        }
    }
    "default"
}

/// THE value of `name` for the statement running on this thread, rendered the
/// way `SHOW` prints it. `None`: not a parameter this server knows.
///
/// The engine's embedded `SHOW`, `SHOW ALL`, `pg_settings` and
/// `current_setting()` all read through here.
pub fn current_value(name: &str) -> Option<String> {
    if let Some(fixed) = fixed_value(name) {
        return Some(fixed);
    }
    match name {
        "application_name" => {
            return Some(
                crate::session_scoped_state_tls()
                    .map(|state| state.application_name())
                    .unwrap_or_default(),
            )
        }
        "session_authorization" => {
            return Some(crate::privileges::access::session_user_name())
        }
        "search_path" => {
            let mut path = crate::session_search_path_tls();
            return Some(if path.is_empty() {
                "\"$user\", public".to_string()
            } else {
                if !path.iter().any(|schema| schema == "public") {
                    path.push("public".to_string());
                }
                path.join(", ")
            });
        }
        _ => {}
    }
    if is_user_settable(name) {
        return crate::session_guc_tls(name)
            .or_else(|| default_value(name))
            .as_ref()
            .map(render_value);
    }
    if is_server_level(name) || super::settings::is_registered(name) {
        return server_value(name).as_ref().map(render_value);
    }
    None
}

/// Is row security on for the statement running on this thread (default on)?
pub fn row_security_enabled() -> bool {
    crate::session_guc_tls("row_security")
        .and_then(|value| value.as_bool())
        .unwrap_or(true)
}

/// `SHOW ALL`: `(name, setting, description)` for every known parameter, with
/// the value resolved by `value_of` (the engine passes [`current_value`]; the
/// PostgreSQL listener passes its per-connection resolver so `SHOW ALL` and
/// `SHOW <name>` cannot disagree on the wire).
pub fn show_all_rows(mut value_of: impl FnMut(&str) -> Option<String>) -> Vec<(String, String, String)> {
    PARAMS
        .iter()
        .map(|def| {
            (
                def.name.to_string(),
                value_of(def.name).unwrap_or_default(),
                description(def),
            )
        })
        .collect()
}

/// The `pg_settings` column names, in PostgreSQL's order.
pub const PG_SETTINGS_COLUMNS: &[&str] = &[
    "name",
    "setting",
    "unit",
    "category",
    "short_desc",
    "context",
    "vartype",
    "source",
    "min_val",
    "max_val",
    "enumvals",
    "boot_val",
    "reset_val",
    "sourcefile",
    "pending_restart",
];

/// One `pg_settings` row per known parameter, every column a string except
/// `pending_restart` (bool) and the NULL ones, as `crate::Value`s.
pub fn pg_settings_rows() -> Vec<Vec<crate::Value>> {
    use crate::Value;
    let text = |s: &str| Value::String(s.to_string());
    let opt = |s: Option<&str>| s.map_or(Value::Null, |s| Value::String(s.to_string()));
    PARAMS
        .iter()
        .map(|def| {
            let setting = current_value(def.name).unwrap_or_default();
            let boot = fixed_value(def.name)
                .or_else(|| default_value(def.name).as_ref().map(render_value))
                .unwrap_or_else(|| setting.clone());
            vec![
                text(def.name),
                Value::String(setting),
                opt(def.unit),
                text(def.category),
                Value::String(description(def)),
                text(def.context.as_str()),
                text(def.vartype),
                text(source_of(def.name)),
                Value::Null,
                Value::Null,
                Value::Null,
                Value::String(boot.clone()),
                Value::String(boot),
                Value::Null,
                Value::Boolean(false),
            ]
        })
        .collect()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]
mod tests {
    use super::*;

    #[test]
    fn params_are_sorted_and_unique() {
        for pair in PARAMS.windows(2) {
            assert!(pair[0].name < pair[1].name, "{} / {}", pair[0].name, pair[1].name);
        }
    }

    #[test]
    fn every_registered_parameter_is_listed() {
        for name in super::super::settings::REGISTERED_PARAMETERS {
            assert!(find(name).is_some(), "{name} is registered but not in PARAMS");
            assert!(current_value(name).is_some(), "{name} has no value");
        }
        for def in PARAMS {
            assert!(current_value(def.name).is_some(), "{} has no value", def.name);
        }
    }

    #[test]
    fn server_version_is_one_string() {
        assert_eq!(current_value("server_version"), Some(pg_server_version_string()));
        assert!(pg_version_function_string().contains(&pg_server_version_string()));
        assert_eq!(current_value("server_version_num").as_deref(), Some("160000"));
        assert!(pg_server_version_string().starts_with(PG_COMPAT_VERSION));
    }

    #[test]
    fn row_security_defaults_on() {
        assert_eq!(current_value("row_security").as_deref(), Some("on"));
        assert!(row_security_enabled());
    }
}
