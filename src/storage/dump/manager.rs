//! Dump Manager for HeliosDB-Lite Multi-User ACID In-Memory Mode (v3.1.0)
//!
//! Provides memory-to-disk persistence with full and incremental dumps,
//! compression, checksumming, and restore functionality.
//!
//! ## Features
//! - Full and incremental dumps with append mode
//! - Zstandard and LZ4 compression support
//! - CRC32 checksum validation
//! - Concurrent read-only restores
//! - >100MB/s throughput target
//! - Dirty state tracking for incremental dumps

use super::format::{CompressionType, DUMP_MAGIC_NUMBER, DUMP_VERSION};
// HDB-005: the SQL-text dump's one serializer. The binary paths below do not
// use it — only `create_sql_dump` does.
use super::sql_text;
use crate::{Error, Result, Schema, Tuple};
use parking_lot::{Mutex, RwLock};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fs::{File, OpenOptions};
use std::io::{BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Instant, SystemTime};
use tracing::{debug, info, warn};

/// Dump type identifier
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DumpType {
    /// Full dump (all data)
    Full,
    /// Incremental dump (changes only)
    Incremental,
}

/// Metadata for a single dump operation
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DumpMetadata {
    /// Unique dump ID
    pub dump_id: u64,
    /// Creation timestamp
    pub created_at: SystemTime,
    /// Dump type (Full/Incremental)
    pub dump_type: DumpType,
    /// Number of tables dumped
    pub table_count: u32,
    /// Total rows dumped
    pub total_rows: u64,
    /// Compressed size in bytes
    pub compressed_size: u64,
    /// Uncompressed size in bytes
    pub uncompressed_size: u64,
    /// CRC32 checksum (hex string)
    pub checksum: String,
    /// Number of appends to this dump
    pub append_count: u32,
    /// Compression the ROW BATCHES in this file are encoded with.
    ///
    /// `None` means the file records nothing — every format-v1 file (the field
    /// did not exist), and the header-less incremental dumps whose 8 KB
    /// placeholder is never filled in. Restore falls back to frame detection
    /// there; see `DumpManager::decompress_batch`.
    ///
    /// HDB-003 1b: restore used to decompress with the RESTORING manager's
    /// configured compression, so a dump written by `dump_full_uncompressed`
    /// could not be read back by the default (zstd) manager.
    #[serde(default)]
    pub compression: Option<CompressionType>,
}

impl DumpMetadata {
    /// Create new metadata
    pub fn new(dump_id: u64, dump_type: DumpType) -> Self {
        Self {
            dump_id,
            created_at: SystemTime::now(),
            dump_type,
            table_count: 0,
            total_rows: 0,
            compressed_size: 0,
            uncompressed_size: 0,
            checksum: String::new(),
            append_count: 0,
            compression: None,
        }
    }
}

/// Index metadata for dumps
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexMetadata {
    /// Index name
    pub name: String,
    /// Index type (e.g., "btree", "hash", "gin")
    pub index_type: String,
    /// Columns in index
    pub columns: Vec<String>,
    /// Is unique index
    pub is_unique: bool,
}

/// v10.12: the schema objects that live OUTSIDE a table's `Schema` and its
/// constraint record — sequences, SERIAL / IDENTITY columns, every user index
/// (ordinary, UNIQUE, expression, HNSW with its metric) and views.
///
/// Before this, a dump carried none of them: the per-table index list reported
/// vector indexes only, the dump module had no code for sequences or views, and
/// a SERIAL column came back as a plain integer. After a restore an INSERT
/// without its key failed, a duplicate the unique index used to reject was
/// accepted, the views were gone and every sequence restarted at 1.
///
/// Both formats carry the same value: the SQL writer renders it through
/// `sql_text` (sequences before the tables, `setval`, indexes after the data,
/// views last) and the binary writer (format v3) stores it as one `SCHM`
/// section before the end marker. Restore replays it AFTER the rows are in, so
/// a `CREATE UNIQUE INDEX` validates the restored rows through the normal path.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SchemaObjects {
    /// `CREATE SEQUENCE` objects with their position.
    pub sequences: Vec<SequenceDump>,
    /// `(table, columns)` declared SERIAL or `GENERATED … AS IDENTITY`.
    pub identity_columns: Vec<(String, Vec<String>)>,
    /// Every user index, in the order it is to be recreated.
    pub indexes: Vec<IndexDump>,
    /// Plain views, in creation order (a view may read an earlier one).
    pub views: Vec<ViewDump>,
    /// Objects the dump does NOT carry, one human-readable line each
    /// (materialized views, triggers, …). Written into the SQL header and
    /// reported by restore as warnings, never dropped silently.
    pub not_carried: Vec<String>,
    /// The `COMMENT ON` statements of the stored comments, tables before their
    /// columns. Not part of the bincode layout above (a file written before
    /// comments were stored has none): the binary writer appends them after it
    /// in the `SCHM` section, where a reader that does not know them stops.
    #[serde(skip)]
    pub comments: Vec<CommentDump>,
}

/// One stored comment, as the statement that makes it again.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CommentDump {
    /// The table the comment belongs to (its own, a column's, a constraint's).
    pub table: Option<String>,
    /// `COMMENT ON … IS '…';`
    pub statement: String,
}

impl SchemaObjects {
    /// True when there is nothing to write.
    pub fn is_empty(&self) -> bool {
        self.sequences.is_empty()
            && self.identity_columns.is_empty()
            && self.indexes.is_empty()
            && self.views.is_empty()
            && self.not_carried.is_empty()
            && self.comments.is_empty()
    }

    /// The identity columns recorded for `table`.
    pub fn identity_for(&self, table: &str) -> &[String] {
        self.identity_columns
            .iter()
            .find(|(t, _)| t == table)
            .map(|(_, cols)| cols.as_slice())
            .unwrap_or(&[])
    }
}

/// One `CREATE SEQUENCE` and its current position.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SequenceDump {
    /// Sequence name.
    pub name: String,
    /// `smallint` | `integer` | `bigint`.
    pub data_type: String,
    /// START WITH.
    pub start_value: i64,
    /// INCREMENT BY.
    pub increment_by: i64,
    /// MINVALUE.
    pub min_value: i64,
    /// MAXVALUE.
    pub max_value: i64,
    /// CACHE.
    pub cache: i64,
    /// CYCLE.
    pub cycle: bool,
    /// The value last handed out, `None` when `nextval` was never called.
    pub last_value: Option<i64>,
}

/// One user index, as the SQL that recreates it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IndexDump {
    /// Table the index is on.
    pub table: String,
    /// Index name.
    pub name: String,
    /// `CREATE UNIQUE INDEX`.
    pub unique: bool,
    /// Access method (`btree`, `hash`, `gin`, `hnsw`, …).
    pub method: String,
    /// The key list exactly as it goes between the parentheses, already
    /// quoted (`"a", "b"`, `lower(email)`, `"emb" vector_cosine_ops`).
    pub key: String,
    /// Extra `WITH (…)` options, verbatim, or empty.
    pub with_options: String,
}

/// One plain view.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ViewDump {
    /// View name.
    pub name: String,
    /// The defining SELECT, as stored.
    pub query_sql: String,
}

/// Dirty state tracker for incremental dumps
pub struct DirtyTracker {
    /// Last dump timestamp
    last_dump_time: Arc<Mutex<Option<Instant>>>,
    /// Dirty flag
    dirty: Arc<RwLock<bool>>,
    /// Dirty tables since last dump
    dirty_tables: Arc<RwLock<HashSet<String>>>,
}

impl DirtyTracker {
    /// Create new dirty tracker
    pub fn new() -> Self {
        Self {
            last_dump_time: Arc::new(Mutex::new(None)),
            dirty: Arc::new(RwLock::new(false)),
            dirty_tables: Arc::new(RwLock::new(HashSet::new())),
        }
    }

    /// Mark database as dirty
    pub fn mark_dirty(&self) {
        *self.dirty.write() = true;
    }

    /// Mark specific table as dirty
    pub fn mark_table_dirty(&self, table: &str) {
        self.dirty_tables.write().insert(table.to_string());
        self.mark_dirty();
    }

    /// Check if database is dirty
    pub fn is_dirty(&self) -> bool {
        *self.dirty.read()
    }

    /// Get list of dirty tables
    pub fn get_dirty_tables(&self) -> Vec<String> {
        self.dirty_tables.read().iter().cloned().collect()
    }

    /// Clear dirty state
    pub fn clear_dirty(&self) {
        *self.dirty.write() = false;
        self.dirty_tables.write().clear();
        *self.last_dump_time.lock() = Some(Instant::now());
    }

    /// Get time since last dump
    pub fn time_since_last_dump(&self) -> Option<std::time::Duration> {
        self.last_dump_time.lock().map(|t| t.elapsed())
    }
}

impl Default for DirtyTracker {
    fn default() -> Self {
        Self::new()
    }
}

/// Database interface for dump operations
pub trait DatabaseInterface: Send + Sync {
    /// List all tables
    fn list_tables(&self) -> Result<Vec<String>>;

    /// Get table schema
    fn get_table_schema(&self, table: &str) -> Result<Schema>;

    /// Scan all rows in a table
    fn scan_table(&self, table: &str) -> Result<Vec<Tuple>>;

    /// Get table indexes
    fn get_table_indexes(&self, table: &str) -> Result<Vec<IndexMetadata>>;

    /// Table-level constraints (FK / CHECK / UNIQUE) — separate from the schema in the catalog.
    ///
    /// `Schema` carries only the COLUMN flags (`primary_key`, `unique`,
    /// `nullable`); every FOREIGN KEY, CHECK and table-level UNIQUE lives in a
    /// side record (`catalog.load_table_constraints`). A dump that serialises
    /// the schema alone therefore drops them all — HDB-003.
    ///
    /// Defaulted so out-of-tree implementors keep compiling; an implementor
    /// that does not override it produces constraint-free dumps, exactly as
    /// before.
    fn get_table_constraints(&self, table: &str) -> Result<crate::sql::TableConstraints> {
        let _ = table;
        Ok(Default::default())
    }

    /// v10.12: sequences, identity columns, every user index and views.
    ///
    /// Defaulted to nothing so out-of-tree implementors keep compiling; such
    /// an implementor dumps exactly what it did before. When this reports an
    /// index, the per-table [`Self::get_table_indexes`] entry of the same name
    /// is not written a second time.
    fn get_schema_objects(&self) -> Result<SchemaObjects> {
        Ok(SchemaObjects::default())
    }
}

/// Database interface for restore operations
pub trait DatabaseRestoreInterface {
    /// Create table with schema
    fn create_table(&mut self, name: &str, schema: Schema) -> Result<()>;

    /// Create index
    fn create_index(&mut self, table: &str, index: &IndexMetadata) -> Result<()>;

    /// Insert row
    fn insert_row(&mut self, table: &str, row: Tuple) -> Result<()>;

