//! SQL SET and SHOW command implementation.
//!
//! # What lives here, and what does NOT — sprinter a3077a3f68d8
//!
//! [`SessionSettings`] is ONE registry per `EmbeddedDatabase` handle. Its name
//! was a lie: every connection shared it, so `SET statement_timeout = 1` from
//! any authenticated client cancelled EVERY other connection's queries after
//! 1 ms, and `SET bulk_load_mode = on` flipped the storage engine's flag for the
//! whole process. On a multi-tenant or shared server that is a denial-of-service
//! lever handed to any client that can type `SET`.
//!
//! So this registry now holds **defaults and genuine SERVER-level parameters
//! only** ([`is_server_level`]). A parameter is server-level iff *its value is
//! consumed by process-wide machinery on behalf of sessions other than the one
//! that set it* — the buffer pool, the on-disk compression codec, the
//! materialized-view refresher, the SMFI index maintainer, the version-retention
//! switch. Everything else is [`is_user_settable`]: it is stored on the session
//! (`crate::session::scoped::SessionScopedState`, the same place GH#28 put the
//! connection-lifetime timeouts and sprinter f4f5d450e816 put
//! `application_name`) and read back from there, so one connection's `SET` can
//! never reach another's.
//!
//! The registry is still the place a name is DECLARED — an entry here is what
//! makes `SHOW <name>` / `RESET <name>` something other than "unrecognized
//! configuration parameter", and it supplies the default a `RESET` falls back
//! to — and it is still where a value is validated. It is simply no longer where
//! a user-settable value is STORED.

use crate::{Error, Result};
use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::Duration;

/// Setting value types
#[derive(Debug, Clone, PartialEq)]
pub enum SettingValue {
    /// Boolean value (on/off, true/false, yes/no, 1/0)
    Boolean(bool),
    /// Integer value
    Integer(i64),
    /// String value
    String(String),
    /// Duration in milliseconds
    Duration(u64),
}

impl SettingValue {
    /// Convert to boolean
    pub fn as_bool(&self) -> Option<bool> {
        match self {
            SettingValue::Boolean(b) => Some(*b),
            SettingValue::Integer(i) => Some(*i != 0),
            SettingValue::String(s) => match s.to_lowercase().as_str() {
                "on" | "true" | "yes" | "1" => Some(true),
                "off" | "false" | "no" | "0" => Some(false),
                _ => None,
            },
            _ => None,
        }
    }

    /// Convert to integer
    pub fn as_i64(&self) -> Option<i64> {
        match self {
            SettingValue::Integer(i) => Some(*i),
            SettingValue::Boolean(b) => Some(if *b { 1 } else { 0 }),
            SettingValue::String(s) => s.parse().ok(),
            SettingValue::Duration(d) => Some(*d as i64),
        }
    }

    /// Convert to string
    pub fn as_string(&self) -> String {
        match self {
            SettingValue::Boolean(b) => if *b { "on" } else { "off" }.to_string(),
            SettingValue::Integer(i) => i.to_string(),
            SettingValue::String(s) => s.clone(),
            SettingValue::Duration(d) => format!("{}ms", d),
        }
    }

    /// Convert to duration (milliseconds)
    pub fn as_duration_ms(&self) -> Option<u64> {
        match self {
            SettingValue::Duration(d) => Some(*d),
            SettingValue::Integer(i) => Some(*i as u64),
            SettingValue::String(s) => s.parse().ok(),
            _ => None,
        }
    }
}

/// Parse setting value from string
pub fn parse_setting_value(s: &str) -> SettingValue {
    // Try boolean
    match s.to_lowercase().as_str() {
        "on" | "true" | "yes" => return SettingValue::Boolean(true),
        "off" | "false" | "no" => return SettingValue::Boolean(false),
        _ => {}
    }

    // Try integer
    if let Ok(i) = s.parse::<i64>() {
        return SettingValue::Integer(i);
    }

    // Default to string
    SettingValue::String(s.to_string())
}

