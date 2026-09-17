//! Dump and restore functionality for HeliosDB-Lite
//!
//! This module provides mechanisms for exporting and importing database state
//! to/from portable dump files, supporting both full and incremental dumps.

mod format;
mod manager;
/// HDB-005: the one schema-aware SQL text serializer behind `dump_sql` and the
/// REPL's `\dump`. Everything else in this module writes the BINARY format.
pub(crate) mod sql_text;

pub use format::{CompressionType, DumpFormat, DumpMetadata as FormatMetadata, DUMP_MAGIC_NUMBER, DUMP_VERSION};
pub use manager::{
    DatabaseInterface, DatabaseRestoreInterface, DirtyTracker, DumpManager, DumpMetadata, DumpMode, DumpOptions,
    DumpOutputFormat, DumpReport, DumpType, IndexMetadata, RestoreOptions, RestoreReport, RestoreValidation,
};