    /// Phase B of a restore: register a table's FK / CHECK / UNIQUE constraints.
    ///
    /// Called AFTER every table in the dump has been created and populated, so
    /// a child table that sorts before its parent (or a cycle) still finds its
    /// parent present. Implementors must register through the same funnel
    /// `CREATE TABLE` / `ALTER TABLE … ADD CONSTRAINT` uses, so the enforcing
    /// ART indexes exist and are backfilled from the rows already restored.
    ///
    /// Defaulted to a no-op: an implementor that does not override it restores
    /// exactly as it did before v2.
    fn restore_table_constraints(&mut self, table: &str, constraints: &crate::sql::TableConstraints) -> Result<()> {
        let _ = (table, constraints);
        Ok(())
    }

    /// Phase C of a restore: verify the restored ROWS satisfy the constraints
    /// registered in phase B.
    ///
    /// Runs for every restored table only after ALL of them have their
    /// constraints registered (an FK graph may be cyclic).
    ///
    /// Under [`RestoreValidation::Strict`] (the default) an `Err` fails the
    /// restore: a dump whose data violates its own constraints must not be
    /// reported as a valid database. Under [`RestoreValidation::Lenient`]
    /// (`heliosdb-nano restore --no-validate`, `RestoreOptions::
    /// validate_constraints = false`) the same violations are reported as
    /// warnings and the restore succeeds — which is how a backup taken from a
    /// database that legitimately holds violating rows (a `NOT ENFORCED` or
    /// `LOCK-FREE` constraint, `SET helios.fk_validation = 'audit'`) is
    /// restored through the supported command. The constraints stay registered
    /// either way, so the database still rejects NEW violations if the operator
    /// keeps the data.
    ///
    /// The error must name the violations WITHOUT a "restore validation failed"
    /// prefix: the manager aggregates every table's into ONE error under that
    /// prefix, so an operator fixing a multi-table dump sees all of them at once.
    fn validate_restored_constraints(&mut self, table: &str, validation: RestoreValidation) -> Result<()> {
        let _ = (table, validation);
        Ok(())
    }

    /// Called ONCE before a restore creates its first table, so an implementor
    /// can refuse a target state it cannot restore into — before the target
    /// directory has been half-populated.
    ///
    /// (The embedded database uses it to refuse a restore while a non-main
    /// branch is active: index state lives on main, so phase B's
    /// `ALTER TABLE … ADD CONSTRAINT … UNIQUE` would fail partway through.)
    fn begin_restore(&mut self) -> Result<()> {
        Ok(())
    }

    /// Drain the non-fatal notes phases B and C accumulated (a dangling FK
    /// parent that is in neither the dump nor the target, a CHECK expression
    /// this build cannot evaluate). They land in `RestoreReport`.
    fn take_restore_warnings(&mut self) -> Vec<String> {
        Vec::new()
    }

    /// v10.12: recreate the dump's sequences, identity columns, indexes and
    /// views. Called after every table is populated and its constraints are
    /// registered, so a UNIQUE index validates the restored rows.
    ///
    /// Defaulted: an implementor that cannot do it says so instead of
    /// restoring a database that silently lacks them.
    fn restore_schema_objects(&mut self, objects: &SchemaObjects) -> Result<()> {
        if objects.is_empty() {
            return Ok(());
        }
        Err(Error::storage(
            "this restore target cannot recreate the dump's sequences, indexes and views",
        ))
    }
}

/// Dump Manager
///
/// Manages full and incremental database dumps with compression and integrity checking.
pub struct DumpManager {
    /// Dump history
    dump_history: Arc<RwLock<Vec<DumpMetadata>>>,
    /// Last dump time
    last_dump_time: Arc<Mutex<Instant>>,
    /// Dirty tracker
    dirty_tracker: Arc<DirtyTracker>,
    /// Compression type
    compression: CompressionType,
    /// Data directory
    data_dir: PathBuf,
    /// Next dump ID counter
    next_dump_id: Arc<Mutex<u64>>,
}

impl DumpManager {
    /// Create a new dump manager
    pub fn new(data_dir: PathBuf, compression: CompressionType) -> Self {
        Self {
            dump_history: Arc::new(RwLock::new(Vec::new())),
            last_dump_time: Arc::new(Mutex::new(Instant::now())),
            dirty_tracker: Arc::new(DirtyTracker::new()),
            compression,
            data_dir,
            next_dump_id: Arc::new(Mutex::new(1)),
        }
    }

    /// Get next dump ID (atomic counter)
    pub fn get_next_dump_id(&self) -> u64 {
        let mut id = self.next_dump_id.lock();
        let current = *id;
        *id += 1;
        current
    }

    /// Dump database (CLI-compatible wrapper)
    ///
    /// This is a simplified interface for CLI use. For more control,
    /// use `create_full_dump` or `create_incremental_dump` directly.
    pub fn dump<D: DatabaseInterface>(&self, opts: &DumpOptions, db: &D) -> Result<DumpReport> {
        let start_time = Instant::now();

        // Dispatch based on format
        let metadata = match opts.format {
            DumpOutputFormat::Binary => match opts.mode {
                DumpMode::Full => self.create_full_dump(&opts.output_path, db)?,
                DumpMode::Incremental => self.create_incremental_dump(&opts.output_path, db, opts.append)?,
            },
            DumpOutputFormat::Sql => {
                let mut sql = opts.sql.clone();
                if sql.tables.is_none() {
                    sql.tables = opts.tables.clone();
                }
                self.create_sql_dump_with(&opts.output_path, db, &sql)?
            }
        };

        let duration_ms = start_time.elapsed().as_millis() as u64;
        let compression_ratio = if metadata.uncompressed_size > 0 {
            metadata.compressed_size as f64 / metadata.uncompressed_size as f64
        } else {
            1.0
        };

        Ok(DumpReport {
            dump_id: metadata.dump_id.to_string(),
            tables_dumped: metadata.table_count as usize,
            rows_dumped: metadata.total_rows,
            bytes_written: metadata.compressed_size,
            bytes_uncompressed: metadata.uncompressed_size,
            duration_ms,
            compression_ratio,
        })
    }

    /// Create a SQL dump of the database — a restorable PostgreSQL script.
    ///
    /// HDB-005. Every identifier is double-quoted and every value is rendered
    /// by the shared, schema-aware serializer (`super::sql_text`), which is
    /// EXHAUSTIVE over `Value` and types each literal against its column. The
    /// v1 format this replaces wrote bare identifiers (`CREATE TABLE IF NOT
    /// EXISTS my table (`) and fell back to `format!("'{:?}'", value)` for
    /// every type it had not enumerated, so JSON, NUMERIC, UUID, BYTEA, arrays
    /// and vectors came out as Rust debug strings; it also dropped defaults,
    /// constraints and indexes entirely.
    ///
    /// ## Statement order
    ///
    /// 1. every `CREATE TABLE` **without** its foreign keys,
    /// 2. every table's `INSERT`s,
    /// 3. every `ALTER TABLE … ADD CONSTRAINT … FOREIGN KEY`,
    /// 4. every VECTOR index — `DatabaseInterface::get_table_indexes` reports
    ///    only those, so ordinary and unique indexes are absent here exactly as
    ///    they are from the binary dump.
    ///
    /// FKs last is what makes the file order-independent: a child table can be
    /// created and filled before its parent exists, cycles need no topological
    /// sort, and no referential check runs against a half-loaded database.
    ///
    /// Restore with `EmbeddedDatabase::execute_sql_script` into an EMPTY
    /// database — `IF NOT EXISTS` keeps a table that is already there, and the
    /// INSERTs would then collide with its rows.
    ///
    /// # Errors
    ///
    /// FAILS rather than writing a file that cannot be restored: a CHECK body
    /// or a column DEFAULT whose expression the SQL renderer cannot spell
    /// (`CASE`, an aggregate, a subquery …) stops the export naming the
    /// constraint, and an unresolved `DictRef`/`CasRef`/`ColumnarRef` stops it
    /// naming the column. A partially written file is left on disk in both
    /// cases; it is not a dump and must not be used as one.
    pub fn create_sql_dump<D: DatabaseInterface>(&self, output_path: &Path, db: &D) -> Result<DumpMetadata> {
        self.create_sql_dump_with(output_path, db, &SqlDumpOptions::default())
    }

    /// [`Self::create_sql_dump`] with a dialect, a table filter and the
    /// schema-only / data-only switches (v10.13).
    ///
    /// Atomic: the file is written to a temporary file in the target's
    /// directory and renamed over the target only when complete; on any error
    /// the temporary file is removed and the target is left as it was.
    pub fn create_sql_dump_with<D: DatabaseInterface>(
        &self,
        output_path: &Path,
        db: &D,
        options: &SqlDumpOptions,
    ) -> Result<DumpMetadata> {
        if options.schema_only && options.data_only {
            return Err(Error::config("--schema-only and --data-only cannot be used together"));
        }
        let start_time = Instant::now();
        let dump_id = self.get_next_dump_id();
        let mut metadata = DumpMetadata::new(dump_id, DumpType::Full);

        info!("Starting SQL dump {} to {}", dump_id, output_path.display());

        let file_name = output_path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "dump.sql".to_string());
        let temp_path = output_path.with_file_name(format!(".{}.partial-{}", file_name, std::process::id()));
        let file =
            File::create(&temp_path).map_err(|e| Error::storage(format!("Failed to create SQL dump file: {}", e)))?;
        let mut writer = BufWriter::new(file);

        let written = Self::write_sql_dump(&mut writer, db, options, &mut metadata).and_then(|()| {
            let file = writer
                .into_inner()
                .map_err(|e| Error::storage(format!("Failed to flush writer: {}", e)))?;
            file.sync_all()
                .map_err(|e| Error::storage(format!("Failed to sync SQL dump: {}", e)))?;
            std::fs::rename(&temp_path, output_path)
                .map_err(|e| Error::storage(format!("Failed to move SQL dump into place: {}", e)))
        });
        if let Err(e) = written {
            let _ = std::fs::remove_file(&temp_path);
            return Err(e);
        }

        let file_size = std::fs::metadata(output_path)
            .map_err(|e| Error::storage(format!("Failed to get file metadata: {}", e)))?
            .len();
        metadata.compressed_size = file_size;
        metadata.uncompressed_size = file_size; // SQL is uncompressed text

        // Add to history
        self.dump_history.write().push(metadata.clone());

        debug!(
            "SQL dump {} wrote {} tables / {} rows in {} ms",
            dump_id,
            metadata.table_count,
            metadata.total_rows,
            start_time.elapsed().as_millis()
        );