/// sprinter a3077a3f68d8: the parameters that are genuinely SERVER-level.
///
/// The rule (see the module docs): a parameter is server-level iff its value is
/// consumed by process-wide machinery **on behalf of sessions other than the one
/// that set it**. A per-session value for these would either be silently ignored
/// or — worse — would let one connection reconfigure shared machinery, which is
/// the very defect this split exists to close.
///
/// Group by group, with the reason each one is here:
///
/// * `server_version`, `server_encoding`, `max_connections`, `port`,
///   `authentication_timeout` — postmaster-scoped in PostgreSQL too, and
///   already refused by `SessionSettings::is_read_only`. A session that could
///   lengthen its own authentication window is a security regression (GH#28).
/// * `shared_buffers` — there is ONE buffer pool in the process. PostgreSQL
///   also makes it postmaster context.
/// * `default_compression`, `compression_level` — the on-disk encoding of
///   shared blocks; background compaction re-encodes other sessions' data with
///   whatever this says.
/// * `time_travel_enabled` — governs whether the storage engine RETAINS version
///   history. Per-session retention is incoherent: the rows either exist for
///   everyone or for no one.
/// * `mv_auto_refresh`, `mv_max_cpu_percent` — drive the single background
///   materialized-view refresher, which serves every session.
/// * `smfi_*` — the Self-Maintaining Filter Index maintainer and its worker
///   pool. The indexes are shared structures; a per-session threshold would
///   mean one connection's inserts were tracked and another's were not, and the
///   index would be wrong for both.
///
/// Everything else registered in [`SessionSettings::new`] is user-settable.
pub fn is_server_level(name: &str) -> bool {
    matches!(
        name,
        "server_version"
            | "server_encoding"
            | "max_connections"
            | "port"
            | "authentication_timeout"
            | "shared_buffers"
            | "default_compression"
            | "compression_level"
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
    )
}

/// sprinter a3077a3f68d8: true for a registered parameter a client may set on
/// its OWN session — i.e. everything in the registry that is not
/// [`is_server_level`].
///
/// `application_name` is deliberately EXCLUDED: it is session state too, but it
/// has its own dedicated slot and its own `SET`/`SHOW`/`RESET` interceptor
/// (sprinter f4f5d450e816), which runs first. Routing it through the generic
/// overlay as well would give it two homes that could disagree.
///
/// The three GH#28 connection-lifetime GUCs are excluded for the same reason:
/// they live on `Session` (the listener's idle timers read them there) and have
/// their own interceptor, `try_handle_session_timeout_guc`.
pub fn is_user_settable(name: &str) -> bool {
    is_registered(name)
        && !is_server_level(name)
        && name != "application_name"
        && name != "idle_session_timeout"
        && name != "idle_in_transaction_session_timeout"
}

/// Every parameter [`SessionSettings::new`] declares, as a static list.
///
/// sprinter a3077a3f68d8 needs "is this a parameter this server knows?" as a
/// PURE predicate: both wire listeners classify a `SET` / `SHOW` / `RESET`
/// before any handle is in reach (`session_show_parameter_name` is an
/// associated fn on the PostgreSQL handler), and an unknown name must keep
/// falling through to PostgreSQL's `42704 unrecognized configuration
/// parameter`, not become a silently-accepted session override.
///
/// Kept in step with the constructor by `registered_list_matches_the_registry`.
pub const REGISTERED_PARAMETERS: &[&str] = &[
    "application_name",
    "authentication_timeout",
    "bulk_load_mode",
    "client_encoding",
    "compression_level",
    "datestyle",
    "default_compression",
    "enable_hashjoin",
    "enable_indexscan",
    "enable_mergejoin",
    "enable_nestloop",
    "enable_seqscan",
    "hnsw_ef_construction",
    "hnsw_m",
    "idle_in_transaction_session_timeout",
    "idle_session_timeout",
    "mv_auto_refresh",
    "mv_max_cpu_percent",
    "optimizer",
    "query_timeout",
    "server_encoding",
    "server_version",
    "shared_buffers",
    "smfi_bulk_load_threshold",
    "smfi_delta_threshold",
    "smfi_enabled",
    "smfi_max_cpu_percent",
    "smfi_max_workers",
    "smfi_parallel_enabled",
    "smfi_parallel_threshold",
    "smfi_tracking_enabled",
    "statement_timeout",
    "time_travel_enabled",
    "timezone",
    "transaction_isolation",
    "transaction_read_only",
    "vector_index_type",
    "work_mem",
];

