//! What the shell reports about the database it holds.
//!
//! `\status`, `\config`, `\telemetry`, `\replication` and `\conninfo` read the
//! same facts from the handle (its configuration and its storage), through the
//! functions here, so two of them can never answer differently about the same
//! thing. Each function returns the text; the commands print it, and the tests
//! read it without capturing stdout.

use super::ReplConfig;
#[cfg(feature = "ha-tier1")]
use crate::replication::role_override::{read_role_override, role_override_path};
use crate::EmbeddedDatabase;
use std::fmt::Write;
use std::path::PathBuf;

fn enabled(on: bool) -> &'static str {
    if on {
        "enabled"
    } else {
        "disabled"
    }
}

/// The storage features of the handle: WAL, compression, time travel.
///
/// One block, used by `\status`, `\config` and `\telemetry`, read from the
/// storage engine and the configuration the handle was opened with.
pub fn storage_features(db: &EmbeddedDatabase) -> String {
    let compression = format!("{:?}", db.config.storage.compression).to_lowercase();
    let mut out = String::new();
    let _ = writeln!(out, "  WAL: {}", enabled(db.storage.is_wal_enabled()));
    let _ = writeln!(out, "  Compression: {compression}");
    let _ = writeln!(out, "  Time-Travel: {}", enabled(db.storage.time_travel_enabled()));
    out
}

/// `\status`.
pub fn render_status(db: &EmbeddedDatabase) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "\nDatabase Status");
    let _ = writeln!(out, "{}", "=".repeat(50));

    let _ = writeln!(out, "\nConnection:");
    let _ = writeln!(out, "  Mode: REPL (embedded)");
    let _ = writeln!(out, "  State: Connected");
    let _ = writeln!(out, "  Branch: {}", db.current_branch());

    let tables = {
        let _scope = db.embedded_catalog_scope();
        db.storage.catalog().list_tables().unwrap_or_default()
    };
    let _ = writeln!(out, "\nSchema:");
    let _ = writeln!(out, "  Tables: {}", tables.len());

    if let Some(stats) = db.storage.get_storage_stats() {
        let _ = writeln!(out, "\nStorage:");
        let _ = writeln!(out, "  Approximate size: {} bytes", stats.approximate_size);
        let _ = writeln!(out, "  Keys: {}", stats.key_count);
    }

    let _ = writeln!(out, "\nFeatures:");
    out.push_str(&storage_features(db));
    out
}

/// `\config`.
pub fn render_config(db: &EmbeddedDatabase, config: Option<&ReplConfig>) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "\nCurrent Configuration:");
    let _ = writeln!(out, "{}", "─".repeat(70));

    let _ = writeln!(out, "REPL Settings:");
    if let Some(cfg) = config {
        let _ = writeln!(out, "  Timing display:  {}", enabled(cfg.show_timing));
        let _ = writeln!(out, "  Output format:   {:?}", cfg.output_format);
        let _ = writeln!(out, "  Show row count:  {}", enabled(cfg.show_row_count));
        let _ = writeln!(out, "  Auto-commit:     {}", enabled(cfg.auto_commit));
        let _ = writeln!(out, "  Null display:    \"{}\"", cfg.null_display);
        let _ = writeln!(out, "  Max col width:   {}", cfg.max_column_width);
        let _ = writeln!(out, "  Max history:     {}", cfg.max_history);
        if let Some(path) = &cfg.history_path {
            let _ = writeln!(out, "  History file:    {path}");
        }
        match &cfg.config_path {
            Some(path) => {
                let _ = writeln!(out, "  Config file:     {}", path.display());
            }
            None => {
                let _ = writeln!(out, "  Config file:     (none - reload disabled)");
            }
        }
    } else {
        let _ = writeln!(out, "  (configuration context not available)");
    }

    let _ = writeln!(out, "\nStorage:");
    out.push_str(&storage_features(db));

    let _ = writeln!(out, "\nPerformance:");
    let workers = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4);
    let _ = writeln!(out, "  Workers: {workers}");

    if config.is_some_and(|c| c.config_path.is_some()) {
        let _ = writeln!(out, "\nUse \\config reload to reload from file");
    } else {
        let _ = writeln!(
            out,
            "\nStart the shell with --config to enable reload: heliosdb-nano repl --config repl.toml"
        );
    }
    out
}