        Ok(metadata)
    }

    /// The body of [`Self::create_sql_dump_with`], into any writer.
    fn write_sql_dump<D: DatabaseInterface, W: Write>(
        writer: &mut W,
        db: &D,
        options: &SqlDumpOptions,
        metadata: &mut DumpMetadata,
    ) -> Result<()> {
        let postgres = options.dialect == SqlDialect::Postgres;
        let write_ddl = !options.data_only;
        let write_data = !options.schema_only;
        let w = |e: std::io::Error| Error::storage(format!("Failed to write SQL dump: {}", e));

        // Header. Every line is a `--` comment, so the whole block is attached
        // to the first statement by the script splitter and stripped off again
        // by `execute_sql_script`.
        writeln!(
            writer,
            "-- HeliosDB Nano Database Dump\n\
             -- Generated: {}\n\
             -- Database: heliosdb-nano\n\
             -- Format: heliosdb-nano sql v2\n\
             -- Restore with EmbeddedDatabase::execute_sql_script, into an EMPTY database.\n",
            chrono::Local::now().format("%Y-%m-%d %H:%M:%S")
        )
        .map_err(|e| Error::storage(format!("Failed to write header: {}", e)))?;
        if postgres {
            writeln!(
                writer,
                "-- Dialect: postgres. Load with: psql -v ON_ERROR_STOP=1 -f <file>, into an EMPTY database."
            )
            .map_err(w)?;
        }

        let all_tables = db.list_tables()?;
        let filtered = options.tables.is_some();
        let tables: Vec<String> = match &options.tables {
            None => all_tables,
            Some(wanted) => {
                let mut picked = Vec::with_capacity(wanted.len());
                for name in wanted {
                    match all_tables.iter().find(|t| t.eq_ignore_ascii_case(name)) {
                        Some(t) => picked.push(t.clone()),
                        None => return Err(Error::storage(format!("dump: table \"{name}\" does not exist"))),
                    }
                }
                picked
            }
        };
        metadata.table_count = tables.len() as u32;
        let in_dump = |t: &str| tables.iter().any(|x| x == t);

        // v10.12: sequences, identity columns, user indexes and views.
        let mut objects = db.get_schema_objects()?;
        if filtered {
            if !objects.sequences.is_empty() || !objects.views.is_empty() {
                objects
                    .not_carried
                    .push("sequences and views: not written by a dump restricted with --tables".to_string());
            }
            objects.sequences.clear();
            objects.views.clear();
            objects.indexes.retain(|i| in_dump(&i.table));
            objects.identity_columns.retain(|(t, _)| in_dump(t));
            objects.comments.retain(|c| c.table.as_deref().is_some_and(in_dump));
        }
        // Postgres: anything without a portable spelling is a comment.
        let mut unportable_indexes: Vec<String> = Vec::new();
        if postgres {
            objects.indexes.retain(|i| {
                let portable = !matches!(i.method.as_str(), "gin" | "gist") || i.key.contains('(');
                if !portable {
                    unportable_indexes.push(format!(
                        "-- NOT PORTABLE: {} (a {} index on a plain column needs an operator class PostgreSQL cannot infer)",
                        sql_text::sanitize_comment(&sql_text::index_dump_ddl(i)),
                        i.method
                    ));
                }
                portable
            });
            for line in &unportable_indexes {
                objects
                    .not_carried
                    .push(line.trim_start_matches("-- NOT PORTABLE: ").to_string());
            }
        }
        for line in &objects.not_carried {
            writeln!(writer, "-- NOT CARRIED: {}", sql_text::sanitize_comment(line)).map_err(w)?;
        }

        let mut schemas: Vec<(String, Schema)> = Vec::with_capacity(tables.len());
        for table in &tables {
            schemas.push((table.clone(), db.get_table_schema(table)?));
        }

        if postgres && write_ddl {
            let has_vector = schemas.iter().any(|(_, s)| {
                s.columns
                    .iter()
                    .any(|c| matches!(c.data_type, crate::DataType::Vector(_)))
            });
            if has_vector {
                writeln!(writer, "CREATE EXTENSION IF NOT EXISTS vector;").map_err(w)?;
            }
        }

        if write_ddl && !objects.sequences.is_empty() {
            writeln!(writer, "-- Sequences").map_err(w)?;
            for seq in &objects.sequences {
                writeln!(writer, "{}", sql_text::sequence_ddl(seq)).map_err(w)?;
            }
            writeln!(writer).map_err(w)?;
        }

        // Phase 1 — schemas. Collected FKs are held back for phase 3.
        let mut deferred_foreign_keys: Vec<String> = Vec::new();
        for (table, schema) in &schemas {
            let constraints = db.get_table_constraints(table)?;
            for fk in &constraints.foreign_keys {
                if in_dump(&fk.references_table) {
                    deferred_foreign_keys.push(sql_text::foreign_key_ddl(table, fk));
                } else {
                    deferred_foreign_keys.push(format!(
                        "-- NOT CARRIED (references a table outside --tables): {}",
                        sql_text::sanitize_comment(&sql_text::foreign_key_ddl(table, fk))
                    ));
                }
            }
            if !write_ddl {
                continue;
            }
            writeln!(writer, "-- Table: {}", sql_text::sanitize_comment(table)).map_err(w)?;
            // `table_ddl` FAILS rather than writing a CHECK body or a DEFAULT
            // it cannot spell: a dump whose first statement does not parse
            // restores nothing at all, so the export must stop here, naming
            // the constraint, instead of producing that file (HDB-005).
            writeln!(
                writer,
                "{}",
                sql_text::table_ddl_with_identity(table, schema, &constraints, objects.identity_for(table))?
            )
            .map_err(w)?;
        }
        if write_ddl {
            writeln!(writer).map_err(w)?;
        }

        // Phase 2 — data.
        let mut total_rows = 0u64;
        if write_data {
            for (table, schema) in &schemas {
                // `DatabaseInterface::scan_table` returns MATERIALIZED values:
                // the embedded implementation goes through
                // `StorageEngine::scan_table_with_schema`, whose decode step
                // already resolves every per-column storage reference
                // (DICTIONARY / CAS / columnar) back to the stored value.
                // `sql_text::value_literal` still refuses a `DictRef`/`CasRef`/
                // `ColumnarRef` outright rather than emitting a placeholder.
                let rows = db.scan_table(table)?;
                if rows.is_empty() {
                    continue;
                }
                total_rows += rows.len() as u64;
                writeln!(writer, "-- Data: {}", sql_text::sanitize_comment(table)).map_err(w)?;
                for chunk in rows.chunks(sql_text::INSERT_BATCH_ROWS) {
                    let statement = sql_text::insert_statement_batch(table, schema, chunk)?;
                    writeln!(writer, "{}", statement).map_err(w)?;
                }
                writeln!(writer).map_err(w)?;
            }
        }
        metadata.total_rows = total_rows;

        // Phase 3 — foreign keys, once every table and row exists.
        if write_ddl && !deferred_foreign_keys.is_empty() {
            writeln!(writer, "-- Foreign keys").map_err(w)?;
            for statement in &deferred_foreign_keys {
                writeln!(writer, "{}", statement).map_err(w)?;
            }
            writeln!(writer).map_err(w)?;
        }

        // Phase 4 — sequence positions, now that the data is in.
        if write_data {
            for seq in &objects.sequences {
                if let Some(statement) = sql_text::sequence_setval(seq) {
                    writeln!(writer, "{}", statement).map_err(w)?;
                }
            }
            // PostgreSQL's SERIAL sequence does not move when a row brings its
            // own key: move it past the loaded keys.
            if postgres {
                for (table, columns) in &objects.identity_columns {
                    for column in columns {
                        writeln!(writer, "{}", sql_text::postgres_serial_catch_up(table, column)).map_err(w)?;
                    }
                }
            }
        }

        if !write_ddl {
            return Ok(());
        }

        // Phase 5 — indexes, after the data so a UNIQUE index validates the
        // loaded rows. The schema objects carry every user index; a per-table
        // entry of the same name is not written twice.
        let mut wrote_index_header = false;
        for (table, _) in &schemas {
            for index in db.get_table_indexes(table)? {
                if objects.indexes.iter().any(|i| i.name == index.name)
                    || unportable_indexes
                        .iter()
                        .any(|l| l.contains(&sql_text::quote_ident(&index.name)))
                {
                    continue;
                }
                if !wrote_index_header {
                    writeln!(writer, "-- Indexes").map_err(w)?;
                    wrote_index_header = true;
                }
                writeln!(writer, "{}", sql_text::index_ddl(table, &index)).map_err(w)?;
            }
        }
        for index in &objects.indexes {
            if !wrote_index_header {
                writeln!(writer, "-- Indexes").map_err(w)?;
                wrote_index_header = true;
            }
            writeln!(writer, "{}", sql_text::index_dump_ddl(index)).map_err(w)?;
        }
        for line in &unportable_indexes {
            writeln!(writer, "{}", line).map_err(w)?;
        }

        // Phase 6 — views, last: they read the tables (and earlier views).
        if !objects.views.is_empty() {
            writeln!(writer, "\n-- Views").map_err(w)?;
            for view in &objects.views {
                writeln!(writer, "{}", sql_text::view_ddl(view)).map_err(w)?;
            }
        }

        // Phase 7 — comments, once everything they describe exists.
        if !objects.comments.is_empty() {
            writeln!(writer, "\n-- Comments").map_err(w)?;
            for comment in &objects.comments {
                writeln!(writer, "{}", comment.statement).map_err(w)?;
            }
        }

        Ok(())
    }

    /// Restore database (CLI-compatible wrapper)
    ///
    /// This is a simplified interface for CLI use. For more control,
    /// use `restore_from_dump` directly.
    pub fn restore<D: DatabaseRestoreInterface>(&self, opts: &RestoreOptions, db: &mut D) -> Result<RestoreReport> {
        let start_time = Instant::now();

        let stats = self.restore_from_dump_stats(
            &opts.input_path,
            db,
            RestoreValidation::from_validate_flag(opts.validate_constraints),
        )?;

        let duration_ms = start_time.elapsed().as_millis() as u64;

        Ok(RestoreReport {
            tables_restored: stats.tables,
            rows_restored: stats.rows,
            constraints_restored: stats.constraints,
            validation_warnings: stats.warnings,
            duration_ms,
        })
    }

    /// Create a full dump of the database
    ///
    /// Serializes all tables, indexes, and metadata to a dump file with compression.
    ///
    /// # Arguments
    /// * `output_path` - Path to output dump file
    /// * `db` - Database interface for reading data
    ///
    /// # Returns
    /// Metadata about the created dump including size and checksum
    pub fn create_full_dump<D: DatabaseInterface>(&self, output_path: &Path, db: &D) -> Result<DumpMetadata> {
        let start_time = Instant::now();
        let dump_id = self.get_next_dump_id();
        let mut metadata = DumpMetadata::new(dump_id, DumpType::Full);

        info!("Starting full dump {} to {}", dump_id, output_path.display());

        // Open dump file
        let file =
            File::create(output_path).map_err(|e| Error::storage(format!("Failed to create dump file: {}", e)))?;
        let mut writer = BufWriter::with_capacity(256 * 1024, file); // 256KB buffer

        // Write magic bytes and version
        writer
            .write_all(DUMP_MAGIC_NUMBER)
            .map_err(|e| Error::storage(format!("Failed to write magic bytes: {}", e)))?;
        writer
            .write_all(&DUMP_VERSION.to_le_bytes())
            .map_err(|e| Error::storage(format!("Failed to write version: {}", e)))?;

        // Reserve space for metadata header (we'll write it later)
        let metadata_pos = writer
            .stream_position()
            .map_err(|e| Error::storage(format!("Failed to get position: {}", e)))?;
        let metadata_placeholder = vec![0u8; 8192]; // 8KB placeholder
        writer
            .write_all(&metadata_placeholder)
            .map_err(|e| Error::storage(format!("Failed to write placeholder: {}", e)))?;

        // Record the batch encoding in the header so a restore decompresses
        // with the compression the FILE was written with, not whatever the
        // restoring manager happens to be configured with (HDB-003 1b).
        metadata.compression = Some(self.compression);

        // Get all tables
        let tables = db.list_tables()?;
        metadata.table_count = tables.len() as u32;
        // v10.12 (format v3): written as one SCHM section before ENDD.
        let objects = db.get_schema_objects()?;

        let mut total_rows = 0u64;
        let mut uncompressed_bytes = 0u64;

        // Dump each table
        for (idx, table) in tables.iter().enumerate() {
            debug!("Dumping table {}/{}: {}", idx + 1, tables.len(), table);

            // Write table marker
            writer
                .write_all(b"TABL")
                .map_err(|e| Error::storage(format!("Failed to write table marker: {}", e)))?;

            // Write table name
            let table_bytes = table.as_bytes();
            writer
                .write_all(&(table_bytes.len() as u32).to_le_bytes())
                .map_err(|e| Error::storage(format!("Failed to write table name length: {}", e)))?;
            writer
                .write_all(table_bytes)
                .map_err(|e| Error::storage(format!("Failed to write table name: {}", e)))?;

            // Get and write schema
            let schema = db.get_table_schema(table)?;
            let schema_bytes = bincode::serialize(&schema)
                .map_err(|e| Error::storage(format!("Failed to serialize schema: {}", e)))?;
            writer
                .write_all(&(schema_bytes.len() as u32).to_le_bytes())
                .map_err(|e| Error::storage(format!("Failed to write schema length: {}", e)))?;
            writer
                .write_all(&schema_bytes)
                .map_err(|e| Error::storage(format!("Failed to write schema: {}", e)))?;

            uncompressed_bytes += schema_bytes.len() as u64;

            // Get and write indexes. v3: an index the schema objects carry is
            // recreated from the SCHM section after the data instead.
            let mut indexes = db.get_table_indexes(table).unwrap_or_default();
            indexes.retain(|i| !objects.indexes.iter().any(|o| o.name == i.name));
            let indexes_bytes = bincode::serialize(&indexes)
                .map_err(|e| Error::storage(format!("Failed to serialize indexes: {}", e)))?;
            writer
                .write_all(&(indexes_bytes.len() as u32).to_le_bytes())
                .map_err(|e| Error::storage(format!("Failed to write indexes length: {}", e)))?;
            writer
                .write_all(&indexes_bytes)
                .map_err(|e| Error::storage(format!("Failed to write indexes: {}", e)))?;

            uncompressed_bytes += indexes_bytes.len() as u64;

            // Format v2: the table's FK / CHECK / UNIQUE constraints, straight
            // after the index blob and before the row count.
            uncompressed_bytes += Self::write_table_constraints(&mut writer, db, table)?;

            // Scan and write rows
            let rows = db.scan_table(table)?;
            let row_count = rows.len() as u64;
            total_rows += row_count;

            writer
                .write_all(&row_count.to_le_bytes())
                .map_err(|e| Error::storage(format!("Failed to write row count: {}", e)))?;

            // Write rows in batches for better compression
            const BATCH_SIZE: usize = 1000;
            for batch in rows.chunks(BATCH_SIZE) {
                let batch_bytes = bincode::serialize(batch)
                    .map_err(|e| Error::storage(format!("Failed to serialize batch: {}", e)))?;

                uncompressed_bytes += batch_bytes.len() as u64;

                // Compress batch
                let compressed = self.compress_data(&batch_bytes)?;

                writer
                    .write_all(&(compressed.len() as u32).to_le_bytes())
                    .map_err(|e| Error::storage(format!("Failed to write batch length: {}", e)))?;
                writer
                    .write_all(&compressed)
                    .map_err(|e| Error::storage(format!("Failed to write batch: {}", e)))?;
            }

            // Write end-of-table marker
            writer
                .write_all(&0u32.to_le_bytes())
                .map_err(|e| Error::storage(format!("Failed to write EOT marker: {}", e)))?;
        }

        // Format v3: the schema objects, after every table.
        if !objects.is_empty() {
            let mut objects_bytes = bincode::serialize(&objects)
                .map_err(|e| Error::storage(format!("Failed to serialize schema objects: {}", e)))?;
            // The comments follow the schema objects' own bytes (see `comments`).
            if !objects.comments.is_empty() {
                objects_bytes.extend(
                    bincode::serialize(&objects.comments)
                        .map_err(|e| Error::storage(format!("Failed to serialize comments: {}", e)))?,
                );
            }
            writer
                .write_all(b"SCHM")
                .map_err(|e| Error::storage(format!("Failed to write schema marker: {}", e)))?;
            writer
                .write_all(&(objects_bytes.len() as u32).to_le_bytes())
                .map_err(|e| Error::storage(format!("Failed to write schema objects length: {}", e)))?;
            writer
                .write_all(&objects_bytes)
                .map_err(|e| Error::storage(format!("Failed to write schema objects: {}", e)))?;
            uncompressed_bytes += objects_bytes.len() as u64;
        }

        metadata.total_rows = total_rows;
        metadata.uncompressed_size = uncompressed_bytes;

        // Write end-of-dump marker
        writer
            .write_all(b"ENDD")
            .map_err(|e| Error::storage(format!("Failed to write end marker: {}", e)))?;

        // Flush and calculate checksum
        writer
            .flush()
            .map_err(|e| Error::storage(format!("Failed to flush writer: {}", e)))?;
        drop(writer);

        let checksum = self.calculate_checksum(output_path)?;
        metadata.checksum = checksum;

        let file_size = std::fs::metadata(output_path)
            .map_err(|e| Error::storage(format!("Failed to get file metadata: {}", e)))?
            .len();
        metadata.compressed_size = file_size;

        // Write metadata to header
        self.write_metadata_header(output_path, metadata_pos, &metadata)?;

        // Update history and clear dirty state
        self.dump_history.write().push(metadata.clone());
        self.dirty_tracker.clear_dirty();
        *self.last_dump_time.lock() = Instant::now();

        let elapsed = start_time.elapsed();
        let throughput_mbps = (metadata.uncompressed_size as f64 / 1_048_576.0) / elapsed.as_secs_f64();

        info!(
            "Full dump {} completed: {} tables, {} rows, {:.2} MB in {:.2}s ({:.2} MB/s)",
            dump_id,
            metadata.table_count,
            metadata.total_rows,
            metadata.uncompressed_size as f64 / 1_048_576.0,
            elapsed.as_secs_f64(),
            throughput_mbps
        );

        Ok(metadata)
    }

    /// Create an incremental dump
    ///
    /// Dumps only the tables that have changed since the last dump.
    ///
    /// # Arguments
    /// * `output_path` - Path to output dump file
    /// * `db` - Database interface for reading data
    /// * `append` - If true, append to existing dump file; if false, create new file
    ///
    /// # Returns
    /// Metadata about the created dump
    pub fn create_incremental_dump<D: DatabaseInterface>(
        &self,
        output_path: &Path,
        db: &D,
        append: bool,
    ) -> Result<DumpMetadata> {
        let dirty_tables = self.dirty_tracker.get_dirty_tables();

        if dirty_tables.is_empty() {
            return Err(Error::storage("No dirty tables to dump"));
        }

        let start_time = Instant::now();
        let dump_id = self.get_next_dump_id();
        let mut metadata = DumpMetadata::new(dump_id, DumpType::Incremental);

        info!("Starting incremental dump {} (append={})", dump_id, append);

        // Appending keeps the EXISTING file's section shape: a format-v1 file's
        // TABL sections carry no constraint blob, and one file must not mix the
        // two shapes or the reader — which decides by the header version — will
        // mis-parse everything after the first appended section.
        let appending = append && output_path.exists();
        let section_version = if appending {
            Self::read_file_version(output_path).unwrap_or(DUMP_VERSION)
        } else {
            DUMP_VERSION
        };

        // Open file in append or create mode
        let file = if appending {
            OpenOptions::new()
                .append(true)
                .open(output_path)
                .map_err(|e| Error::storage(format!("Failed to open dump file: {}", e)))?
        } else {
            File::create(output_path).map_err(|e| Error::storage(format!("Failed to create dump file: {}", e)))?
        };

        let mut writer = BufWriter::with_capacity(256 * 1024, file);

        // `appending`, NOT `!append || !output_path.exists()`: `File::create`
        // above has already made the path exist, so the stale expression was
        // false on BOTH sides for `append = true` on a non-existent file and
        // skipped the header entirely — producing a file that starts with
        // `INCR` and fails every restore with "bad magic bytes".
        if !appending {
            // Write file header for new file
            writer
                .write_all(DUMP_MAGIC_NUMBER)
                .map_err(|e| Error::storage(format!("Failed to write magic bytes: {}", e)))?;
            writer
                .write_all(&DUMP_VERSION.to_le_bytes())
                .map_err(|e| Error::storage(format!("Failed to write version: {}", e)))?;

            // Reserve metadata space
            let metadata_placeholder = vec![0u8; 8192];
            writer
                .write_all(&metadata_placeholder)
                .map_err(|e| Error::storage(format!("Failed to write placeholder: {}", e)))?;
        }

        // Write incremental marker
        writer
            .write_all(b"INCR")
            .map_err(|e| Error::storage(format!("Failed to write incremental marker: {}", e)))?;

        // As in `create_full_dump` — but note that this writer never fills the
        // 8 KB placeholder in, so the value only reaches the dump HISTORY, not
        // the file. Restore's frame detection covers the header-less file.
        metadata.compression = Some(self.compression);
        metadata.table_count = dirty_tables.len() as u32;

        let mut total_rows = 0u64;
        let mut uncompressed_bytes = 0u64;

        // Dump dirty tables only
        for table in &dirty_tables {
            debug!("Dumping dirty table: {}", table);

            // Write table marker
            writer
                .write_all(b"TABL")
                .map_err(|e| Error::storage(format!("Failed to write table marker: {}", e)))?;

            let table_bytes = table.as_bytes();
            writer
                .write_all(&(table_bytes.len() as u32).to_le_bytes())
                .map_err(|e| Error::storage(format!("Failed to write table name length: {}", e)))?;
            writer
                .write_all(table_bytes)
                .map_err(|e| Error::storage(format!("Failed to write table name: {}", e)))?;

            let schema = db.get_table_schema(table)?;
            let schema_bytes = bincode::serialize(&schema)
                .map_err(|e| Error::storage(format!("Failed to serialize schema: {}", e)))?;
            writer
                .write_all(&(schema_bytes.len() as u32).to_le_bytes())
                .map_err(|e| Error::storage(format!("Failed to write schema length: {}", e)))?;
            writer
                .write_all(&schema_bytes)
                .map_err(|e| Error::storage(format!("Failed to write schema: {}", e)))?;

            uncompressed_bytes += schema_bytes.len() as u64;

            let indexes = db.get_table_indexes(table).unwrap_or_default();
            let indexes_bytes = bincode::serialize(&indexes)
                .map_err(|e| Error::storage(format!("Failed to serialize indexes: {}", e)))?;
            writer
                .write_all(&(indexes_bytes.len() as u32).to_le_bytes())
                .map_err(|e| Error::storage(format!("Failed to write indexes length: {}", e)))?;
            writer
                .write_all(&indexes_bytes)
                .map_err(|e| Error::storage(format!("Failed to write indexes: {}", e)))?;

            uncompressed_bytes += indexes_bytes.len() as u64;

            // Format v2: same TABL layout as the full writer — constraints
            // blob between the index blob and the row count. Skipped when
            // appending to a v1 file (see `section_version`).
            if section_version >= 2 {
                uncompressed_bytes += Self::write_table_constraints(&mut writer, db, table)?;
            }

            let rows = db.scan_table(table)?;
            let row_count = rows.len() as u64;
            total_rows += row_count;

            writer
                .write_all(&row_count.to_le_bytes())
                .map_err(|e| Error::storage(format!("Failed to write row count: {}", e)))?;

            for batch in rows.chunks(1000) {
                let batch_bytes = bincode::serialize(batch)
                    .map_err(|e| Error::storage(format!("Failed to serialize batch: {}", e)))?;

                uncompressed_bytes += batch_bytes.len() as u64;
                let compressed = self.compress_data(&batch_bytes)?;

                writer
                    .write_all(&(compressed.len() as u32).to_le_bytes())
                    .map_err(|e| Error::storage(format!("Failed to write batch length: {}", e)))?;
                writer
                    .write_all(&compressed)
                    .map_err(|e| Error::storage(format!("Failed to write batch: {}", e)))?;
            }

            writer
                .write_all(&0u32.to_le_bytes())
                .map_err(|e| Error::storage(format!("Failed to write EOT marker: {}", e)))?;
        }

        metadata.total_rows = total_rows;
        metadata.uncompressed_size = uncompressed_bytes;
        metadata.append_count = if append { 1 } else { 0 };

        writer
            .flush()
            .map_err(|e| Error::storage(format!("Failed to flush writer: {}", e)))?;
        drop(writer);

        let checksum = self.calculate_checksum(output_path)?;
        metadata.checksum = checksum;

        let file_size = std::fs::metadata(output_path)
            .map_err(|e| Error::storage(format!("Failed to get file metadata: {}", e)))?
            .len();
        metadata.compressed_size = file_size;

        self.dump_history.write().push(metadata.clone());
        self.dirty_tracker.clear_dirty();

        let elapsed = start_time.elapsed();
        info!(
            "Incremental dump {} completed: {} tables, {} rows in {:.2}s",
            dump_id,
            metadata.table_count,
            metadata.total_rows,
            elapsed.as_secs_f64()
        );

        Ok(metadata)
    }

    /// Restore a database from a dump file.
    ///
    /// Replays the tables, indexes, rows and — for format v2 — the FK / CHECK /
    /// UNIQUE constraints of a binary dump into `db`, in three phases:
    ///
    /// * **A** — for every `TABL` section: create the table, recreate its
    ///   indexes, insert its rows, and COLLECT its constraint record. The
    ///   constraints cannot be applied here: a child table may appear before
    ///   its parent in the file, and an FK graph may be cyclic.
    /// * **B** — after the `ENDD` marker, register every collected constraint
    ///   record ([`DatabaseRestoreInterface::restore_table_constraints`]).
    /// * **C** — with the whole graph registered, validate the restored ROWS
    ///   against it ([`DatabaseRestoreInterface::validate_restored_constraints`]).
    ///
    /// A dump whose data violates its own constraints fails the restore with a
    /// [`Error::constraint_violation`] naming EVERY offending table, constraint
    /// and key (the tables are aggregated into one error, so a multi-table
    /// dump does not have to be fixed one restore at a time) — reporting
    /// success would hand back a database that is invalid by its own rules.
    ///
    /// The failure does NOT roll anything back: the target holds every restored
    /// table and row and every registered constraint, so it rejects NEW
    /// violations while keeping the data the operator asked for. To accept such
    /// a dump deliberately — a backup of a database run with a `NOT ENFORCED` /
    /// `LOCK-FREE` constraint or `SET helios.fk_validation = 'audit'`, which
    /// legitimately holds violating rows — restore through
    /// [`Self::restore`] with `RestoreOptions::validate_constraints = false`
    /// (`heliosdb-nano restore --no-validate`): the violations are then
    /// reported as `RestoreReport::validation_warnings`.
    ///
    /// Version-1 dumps (everything written before v4.35) carry no constraint
    /// blob: they restore exactly as before, with schema, indexes and rows but
    /// no FK/CHECK/UNIQUE records. Recreate those from your DDL.
    ///
    /// # Arguments
    /// * `input_path` - Path to dump file
    /// * `db` - Database interface for writing data
    ///
    /// # Returns
    /// Ok(()) on success
    pub fn restore_from_dump<D: DatabaseRestoreInterface>(&self, input_path: &Path, db: &mut D) -> Result<()> {
        self.restore_from_dump_stats(input_path, db, RestoreValidation::Strict)
            .map(|_| ())
    }

    /// [`Self::restore_from_dump`] plus the counters `restore` reports.
    ///
    /// Split out so the public entry point keeps its `Result<()>` signature
    /// while the CLI wrapper can report what was actually restored (the report
    /// used to be hard-coded to zero tables and zero rows).
    fn restore_from_dump_stats<D: DatabaseRestoreInterface>(
        &self,
        input_path: &Path,
        db: &mut D,
        validation: RestoreValidation,
    ) -> Result<RestoreStats> {
        info!("Starting restore from {}", input_path.display());

        // Validate dump first
        self.validate_dump(input_path)?;

        // Ask the target whether it can be restored into AT ALL, before the
        // first table is created — a refusal here leaves the target untouched.
        db.begin_restore()?;

        // Drop anything a previous (possibly failed) restore left behind, so
        // this report only carries this restore's notes.
        let _ = db.take_restore_warnings();

        // The compression the FILE records. `None` for a v1 file (the header
        // had no such field) and for a header-less incremental dump; see
        // `decompress_batch`.
        let recorded_compression = self.read_header_compression(input_path);

        // Open dump file
        let file = File::open(input_path).map_err(|e| Error::storage(format!("Failed to open dump file: {}", e)))?;
        let mut reader = BufReader::with_capacity(256 * 1024, file);

        // Read and verify magic bytes
        let mut magic = [0u8; 8];
        reader
            .read_exact(&mut magic)
            .map_err(|e| Error::storage(format!("Failed to read magic bytes: {}", e)))?;
        if &magic != DUMP_MAGIC_NUMBER {
            return Err(Error::storage("Invalid dump file: bad magic bytes"));
        }

        // Read version. Every version up to the current one is readable; only a
        // FUTURE one is refused.
        let mut version_bytes = [0u8; 4];
        reader
            .read_exact(&mut version_bytes)
            .map_err(|e| Error::storage(format!("Failed to read version: {}", e)))?;
        let version = u32::from_le_bytes(version_bytes);
        if version == 0 || version > DUMP_VERSION {
            return Err(Error::storage(format!("Unsupported dump version: {}", version)));
        }

        // Skip metadata header
        reader
            .seek(SeekFrom::Current(8192))
            .map_err(|e| Error::storage(format!("Failed to seek past metadata: {}", e)))?;

        let mut total_tables = 0;
        let mut total_rows = 0u64;
        // Phase A collects these; phases B and C run after the end marker.
        let mut pending_constraints: Vec<(String, crate::sql::TableConstraints)> = Vec::new();
        let mut restored_tables: Vec<String> = Vec::new();
        // Format v3's SCHM section; replayed after phase B.
        let mut schema_objects: Option<SchemaObjects> = None;

        // Read tables until end marker
        loop {
            // Read marker
            let mut marker = [0u8; 4];
            if reader.read_exact(&mut marker).is_err() {
                break; // EOF
            }

            match &marker {
                b"ENDD" => {
                    debug!("Reached end-of-dump marker");
                    break;
                }
                b"INCR" => {
                    debug!("Found incremental marker, continuing...");
                    continue;
                }
                b"TABL" => {
                    // Table data follows
                }
                b"SCHM" if version >= 3 => {
                    let mut len_bytes = [0u8; 4];
                    reader
                        .read_exact(&mut len_bytes)
                        .map_err(|e| Error::storage(format!("Failed to read schema objects length: {}", e)))?;
                    let mut bytes = vec![0u8; u32::from_le_bytes(len_bytes) as usize];
                    reader
                        .read_exact(&mut bytes)
                        .map_err(|e| Error::storage(format!("Failed to read schema objects: {}", e)))?;
                    let mut objects: SchemaObjects = bincode::deserialize(&bytes)
                        .map_err(|e| Error::storage(format!("Failed to deserialize schema objects: {}", e)))?;
                    // What follows the schema objects' own bytes: the comments.
                    let used = bincode::serialized_size(&objects)
                        .ok()
                        .and_then(|used| usize::try_from(used).ok())
                        .unwrap_or(bytes.len());
                    if let Some(rest) = bytes.get(used..).filter(|rest| !rest.is_empty()) {
                        objects.comments = bincode::deserialize(rest)
                            .map_err(|e| Error::storage(format!("Failed to deserialize comments: {}", e)))?;
                    }
                    schema_objects = Some(objects);
                    continue;
                }
                _ => {
                    return Err(Error::storage(format!("Invalid marker: {:?}", marker)));
                }
            }

            // Read table name
            let mut len_bytes = [0u8; 4];
            reader
                .read_exact(&mut len_bytes)
                .map_err(|e| Error::storage(format!("Failed to read table name length: {}", e)))?;
            let table_name_len = u32::from_le_bytes(len_bytes);

            let mut table_bytes = vec![0u8; table_name_len as usize];
            reader
                .read_exact(&mut table_bytes)
                .map_err(|e| Error::storage(format!("Failed to read table name: {}", e)))?;
            let table =
                String::from_utf8(table_bytes).map_err(|e| Error::storage(format!("Invalid table name: {}", e)))?;

            debug!("Restoring table: {}", table);

            // Read schema
            let mut schema_len_bytes = [0u8; 4];
            reader
                .read_exact(&mut schema_len_bytes)
                .map_err(|e| Error::storage(format!("Failed to read schema length: {}", e)))?;
            let schema_len = u32::from_le_bytes(schema_len_bytes);

            let mut schema_bytes = vec![0u8; schema_len as usize];
            reader
                .read_exact(&mut schema_bytes)
                .map_err(|e| Error::storage(format!("Failed to read schema: {}", e)))?;
            let schema: Schema = bincode::deserialize(&schema_bytes)
                .map_err(|e| Error::storage(format!("Failed to deserialize schema: {}", e)))?;

            // Read indexes
            let mut indexes_len_bytes = [0u8; 4];
            reader
                .read_exact(&mut indexes_len_bytes)
                .map_err(|e| Error::storage(format!("Failed to read indexes length: {}", e)))?;
            let indexes_len = u32::from_le_bytes(indexes_len_bytes);

            let mut indexes_bytes = vec![0u8; indexes_len as usize];
            reader
                .read_exact(&mut indexes_bytes)
                .map_err(|e| Error::storage(format!("Failed to read indexes: {}", e)))?;
            let indexes: Vec<IndexMetadata> = bincode::deserialize(&indexes_bytes)
                .map_err(|e| Error::storage(format!("Failed to deserialize indexes: {}", e)))?;

            // Read the constraints blob — v2 and later ONLY. A v1 file has no
            // such blob, and reading one would consume the row count.
            let constraints = if version >= 2 {
                let mut constraints_len_bytes = [0u8; 4];
                reader
                    .read_exact(&mut constraints_len_bytes)
                    .map_err(|e| Error::storage(format!("Failed to read constraints length: {}", e)))?;
                let constraints_len = u32::from_le_bytes(constraints_len_bytes);

                let mut constraints_bytes = vec![0u8; constraints_len as usize];
                reader
                    .read_exact(&mut constraints_bytes)
                    .map_err(|e| Error::storage(format!("Failed to read constraints: {}", e)))?;
                bincode::deserialize(&constraints_bytes)
                    .map_err(|e| Error::storage(format!("Failed to deserialize constraints: {}", e)))?
            } else {
                crate::sql::TableConstraints::default()
            };

            // Create table
            db.create_table(&table, schema)?;

            // Restore indexes
            for index in indexes {
                db.create_index(&table, &index)?;
            }

            // Read row count
            let mut row_count_bytes = [0u8; 8];
            reader
                .read_exact(&mut row_count_bytes)
                .map_err(|e| Error::storage(format!("Failed to read row count: {}", e)))?;
            let row_count = u64::from_le_bytes(row_count_bytes);

            // Read batches
            let mut rows_read = 0u64;
            loop {
                let mut batch_len_bytes = [0u8; 4];
                reader
                    .read_exact(&mut batch_len_bytes)
                    .map_err(|e| Error::storage(format!("Failed to read batch length: {}", e)))?;
                let batch_len = u32::from_le_bytes(batch_len_bytes);

                if batch_len == 0 {
                    // End of table marker
                    break;
                }

                let mut batch_bytes = vec![0u8; batch_len as usize];
                reader
                    .read_exact(&mut batch_bytes)
                    .map_err(|e| Error::storage(format!("Failed to read batch: {}", e)))?;

                // Decompress batch with the compression the FILE records.
                let decompressed = self.decompress_batch(&batch_bytes, recorded_compression)?;

                // Deserialize batch
                let batch: Vec<Tuple> = bincode::deserialize(&decompressed)
                    .map_err(|e| Error::storage(format!("Failed to deserialize batch: {}", e)))?;

                rows_read += batch.len() as u64;

                // Insert rows
                for row in batch {
                    db.insert_row(&table, row)?;
                }
            }

            if rows_read != row_count {
                warn!(
                    "Row count mismatch for table {}: expected {}, got {}",
                    table, row_count, rows_read
                );
            }

            if !restored_tables.iter().any(|t| t == &table) {
                restored_tables.push(table.clone());
            }
            pending_constraints.push((table, constraints));

            total_tables += 1;
            total_rows += rows_read;
        }

        // Phase B: every table exists and is populated, so a child that sorted
        // before its parent — or a cycle — can now be wired up.
        let mut constraints_restored = 0u64;
        for (table, constraints) in &pending_constraints {
            let count = constraints.foreign_keys.len()
                + constraints.check_constraints.len()
                + constraints.unique_constraints.len();
            if count == 0 {
                continue;
            }
            db.restore_table_constraints(table, constraints)?;
            constraints_restored += count as u64;
        }

        // v10.12: sequences, identity columns, indexes and views — after the
        // rows and the constraints, so a UNIQUE index validates the data.
        if let Some(objects) = &schema_objects {
            db.restore_schema_objects(objects)?;
        }

        // Phase C: validate the restored rows against the COMPLETE graph.
        // Every table is validated even after one fails, so an operator fixing
        // a bad dump sees all of it at once rather than one table per attempt.
        let mut validation_failures: Vec<String> = Vec::new();
        for table in &restored_tables {
            if let Err(e) = db.validate_restored_constraints(table, validation) {
                validation_failures.push(e.to_string());
            }
        }

        // Drained BEFORE the failure return: these notes are exactly the
        // diagnostics that explain a failed restore, and the next restore would
        // otherwise throw them away.
        let warnings = db.take_restore_warnings();
        for warning in &warnings {
            warn!("Restore warning: {}", warning);
        }

        if !validation_failures.is_empty() {
            for failure in &validation_failures {
                warn!("Restore validation failure: {}", failure);
            }
            let mut message = format!("restore validation failed: {}", validation_failures.join("; "));
            if !warnings.is_empty() {
                message.push_str(&format!(
                    " ({} {}: {})",
                    warnings.len(),
                    if warnings.len() == 1 { "warning" } else { "warnings" },
                    warnings.join("; ")
                ));
            }
            // Nothing is rolled back: say so, because the target directory now
            // holds a complete restore that the caller is being told failed.
            message.push_str(
                ". The target contains the restored tables and rows; their constraints are registered and \
                 reject new violations. Restore with --no-validate to accept the data as it is.",
            );
            return Err(Error::constraint_violation(message));
        }

        info!(
            "Restore completed: {} tables, {} rows, {} constraints",
            total_tables, total_rows, constraints_restored
        );

        Ok(RestoreStats {
            tables: total_tables,
            rows: total_rows,
            constraints: constraints_restored,
            warnings,
        })
    }

    /// List all dumps in history
    pub fn list_dumps(&self) -> Vec<DumpMetadata> {
        self.dump_history.read().clone()
    }

    /// Validate dump file integrity
    pub fn validate_dump(&self, path: &Path) -> Result<()> {
        if !path.exists() {
            return Err(Error::storage("Dump file does not exist"));
        }

        let file = File::open(path).map_err(|e| Error::storage(format!("Failed to open dump file: {}", e)))?;
        let mut reader = BufReader::new(file);

        // Verify magic bytes
        let mut magic = [0u8; 8];
        reader
            .read_exact(&mut magic)
            .map_err(|e| Error::storage(format!("Failed to read magic bytes: {}", e)))?;
        if &magic != DUMP_MAGIC_NUMBER {
            return Err(Error::storage("Invalid dump file: bad magic bytes"));
        }

        // Verify version
        let mut version_bytes = [0u8; 4];
        reader
            .read_exact(&mut version_bytes)
            .map_err(|e| Error::storage(format!("Failed to read version: {}", e)))?;
        let version = u32::from_le_bytes(version_bytes);
        // Same gate as the reader (`restore_from_dump_stats`): version 0 is not
        // a version this writer ever stamped, so a file claiming it is corrupt
        // rather than old. The two checks on the same file must agree.
        if version == 0 || version > DUMP_VERSION {
            return Err(Error::storage(format!("Unsupported dump version: {}", version)));
        }

        // Verify checksum
        drop(reader);
        let _checksum = self.calculate_checksum(path)?;

        debug!("Dump file validation passed: {}", path.display());

        Ok(())
    }

    /// Get dump metadata by ID
    pub fn get_dump_metadata(&self, dump_id: u64) -> Result<DumpMetadata> {
        self.dump_history
            .read()
            .iter()
            .find(|m| m.dump_id == dump_id)
            .cloned()
            .ok_or_else(|| Error::storage(format!("Dump {} not found", dump_id)))
    }

    /// Delete old dumps, keeping only the most recent N
    pub fn delete_old_dumps(&self, keep_count: usize) -> Result<()> {
        let mut history = self.dump_history.write();

        if history.len() <= keep_count {
            return Ok(());
        }

        // Sort by creation time (newest first)
        history.sort_by(|a, b| b.created_at.cmp(&a.created_at));

        // Remove old dumps
        let removed = history.drain(keep_count..).collect::<Vec<_>>();

        info!("Removed {} old dump(s) from history", removed.len());

        Ok(())
    }

    /// Get dirty tracker
    pub fn dirty_tracker(&self) -> &Arc<DirtyTracker> {
        &self.dirty_tracker
    }

    // Helper methods

    /// Compress data based on configuration
    fn compress_data(&self, data: &[u8]) -> Result<Vec<u8>> {
        match self.compression {
            CompressionType::None => Ok(data.to_vec()),
            CompressionType::Zstd => {
                zstd::bulk::compress(data, 3).map_err(|e| Error::compression(format!("Zstd compression failed: {}", e)))
            }
            CompressionType::Gzip | CompressionType::Brotli => {
                // For now, use zstd as fallback for unsupported types
                zstd::bulk::compress(data, 3).map_err(|e| Error::compression(format!("Compression failed: {}", e)))
            }
        }
    }

    /// Write one table's constraint blob — the format-v2 `TABL` section member
    /// that sits between the index blob and the row count.
    ///
    /// Returns the uncompressed byte count so both writers can keep their
    /// `uncompressed_bytes` total honest. Length-prefixed like the schema and
    /// index blobs, so a future reader can skip it without knowing the type.
    /// Never compressed: it is a handful of bytes and the reader needs it
    /// before it knows anything about batch encoding.
    fn write_table_constraints<W: Write, D: DatabaseInterface>(writer: &mut W, db: &D, table: &str) -> Result<u64> {
        let constraints = db.get_table_constraints(table)?;
        let bytes = bincode::serialize(&constraints)
            .map_err(|e| Error::storage(format!("Failed to serialize constraints: {}", e)))?;
        writer
            .write_all(&(bytes.len() as u32).to_le_bytes())
            .map_err(|e| Error::storage(format!("Failed to write constraints length: {}", e)))?;
        writer
            .write_all(&bytes)
            .map_err(|e| Error::storage(format!("Failed to write constraints: {}", e)))?;
        Ok(bytes.len() as u64)
    }

    /// The format version stamped in an existing dump file's header, or `None`
    /// when the file is unreadable or not a dump at all.
    fn read_file_version(path: &Path) -> Option<u32> {
        let mut file = File::open(path).ok()?;
        let mut magic = [0u8; 8];
        file.read_exact(&mut magic).ok()?;
        if &magic != DUMP_MAGIC_NUMBER {
            return None;
        }
        let mut version = [0u8; 4];
        file.read_exact(&mut version).ok()?;
        Some(u32::from_le_bytes(version))
    }

    /// The batch compression the dump file's own header records, or `None` when
    /// the file records nothing.
    ///
    /// Best-effort by design: a v1 file has no such field, and
    /// `create_incremental_dump` never fills its 8 KB placeholder in, so an
    /// unreadable header is normal rather than a restore-stopping error —
    /// `decompress_batch` falls back to frame detection.
    fn read_header_compression(&self, path: &Path) -> Option<CompressionType> {
        let file = File::open(path).ok()?;
        let mut reader = BufReader::new(file);
        // Magic (8) + version (4), then the length-prefixed metadata JSON that
        // `write_metadata_header` stamps over the placeholder.
        reader.seek(SeekFrom::Start(12)).ok()?;
        let mut len_bytes = [0u8; 4];
        reader.read_exact(&mut len_bytes).ok()?;
        let len = u32::from_le_bytes(len_bytes) as usize;
        if len == 0 || len > 8192 {
            return None;
        }
        let mut json = vec![0u8; len];
        reader.read_exact(&mut json).ok()?;
        let metadata: DumpMetadata = serde_json::from_slice(&json).ok()?;
        metadata.compression
    }

    /// Decompress one row batch, using the compression the FILE records rather
    /// than this manager's configured one.
    ///
    /// HDB-003 1b: restore used to call `decompress_data`, i.e. it decoded with
    /// whatever the RESTORING manager was configured with. A dump written by
    /// `dump_full_uncompressed` (compression `None`) therefore could not be
    /// restored by a manager built with the default zstd — the restore failed
    /// immediately after a successful dump.
    ///
    /// `compress_data` emits exactly two encodings — raw bytes for `None`, a
    /// zstd frame for everything else — so the 4-byte zstd frame magic is a
    /// complete discriminator, and it is the ONLY signal a file that records no
    /// compression (v1, or a header-less incremental dump) carries. A raw batch
    /// can never be mistaken for a frame: it is a bincode `Vec<Tuple>` whose
    /// first eight bytes are a little-endian element count of at most
    /// `BATCH_SIZE`, i.e. `xx 00 00 00 …`.
    fn decompress_batch(&self, data: &[u8], recorded: Option<CompressionType>) -> Result<Vec<u8>> {
        const ZSTD_FRAME_MAGIC: [u8; 4] = [0x28, 0xb5, 0x2f, 0xfd];
        if data.get(..4) == Some(ZSTD_FRAME_MAGIC.as_slice()) {
            return zstd::bulk::decompress(data, 100 * 1024 * 1024) // 100MB max
                .map_err(|e| Error::compression(format!("Zstd decompression failed: {}", e)));
        }
        if matches!(recorded, Some(c) if c != CompressionType::None) {
            debug!(
                "Dump records {:?} compression but this batch is not a zstd frame; reading it raw",
                recorded
            );
        }
        Ok(data.to_vec())
    }

    /// Decompress data with THIS manager's configured compression.
    ///
    /// Test-only since HDB-003: the restore path must decode with the
    /// compression the FILE records (`decompress_batch`), never the manager's.
    #[cfg(test)]
    fn decompress_data(&self, data: &[u8]) -> Result<Vec<u8>> {
        match self.compression {
            CompressionType::None => Ok(data.to_vec()),
            CompressionType::Zstd => {
                zstd::bulk::decompress(data, 100 * 1024 * 1024) // 100MB max
                    .map_err(|e| Error::compression(format!("Zstd decompression failed: {}", e)))
            }
            CompressionType::Gzip | CompressionType::Brotli => {
                // For now, use zstd as fallback
                zstd::bulk::decompress(data, 100 * 1024 * 1024)
                    .map_err(|e| Error::compression(format!("Decompression failed: {}", e)))
            }
        }
    }

    /// Calculate CRC32 checksum of file
    fn calculate_checksum(&self, path: &Path) -> Result<String> {
        let file = File::open(path).map_err(|e| Error::storage(format!("Failed to open file for checksum: {}", e)))?;
        let mut reader = BufReader::new(file);
        let mut buffer = vec![0u8; 8192];
        let mut hasher = crc32fast::Hasher::new();

        loop {
            let bytes_read = reader
                .read(&mut buffer)
                .map_err(|e| Error::storage(format!("Failed to read file: {}", e)))?;
            if bytes_read == 0 {
                break;
            }
            if let Some(data) = buffer.get(..bytes_read) {
                hasher.update(data);
            }
        }

        Ok(format!("{:08x}", hasher.finalize()))
    }

    /// Write metadata to file header
    fn write_metadata_header(&self, path: &Path, position: u64, metadata: &DumpMetadata) -> Result<()> {
        let file = OpenOptions::new()
            .write(true)
            .open(path)
            .map_err(|e| Error::storage(format!("Failed to open dump file: {}", e)))?;
        let mut writer = BufWriter::new(file);

        writer
            .seek(SeekFrom::Start(position))
            .map_err(|e| Error::storage(format!("Failed to seek to metadata position: {}", e)))?;

        let metadata_bytes =
            serde_json::to_vec(metadata).map_err(|e| Error::storage(format!("Failed to serialize metadata: {}", e)))?;

        // Write actual metadata length
        writer
            .write_all(&(metadata_bytes.len() as u32).to_le_bytes())
            .map_err(|e| Error::storage(format!("Failed to write metadata length: {}", e)))?;
        writer
            .write_all(&metadata_bytes)
            .map_err(|e| Error::storage(format!("Failed to write metadata: {}", e)))?;

        writer
            .flush()
            .map_err(|e| Error::storage(format!("Failed to flush writer: {}", e)))?;

        Ok(())
    }
}