/// The registry DEFAULTS, built once.
///
/// sprinter a3077a3f68d8: the per-session overlay only holds what a session has
/// actually `SET`, so every reader needs the default to fall back to — and
/// `SessionSettings::new()` allocates the whole map, which is fine at open but
/// not on a `SHOW` / `current_setting()` path.
fn defaults() -> &'static HashMap<String, SettingValue> {
    static DEFAULTS: std::sync::OnceLock<HashMap<String, SettingValue>> = std::sync::OnceLock::new();
    DEFAULTS.get_or_init(|| SessionSettings::new().get_all())
}

/// The value a registered parameter has when no session has overridden it.
pub fn default_value(name: &str) -> Option<SettingValue> {
    defaults().get(name).cloned()
}

/// Render a setting value the way PostgreSQL's `SHOW` / `current_setting()` do.
///
/// Durations go through the ONE formatter GH#28 already uses for the
/// connection-lifetime GUCs (`0`, `30s`, `10min`, `250ms`), so
/// `SHOW statement_timeout` answers `0` rather than this module's internal `0ms`
/// spelling and the two families cannot drift.
pub fn render_value(value: &SettingValue) -> String {
    match value {
        SettingValue::Duration(ms) => crate::protocol::postgres::timeouts::format_guc_duration_ms(*ms),
        other => other.as_string(),
    }
}

/// True when `name` is a parameter this server declares — the gate that keeps an
/// unknown `SET x = 1` falling through to the planner (and an unknown
/// `SHOW x` to 42704) exactly as before this item.
pub fn is_registered(name: &str) -> bool {
    REGISTERED_PARAMETERS.contains(&name)
}

/// Session settings manager
#[derive(Debug, Clone)]
pub struct SessionSettings {
    settings: Arc<RwLock<HashMap<String, SettingValue>>>,
}