/// `\telemetry`.
pub fn render_telemetry(db: &EmbeddedDatabase) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "\nDatabase Telemetry:");
    let _ = writeln!(out, "{}", "═".repeat(60));

    let _ = writeln!(out, "\nStorage:");
    let tables = {
        let _scope = db.embedded_catalog_scope();
        db.storage.catalog().list_tables()
    };
    match tables {
        Ok(tables) => {
            let _ = writeln!(out, "  Tables: {}", tables.len());
        }
        Err(_) => {
            let _ = writeln!(out, "  Tables: unknown");
        }
    }

    let _ = writeln!(out, "\nSystem:");
    let workers = std::thread::available_parallelism()
        .map(|n| n.get().to_string())
        .unwrap_or_else(|_| "unknown".to_string());
    let _ = writeln!(out, "  Workers: {workers}");
    {
        use sysinfo::System;
        let sys = System::new_all();
        let _ = writeln!(out, "  Memory Used: {} MB", sys.used_memory() / 1024 / 1024);
        let _ = writeln!(out, "  Memory Total: {} MB", sys.total_memory() / 1024 / 1024);
    }

    let _ = writeln!(out, "\nConfiguration:");
    out.push_str(&storage_features(db));

    let _ = writeln!(out, "\nVector Stores:");
    match db.list_vector_stores() {
        Ok(stores) if stores.is_empty() => {
            let _ = writeln!(out, "  None configured");
        }
        Ok(stores) => {
            for store in stores {
                let _ = writeln!(
                    out,
                    "  {} ({} dims, {} vectors)",
                    store.name, store.dimensions, store.vector_count
                );
            }
        }
        Err(_) => {
            let _ = writeln!(out, "  Unable to retrieve");
        }
    }

    let _ = writeln!(out, "\nAgent Sessions:");
    match db.list_agent_sessions() {
        Ok(sessions) => {
            let _ = writeln!(out, "  Active: {}", sessions.len());
        }
        Err(_) => {
            let _ = writeln!(out, "  Active: unknown");
        }
    }

    let _ = writeln!(out, "\nFor detailed stats, use:");
    let _ = writeln!(out, "  SELECT * FROM pg_database_branches();");
    let _ = writeln!(out, "  SELECT * FROM pg_mv_staleness();");
    let _ = writeln!(out, "  SELECT * FROM pg_vector_index_stats();");
    out
}

/// The part of a role record `\replication` shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoleRecordSummary {
    /// `"primary"` or `"standby"`.
    pub role: String,
    /// The primary a standby follows.
    pub primary_host: Option<String>,
    /// The replication epoch the record was written at.
    pub epoch: u64,
}

/// What `\replication` reports: the handle's own state, and the role record
/// beside its data directory.
#[derive(Debug, Clone)]
pub struct ReplicationState {
    /// The handle serves a physical standby (writes are refused).
    pub physical_standby: bool,
    /// The write-ahead log is on, with its current LSN when known.
    #[allow(clippy::option_option)]
    pub wal: Option<Option<u64>>,
    /// The role record beside the data directory (a switchover or promotion
    /// leaves one), when there is one that can be read.
    pub role_record: Option<RoleRecordSummary>,
    /// Where the record was looked for, when the handle has a data directory.
    pub record_path: Option<PathBuf>,
    /// The record exists but could not be read.
    pub record_error: Option<String>,
}