// Mode and options types for CLI compatibility
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DumpMode {
    Full,
    Incremental,
}

/// Output format for dumps
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DumpOutputFormat {
    /// HeliosDB binary format (compressed)
    Binary,
    /// SQL text format (CREATE TABLE + INSERT)
    Sql,
}

/// v10.13: which SQL a `--format sql` dump is written in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SqlDialect {
    /// Today's output, restored with `EmbeddedDatabase::execute_sql_script`.
    #[default]
    Nano,
    /// Loadable by PostgreSQL's `psql -v ON_ERROR_STOP=1`: the pgvector
    /// extension is created when a VECTOR column exists, SERIAL counters are
    /// moved past the loaded keys, and anything with no portable form is
    /// written as a comment and listed in the header — never dropped silently.
    Postgres,
}

/// v10.13: what a SQL dump writes.
#[derive(Debug, Clone, Default)]
pub struct SqlDumpOptions {
    /// Output dialect.
    pub dialect: SqlDialect,
    /// Only these tables (then views and sequences are not written, and a
    /// foreign key to a table outside the set is written as a comment).
    pub tables: Option<Vec<String>>,
    /// DDL only, no rows and no sequence positions.
    pub schema_only: bool,
    /// Rows and sequence positions only, no DDL.
    pub data_only: bool,
}