impl SessionSettings {
    /// Create new session settings with defaults
    pub fn new() -> Self {
        let mut settings = HashMap::new();

        // Query execution settings
        settings.insert("statement_timeout".to_string(), SettingValue::Duration(0)); // 0 = unlimited
        settings.insert("query_timeout".to_string(), SettingValue::Duration(0)); // 0 = unlimited

        // GH#28: connection-lifetime GUCs (PostgreSQL names and defaults).
        // Registered so the embedded / params executor families answer
        // `SHOW` and validate `SET`; the wire listeners enforce the timeouts
        // from their per-connection policy, never from this process-global
        // registry (see `crate::protocol::postgres::timeouts`).
        settings.insert("idle_session_timeout".to_string(), SettingValue::Duration(0)); // 0 = disabled
        settings.insert(
            "idle_in_transaction_session_timeout".to_string(),
            SettingValue::Duration(0),
        ); // 0 = disabled
        settings.insert("authentication_timeout".to_string(), SettingValue::Duration(60_000)); // server-scoped

        // Optimizer settings
        settings.insert("optimizer".to_string(), SettingValue::Boolean(true));
        settings.insert("enable_seqscan".to_string(), SettingValue::Boolean(true));
        settings.insert("enable_indexscan".to_string(), SettingValue::Boolean(true));
        settings.insert("enable_hashjoin".to_string(), SettingValue::Boolean(true));
        settings.insert("enable_mergejoin".to_string(), SettingValue::Boolean(true));
        settings.insert("enable_nestloop".to_string(), SettingValue::Boolean(true));

        // Memory settings
        settings.insert("work_mem".to_string(), SettingValue::Integer(4096)); // KB
        settings.insert("shared_buffers".to_string(), SettingValue::Integer(131072)); // KB (128MB)

        // Transaction settings
        settings.insert(
            "transaction_isolation".to_string(),
            SettingValue::String("READ COMMITTED".to_string()),
        );
        settings.insert("transaction_read_only".to_string(), SettingValue::Boolean(false));

        // Time-travel settings
        settings.insert("time_travel_enabled".to_string(), SettingValue::Boolean(true));

        // Compression settings
        settings.insert(
            "default_compression".to_string(),
            SettingValue::String("zstd".to_string()),
        );
        settings.insert("compression_level".to_string(), SettingValue::Integer(3));

        // Vector settings
        settings.insert(
            "vector_index_type".to_string(),
            SettingValue::String("hnsw".to_string()),
        );
        settings.insert("hnsw_ef_construction".to_string(), SettingValue::Integer(200));
        settings.insert("hnsw_m".to_string(), SettingValue::Integer(16));

        // Materialized view settings
        settings.insert("mv_auto_refresh".to_string(), SettingValue::Boolean(false));
        settings.insert("mv_max_cpu_percent".to_string(), SettingValue::Integer(15));

        // SMFI (Self-Maintaining Filter Index) settings
        settings.insert("smfi_enabled".to_string(), SettingValue::Boolean(true));
        settings.insert("smfi_tracking_enabled".to_string(), SettingValue::Boolean(true));
        settings.insert("smfi_bulk_load_threshold".to_string(), SettingValue::Integer(10000));

        // Bulk loading performance settings
        settings.insert("bulk_load_mode".to_string(), SettingValue::Boolean(false));
        settings.insert("smfi_parallel_enabled".to_string(), SettingValue::Boolean(true));
        settings.insert("smfi_max_cpu_percent".to_string(), SettingValue::Integer(15));
        settings.insert("smfi_delta_threshold".to_string(), SettingValue::Integer(1000));
        settings.insert("smfi_parallel_threshold".to_string(), SettingValue::Integer(10000));
        settings.insert("smfi_max_workers".to_string(), SettingValue::Integer(8));

        // sprinter f4f5d450e816: `application_name` — REGISTERED so the name is
        // known (a `SHOW`/`RESET` of it is no longer "unrecognized configuration
        // parameter"), with PostgreSQL's default of the empty string.
        //
        // This entry is a DECLARATION, never the value. `application_name` is
        // per-connection, and this registry is ONE process-global map, so
        // `EmbeddedDatabase::try_handle_application_name_setting` intercepts
        // every `SET` / `RESET` / `SHOW` of it BEFORE this map is consulted and
        // answers from the session's own backend state — the same separation
        // GH#28 made for the connection-lifetime timeouts.
        settings.insert("application_name".to_string(), SettingValue::String(String::new()));

        // Display settings
        settings.insert("client_encoding".to_string(), SettingValue::String("UTF8".to_string()));
        settings.insert("datestyle".to_string(), SettingValue::String("ISO, MDY".to_string()));
        settings.insert("timezone".to_string(), SettingValue::String("UTC".to_string()));

        // Server info (read-only)
        settings.insert(
            "server_version".to_string(),
            SettingValue::String(env!("CARGO_PKG_VERSION").to_string()),
        );
        settings.insert("server_encoding".to_string(), SettingValue::String("UTF8".to_string()));

        Self {
            settings: Arc::new(RwLock::new(settings)),
        }
    }

    /// Set a setting value
    pub fn set(&self, name: &str, value: SettingValue) -> Result<()> {
        let normalized_name = name.to_lowercase();

        // Check if setting is read-only
        if Self::is_read_only(&normalized_name) {
            // PostgreSQL wording for a postmaster-scoped parameter
            // (SQLSTATE 55P02 cant_change_runtime_param on the wire).
            return Err(Error::query_execution(format!(
                "parameter \"{}\" cannot be changed now",
                normalized_name
            )));
        }

        // Validate setting value
        Self::validate_setting(&normalized_name, &value)?;

        let mut settings = self
            .settings
            .write()
            .map_err(|e| Error::Generic(format!("Failed to acquire settings lock: {}", e)))?;

        settings.insert(normalized_name, value);
        Ok(())
    }

    /// Get a setting value
    pub fn get(&self, name: &str) -> Option<SettingValue> {
        let normalized_name = name.to_lowercase();
        let settings = self.settings.read().ok()?;
        settings.get(&normalized_name).cloned()
    }

    /// Get all settings
    pub fn get_all(&self) -> HashMap<String, SettingValue> {
        self.settings.read().map(|s| s.clone()).unwrap_or_default()
    }