impl ReplicationState {
    /// Read the state from the handle.
    pub fn read(db: &EmbeddedDatabase) -> Self {
        let wal = db.storage.is_wal_enabled().then(|| db.storage.wal_lsn());
        #[cfg_attr(not(feature = "ha-tier1"), allow(unused_mut))]
        let mut state = Self {
            physical_standby: db.is_physical_standby(),
            wal,
            role_record: None,
            record_path: None,
            record_error: None,
        };
        if db.config.storage.memory_only {
            return state;
        }
        #[cfg(feature = "ha-tier1")]
        if let Some(dir) = db.config.storage.path.as_deref() {
            state.record_path = role_override_path(dir).ok();
            match read_role_override(dir) {
                Ok(record) => {
                    state.role_record = record.map(|record| RoleRecordSummary {
                        role: record.replication.role,
                        primary_host: record.replication.primary_host,
                        epoch: record.directory.epoch,
                    });
                }
                Err(error) => state.record_error = Some(error),
            }
        }
        state
    }
}

/// `\replication`, from the state of the handle.
pub fn render_replication_state(state: &ReplicationState) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "\nReplication (this handle)");
    let _ = writeln!(out, "{}", "=".repeat(50));

    let role = match (&state.role_record, state.physical_standby) {
        (_, true) => "physical standby (reads only; writes are refused)".to_string(),
        (Some(record), false) if record.role == "primary" => "primary".to_string(),
        (Some(record), false) => format!("{} (recorded; this handle is not serving as a standby)", record.role),
        (None, false) => "standalone (in-process shell)".to_string(),
    };
    let _ = writeln!(out, "  Role: {role}");

    match state.wal {
        Some(Some(lsn)) => {
            let _ = writeln!(out, "  WAL: enabled (current LSN {lsn})");
        }
        Some(None) => {
            let _ = writeln!(out, "  WAL: enabled");
        }
        None => {
            let _ = writeln!(out, "  WAL: disabled");
        }
    }

    match (&state.role_record, &state.record_error, &state.record_path) {
        (Some(record), _, path) => {
            if let Some(path) = path {
                let _ = writeln!(out, "  Role record: {}", path.display());
            }
            let _ = writeln!(out, "    role: {}", record.role);
            if let Some(host) = &record.primary_host {
                let _ = writeln!(out, "    follows: {host}");
            }
            let _ = writeln!(out, "    epoch: {}", record.epoch);
        }
        (None, Some(error), _) => {
            let _ = writeln!(out, "  Role record: unreadable ({error})");
        }
        (None, None, Some(path)) => {
            let _ = writeln!(out, "  Role record: none ({} does not exist)", path.display());
        }
        (None, None, None) => {
            let _ = writeln!(out, "  Role record: none (no data directory)");
        }
    }

    let _ = writeln!(
        out,
        "\nThe state of a running server and its standbys is reported by that server;\nconnect to it and run SHOW REPLICATION STATUS."
    );
    out
}

/// `\replication`.
pub fn render_replication(db: &EmbeddedDatabase) -> String {
    render_replication_state(&ReplicationState::read(db))
}

/// `\conninfo`: what this shell is connected to.
pub fn render_conninfo(db: &EmbeddedDatabase) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "\nConnection");
    let _ = writeln!(out, "{}", "=".repeat(50));
    let _ = writeln!(out, "  Mode: embedded (in-process, single user)");
    if db.config.storage.memory_only || db.config.storage.path.is_none() {
        let _ = writeln!(out, "  Data directory: (in memory - lost on exit)");
    } else if let Some(path) = &db.config.storage.path {
        let _ = writeln!(out, "  Data directory: {}", path.display());
    }
    let _ = writeln!(out, "  Branch: {}", db.current_branch());
    let _ = writeln!(
        out,
        "\nThis shell opens the database itself; it listens on no port and holds no TLS\nsettings. To reach a server, connect with psql or any PostgreSQL driver."
    );
    out
}