#[derive(Debug, Clone)]
pub struct DumpOptions {
    pub output_path: PathBuf,
    pub mode: DumpMode,
    pub compression: CompressionType,
    pub append: bool,
    pub tables: Option<Vec<String>>,
    pub verbose: bool,
    pub connection: Option<String>,
    pub format: DumpOutputFormat,
    /// v10.13: dialect and section switches of a `--format sql` dump.
    /// `tables` above is the table filter for both.
    pub sql: SqlDumpOptions,
}

impl Default for DumpOptions {
    fn default() -> Self {
        Self {
            output_path: PathBuf::from("backup.heliodump"),
            mode: DumpMode::Full,
            compression: CompressionType::Zstd,
            append: false,
            tables: None,
            verbose: false,
            connection: None,
            format: DumpOutputFormat::Binary,
            sql: SqlDumpOptions::default(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct RestoreOptions {
    pub input_path: PathBuf,
    pub target: Option<PathBuf>,
    pub tables: Option<Vec<String>>,
    pub verify: bool,
    pub verbose: bool,
    pub connection: Option<String>,
    /// Fail the restore when the restored ROWS violate the constraints the
    /// dump carries (default `true`, `heliosdb-nano restore --no-validate`
    /// turns it off).
    ///
    /// A database may legitimately hold rows its own constraints reject: a
    /// `NOT ENFORCED` or `LOCK-FREE` constraint never checked them, and
    /// `SET helios.fk_validation = 'audit'` (docs/guides/fk_validation_modes.md)
    /// accepts orphans and logs them instead. Those modes are session state and
    /// cannot be set before a CLI restore, so without this switch a backup of
    /// such a database would not be restorable at all. With it off, every
    /// violation is reported as a warning (`RestoreReport::validation_warnings`)
    /// and the restore succeeds; the constraints are registered either way, so
    /// the restored database still rejects NEW violations.
    pub validate_constraints: bool,
}

impl Default for RestoreOptions {
    fn default() -> Self {
        Self {
            input_path: PathBuf::from("backup.heliodump"),
            target: None,
            tables: None,
            verify: true,
            verbose: false,
            connection: None,
            validate_constraints: true,
        }
    }
}

/// What a restore does with rows that violate the constraints it just
/// registered (phase C).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestoreValidation {
    /// Default: violations fail the restore, naming every offending table,
    /// constraint and key.
    Strict,
    /// `--no-validate`: violations become `RestoreReport::validation_warnings`
    /// lines and the restore succeeds.
    Lenient,
}

impl RestoreValidation {
    /// `Strict` unless the caller opted out.
    pub fn from_validate_flag(validate: bool) -> Self {
        if validate {
            Self::Strict
        } else {
            Self::Lenient
        }
    }

    /// True when a violation is a warning rather than a failure.
    pub fn is_lenient(self) -> bool {
        matches!(self, Self::Lenient)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DumpReport {
    pub dump_id: String,
    pub tables_dumped: usize,
    pub rows_dumped: u64,
    pub bytes_written: u64,
    pub bytes_uncompressed: u64,
    pub duration_ms: u64,
    pub compression_ratio: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RestoreReport {
    pub tables_restored: usize,
    pub rows_restored: u64,
    /// Constraint records (FK + CHECK + UNIQUE/PK) re-registered from the dump.
    /// Always 0 for a format-v1 dump, which carries none.
    pub constraints_restored: u64,
    /// Non-fatal notes from the constraint phases — a dangling FK parent that
    /// is in neither the dump nor the target, a CHECK this build could not
    /// evaluate. Empty when everything validated.
    pub validation_warnings: Vec<String>,
    pub duration_ms: u64,
}

/// What a restore actually did, for the CLI report.
///
/// Internal: `restore_from_dump` keeps its `Result<()>` signature, and
/// `restore_from_dump_stats` fills this in on the way through.
#[derive(Debug, Default)]
struct RestoreStats {
    tables: usize,
    rows: u64,
    constraints: u64,
    warnings: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Column, DataType, Value};
    use std::collections::HashMap;

    /// Mock database for testing
    struct MockDatabase {
        tables: HashMap<String, (Schema, Vec<Tuple>)>,
        indexes: HashMap<String, Vec<IndexMetadata>>,
    }

    impl MockDatabase {
        fn new() -> Self {
            Self {
                tables: HashMap::new(),
                indexes: HashMap::new(),
            }
        }

        fn add_table(&mut self, name: &str, schema: Schema, rows: Vec<Tuple>) {
            self.tables.insert(name.to_string(), (schema, rows));
        }

        fn add_index(&mut self, table: &str, index: IndexMetadata) {
            self.indexes
                .entry(table.to_string())
                .or_insert_with(Vec::new)
                .push(index);
        }
    }

    impl DatabaseInterface for MockDatabase {
        fn list_tables(&self) -> Result<Vec<String>> {
            Ok(self.tables.keys().cloned().collect())
        }

        fn get_table_schema(&self, table: &str) -> Result<Schema> {
            self.tables
                .get(table)
                .map(|(schema, _)| schema.clone())
                .ok_or_else(|| Error::storage(format!("Table {} not found", table)))
        }

        fn scan_table(&self, table: &str) -> Result<Vec<Tuple>> {
            self.tables
                .get(table)
                .map(|(_, rows)| rows.clone())
                .ok_or_else(|| Error::storage(format!("Table {} not found", table)))
        }

        fn get_table_indexes(&self, table: &str) -> Result<Vec<IndexMetadata>> {
            Ok(self.indexes.get(table).cloned().unwrap_or_default())
        }
    }

    impl DatabaseRestoreInterface for MockDatabase {
        fn create_table(&mut self, name: &str, schema: Schema) -> Result<()> {
            self.tables.insert(name.to_string(), (schema, Vec::new()));
            Ok(())
        }

        fn create_index(&mut self, table: &str, index: &IndexMetadata) -> Result<()> {
            self.add_index(table, index.clone());
            Ok(())
        }

        fn insert_row(&mut self, table: &str, row: Tuple) -> Result<()> {
            if let Some((_, rows)) = self.tables.get_mut(table) {
                rows.push(row);
                Ok(())
            } else {
                Err(Error::storage(format!("Table {} not found", table)))
            }
        }
    }

    #[test]
    fn test_dump_manager_creation() {
        let temp_dir = tempfile::tempdir().expect("Failed to create temp dir");
        let manager = DumpManager::new(temp_dir.path().to_path_buf(), CompressionType::Zstd);
        assert_eq!(manager.list_dumps().len(), 0);
    }

    #[test]
    fn test_full_dump_creation() -> Result<()> {
        let temp_dir = tempfile::tempdir().expect("Failed to create temp dir");
        let manager = DumpManager::new(temp_dir.path().to_path_buf(), CompressionType::Zstd);

        // Create mock database
        let mut db = MockDatabase::new();
        let schema = Schema::new(vec![
            Column::new("id", DataType::Int4),
            Column::new("name", DataType::Text),
        ]);

        let rows = vec![
            Tuple::new(vec![Value::Int4(1), Value::String("Alice".to_string())]),
            Tuple::new(vec![Value::Int4(2), Value::String("Bob".to_string())]),
        ];

        db.add_table("users", schema, rows);

        // Create dump
        let dump_path = temp_dir.path().join("test.dump");
        let metadata = manager.create_full_dump(&dump_path, &db)?;

        assert_eq!(metadata.dump_type, DumpType::Full);
        assert_eq!(metadata.table_count, 1);
        assert_eq!(metadata.total_rows, 2);
        assert!(dump_path.exists());
        assert!(metadata.compressed_size > 0);
        assert!(!metadata.checksum.is_empty());

        Ok(())
    }

    #[test]
    fn test_restore_from_dump() -> Result<()> {
        let temp_dir = tempfile::tempdir().expect("Failed to create temp dir");
        let manager = DumpManager::new(temp_dir.path().to_path_buf(), CompressionType::None);

        // Create and dump mock database
        let mut db = MockDatabase::new();
        let schema = Schema::new(vec![
            Column::new("id", DataType::Int4),
            Column::new("value", DataType::Float8),
        ]);

        let rows = vec![
            Tuple::new(vec![Value::Int4(1), Value::Float8(1.5)]),
            Tuple::new(vec![Value::Int4(2), Value::Float8(2.5)]),
            Tuple::new(vec![Value::Int4(3), Value::Float8(3.5)]),
        ];

        db.add_table("data", schema, rows);

        let dump_path = temp_dir.path().join("test_restore.dump");
        manager.create_full_dump(&dump_path, &db)?;

        // Restore to new database
        let mut db2 = MockDatabase::new();
        manager.restore_from_dump(&dump_path, &mut db2)?;

        // Verify restored data
        let restored_rows = db2.scan_table("data")?;
        assert_eq!(restored_rows.len(), 3);

        Ok(())
    }

    #[test]
    fn test_compression_roundtrip() -> Result<()> {
        let temp_dir = tempfile::tempdir().expect("Failed to create temp dir");
        let manager = DumpManager::new(temp_dir.path().to_path_buf(), CompressionType::Zstd);

        let test_data = b"Hello, World! This is test data for compression.".repeat(100);
        let compressed = manager.compress_data(&test_data)?;
        let decompressed = manager.decompress_data(&compressed)?;

        assert_eq!(test_data.to_vec(), decompressed);
        assert!(compressed.len() < test_data.len());

        Ok(())
    }

    #[test]
    fn test_dirty_tracker() {
        let tracker = DirtyTracker::new();

        assert!(!tracker.is_dirty());

        tracker.mark_table_dirty("users");
        assert!(tracker.is_dirty());

        let dirty_tables = tracker.get_dirty_tables();
        assert_eq!(dirty_tables.len(), 1);
        assert!(dirty_tables.contains(&"users".to_string()));

        tracker.clear_dirty();
        assert!(!tracker.is_dirty());
        assert_eq!(tracker.get_dirty_tables().len(), 0);
    }

    #[test]
    fn test_large_dataset_throughput() -> Result<()> {
        let temp_dir = tempfile::tempdir().expect("Failed to create temp dir");
        let manager = DumpManager::new(temp_dir.path().to_path_buf(), CompressionType::Zstd);

        // Create large dataset
        let mut db = MockDatabase::new();
        let schema = Schema::new(vec![
            Column::new("id", DataType::Int4),
            Column::new("data", DataType::Text),
        ]);

        // Generate 100K rows
        let mut rows = Vec::new();
        for i in 0..100_000 {
            rows.push(Tuple::new(vec![
                Value::Int4(i),
                Value::String(format!("Data row {} with some content", i)),
            ]));
        }

        db.add_table("large_table", schema, rows);

        // Measure dump time
        let start = Instant::now();
        let dump_path = temp_dir.path().join("large.dump");
        let metadata = manager.create_full_dump(&dump_path, &db)?;
        let elapsed = start.elapsed();

        // Calculate throughput
        let throughput_mbps = (metadata.uncompressed_size as f64 / 1_048_576.0) / elapsed.as_secs_f64();

        println!("Dumped {} rows in {:?}", metadata.total_rows, elapsed);
        println!("Throughput: {:.2} MB/s", throughput_mbps);

        // Should achieve >3 MB/s (conservative target for debug builds in VMs/containers)
        // Release builds should achieve >50 MB/s
        assert!(throughput_mbps > 3.0, "Throughput too low: {:.2} MB/s", throughput_mbps);

        Ok(())
    }

    #[test]
    fn test_validate_dump() -> Result<()> {
        let temp_dir = tempfile::tempdir().expect("Failed to create temp dir");
        let manager = DumpManager::new(temp_dir.path().to_path_buf(), CompressionType::None);

        let mut db = MockDatabase::new();
        let schema = Schema::new(vec![Column::new("id", DataType::Int4)]);
        db.add_table("test", schema, vec![]);

        let dump_path = temp_dir.path().join("validate.dump");
        manager.create_full_dump(&dump_path, &db)?;

        // Should validate successfully
        manager.validate_dump(&dump_path)?;

        // Test invalid file
        let invalid_path = temp_dir.path().join("invalid.dump");
        std::fs::write(&invalid_path, b"invalid data")?;

        assert!(manager.validate_dump(&invalid_path).is_err());

        Ok(())
    }

    #[test]
    fn test_incremental_dump() -> Result<()> {
        let temp_dir = tempfile::tempdir().expect("Failed to create temp dir");
        let manager = DumpManager::new(temp_dir.path().to_path_buf(), CompressionType::None);

        // Mark some tables as dirty
        manager.dirty_tracker().mark_table_dirty("users");

        let mut db = MockDatabase::new();
        let schema = Schema::new(vec![Column::new("id", DataType::Int4)]);
        db.add_table("users", schema, vec![Tuple::new(vec![Value::Int4(1)])]);

        let dump_path = temp_dir.path().join("incremental.dump");

        // Create incremental dump
        let metadata = manager.create_incremental_dump(&dump_path, &db, false)?;

        assert_eq!(metadata.dump_type, DumpType::Incremental);
        assert_eq!(metadata.table_count, 1);

        Ok(())
    }

    #[test]
    fn test_dump_with_indexes() -> Result<()> {
        let temp_dir = tempfile::tempdir().expect("Failed to create temp dir");
        let manager = DumpManager::new(temp_dir.path().to_path_buf(), CompressionType::None);

        let mut db = MockDatabase::new();
        let schema = Schema::new(vec![
            Column::new("id", DataType::Int4),
            Column::new("email", DataType::Text),
        ]);

        db.add_table("users", schema, vec![]);
        db.add_index(
            "users",
            IndexMetadata {
                name: "idx_email".to_string(),
                index_type: "btree".to_string(),
                columns: vec!["email".to_string()],
                is_unique: true,
            },
        );

        let dump_path = temp_dir.path().join("with_indexes.dump");
        manager.create_full_dump(&dump_path, &db)?;

        let mut db2 = MockDatabase::new();
        manager.restore_from_dump(&dump_path, &mut db2)?;

        let indexes = db2.get_table_indexes("users")?;
        assert_eq!(indexes.len(), 1);
        assert_eq!(indexes[0].name, "idx_email");

        Ok(())
    }

    #[test]
    fn test_get_next_dump_id() {
        let temp_dir = tempfile::tempdir().expect("Failed to create temp dir");
        let manager = DumpManager::new(temp_dir.path().to_path_buf(), CompressionType::None);

        let id1 = manager.get_next_dump_id();
        let id2 = manager.get_next_dump_id();
        let id3 = manager.get_next_dump_id();

        assert_eq!(id1, 1);
        assert_eq!(id2, 2);
        assert_eq!(id3, 3);
    }

    #[test]
    fn test_delete_old_dumps() -> Result<()> {
        let temp_dir = tempfile::tempdir().expect("Failed to create temp dir");
        let manager = DumpManager::new(temp_dir.path().to_path_buf(), CompressionType::None);

        // Add some mock dumps to history
        for i in 1..=5 {
            let mut metadata = DumpMetadata::new(i, DumpType::Full);
            metadata.created_at = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(i);
            manager.dump_history.write().push(metadata);
        }

        assert_eq!(manager.list_dumps().len(), 5);

        // Keep only 3 most recent
        manager.delete_old_dumps(3)?;

        assert_eq!(manager.list_dumps().len(), 3);

        Ok(())
    }

    #[test]
    fn test_checksum_calculation() -> Result<()> {
        let temp_dir = tempfile::tempdir().expect("Failed to create temp dir");
        let manager = DumpManager::new(temp_dir.path().to_path_buf(), CompressionType::None);

        let test_file = temp_dir.path().join("test.dat");
        std::fs::write(&test_file, b"test data for checksum")?;

        let checksum1 = manager.calculate_checksum(&test_file)?;
        let checksum2 = manager.calculate_checksum(&test_file)?;

        // Same file should produce same checksum
        assert_eq!(checksum1, checksum2);

        // Different content should produce different checksum
        std::fs::write(&test_file, b"different test data")?;
        let checksum3 = manager.calculate_checksum(&test_file)?;
        assert_ne!(checksum1, checksum3);

        Ok(())
    }
}