    /// Check if a setting is read-only
    fn is_read_only(name: &str) -> bool {
        // `authentication_timeout` (GH#28) is postmaster-scoped in PostgreSQL:
        // a session lengthening its own authentication window would be a
        // security regression, so it fails closed like `max_connections`.
        matches!(
            name,
            "server_version" | "server_encoding" | "max_connections" | "port" | "authentication_timeout"
        )
    }

    /// sprinter a3077a3f68d8: validate a value for `name` WITHOUT storing it.
    ///
    /// The per-session overlay stores user-settable GUCs on the session, but the
    /// rules for what is a legal value are a property of the PARAMETER, not of
    /// where it is kept — so both writers go through this one validator and a
    /// session-scoped `SET default_compression = 'banana'` fails closed exactly
    /// as the registry write did.
    pub fn validate(name: &str, value: &SettingValue) -> Result<()> {
        Self::validate_setting(&name.to_lowercase(), value)
    }

    /// Validate setting value
    fn validate_setting(name: &str, value: &SettingValue) -> Result<()> {
        match name {
            // GH#28: PostgreSQL GUC duration syntax (bare integer = ms, or
            // `<n>us|ms|s|min|h|d`). Fail closed — a stored `'banana'` would
            // otherwise be a silent lie on `SHOW`.
            "idle_session_timeout" | "idle_in_transaction_session_timeout" => {
                let raw = value.as_string();
                if crate::protocol::postgres::timeouts::parse_guc_duration_ms(&raw, 1).is_err() {
                    return Err(Error::query_execution(format!(
                        "invalid value for parameter \"{}\": \"{}\"",
                        name, raw
                    )));
                }
            }
            "transaction_isolation" => {
                if let Some(s) = match value {
                    SettingValue::String(s) => Some(s.as_str()),
                    _ => None,
                } {
                    let upper = s.to_uppercase();
                    if !matches!(
                        upper.as_str(),
                        "READ UNCOMMITTED" | "READ COMMITTED" | "REPEATABLE READ" | "SERIALIZABLE"
                    ) {
                        return Err(Error::query_execution(format!(
                            "Invalid transaction isolation level: {}",
                            s
                        )));
                    }
                }
            }
            "default_compression" => {
                if let Some(s) = match value {
                    SettingValue::String(s) => Some(s.as_str()),
                    _ => None,
                } {
                    let lower = s.to_lowercase();
                    if !matches!(lower.as_str(), "none" | "zstd" | "lz4") {
                        return Err(Error::query_execution(format!("Invalid compression type: {}", s)));
                    }
                }
            }
            "vector_index_type" => {
                if let Some(s) = match value {
                    SettingValue::String(s) => Some(s.as_str()),
                    _ => None,
                } {
                    let lower = s.to_lowercase();
                    if !matches!(lower.as_str(), "hnsw" | "flat" | "ivf") {
                        return Err(Error::query_execution(format!("Invalid vector index type: {}", s)));
                    }
                }
            }
            "work_mem" | "shared_buffers" => {
                if let Some(val) = value.as_i64() {
                    if val < 0 {
                        return Err(Error::query_execution(format!("{} must be non-negative", name)));
                    }
                }
            }
            "mv_max_cpu_percent" => {
                if let Some(val) = value.as_i64() {
                    if !(1..=100).contains(&val) {
                        return Err(Error::query_execution(
                            "mv_max_cpu_percent must be between 1 and 100".to_string(),
                        ));
                    }
                }
            }
            "compression_level" => {
                if let Some(val) = value.as_i64() {
                    if !(1..=22).contains(&val) {
                        return Err(Error::query_execution(
                            "compression_level must be between 1 and 22".to_string(),
                        ));
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }

    /// Reset a setting to default
    pub fn reset(&self, name: &str) -> Result<()> {
        let normalized_name = name.to_lowercase();

        if Self::is_read_only(&normalized_name) {
            return Err(Error::query_execution(format!(
                "parameter \"{}\" cannot be changed now",
                normalized_name
            )));
        }

        // Get default value
        let default_settings = Self::new();
        if let Some(default_value) = default_settings.get(&normalized_name) {
            self.set(&normalized_name, default_value)?;
            Ok(())
        } else {
            Err(Error::query_execution(format!("Unknown setting: {}", name)))
        }
    }

    /// Get statement timeout as Duration (None = unlimited)
    pub fn statement_timeout(&self) -> Option<Duration> {
        self.get("statement_timeout")
            .and_then(|v| v.as_duration_ms())
            .filter(|&ms| ms > 0)
            .map(Duration::from_millis)
    }

    /// Get query timeout as Duration (None = unlimited)
    pub fn query_timeout(&self) -> Option<Duration> {
        self.get("query_timeout")
            .and_then(|v| v.as_duration_ms())
            .filter(|&ms| ms > 0)
            .map(Duration::from_millis)
    }

    /// Check if optimizer is enabled
    pub fn optimizer_enabled(&self) -> bool {
        self.get("optimizer").and_then(|v| v.as_bool()).unwrap_or(true)
    }

    /// Check if time-travel is enabled
    pub fn time_travel_enabled(&self) -> bool {
        self.get("time_travel_enabled")
            .and_then(|v| v.as_bool())
            .unwrap_or(true)
    }
}

impl Default for SessionSettings {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn test_setting_value_parsing() {
        assert_eq!(parse_setting_value("on").as_bool(), Some(true));
        assert_eq!(parse_setting_value("off").as_bool(), Some(false));
        assert_eq!(parse_setting_value("123").as_i64(), Some(123));
        assert_eq!(parse_setting_value("hello").as_string(), "hello");
    }

    #[test]
    fn test_session_settings() {
        let settings = SessionSettings::new();

        // Test default values
        assert!(settings.optimizer_enabled());

        // Test set/get
        settings.set("optimizer", SettingValue::Boolean(false)).unwrap();
        assert!(!settings.optimizer_enabled());

        // Test read-only
        let result = settings.set("server_version", SettingValue::String("1.0".to_string()));
        assert!(result.is_err());
    }

    #[test]
    fn test_setting_validation() {
        let settings = SessionSettings::new();

        // Valid isolation level
        settings
            .set(
                "transaction_isolation",
                SettingValue::String("SERIALIZABLE".to_string()),
            )
            .unwrap();

        // Invalid isolation level
        let result = settings.set("transaction_isolation", SettingValue::String("INVALID".to_string()));
        assert!(result.is_err());

        // Valid compression
        settings
            .set("default_compression", SettingValue::String("zstd".to_string()))
            .unwrap();

        // Invalid compression
        let result = settings.set("default_compression", SettingValue::String("invalid".to_string()));
        assert!(result.is_err());
    }

    /// sprinter a3077a3f68d8: the static [`REGISTERED_PARAMETERS`] list is what
    /// both wire listeners classify against, so it must be exactly the set the
    /// constructor declares. A name added to one and not the other is a name
    /// that is either unsettable or unrecognized depending on which code path a
    /// client reaches — the class of split-brain GH#28 was about.
    #[test]
    fn registered_list_matches_the_registry() {
        let built: std::collections::BTreeSet<String> = SessionSettings::new().get_all().into_keys().collect();
        let listed: std::collections::BTreeSet<String> = REGISTERED_PARAMETERS.iter().map(|s| s.to_string()).collect();
        assert_eq!(built, listed, "REGISTERED_PARAMETERS drifted from SessionSettings::new");
        // Every registered name is exactly one of: server-level, user-settable,
        // or one of the three with a dedicated session slot of their own.
        for name in REGISTERED_PARAMETERS {
            let dedicated = matches!(
                *name,
                "application_name" | "idle_session_timeout" | "idle_in_transaction_session_timeout"
            );
            assert_eq!(
                is_user_settable(name),
                !is_server_level(name) && !dedicated,
                "{name} is classified inconsistently"
            );
        }
        assert!(
            !is_user_settable("no_such_parameter"),
            "an unknown name is not settable"
        );
    }

    #[test]
    fn test_reset_setting() {
        let settings = SessionSettings::new();

        // Change a setting
        settings.set("optimizer", SettingValue::Boolean(false)).unwrap();
        assert!(!settings.optimizer_enabled());

        // Reset it
        settings.reset("optimizer").unwrap();
        assert!(settings.optimizer_enabled());
    }
}
