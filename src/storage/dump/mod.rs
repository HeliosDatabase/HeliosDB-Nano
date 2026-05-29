//! Dump and restore functionality for HeliosDB-Lite
//!
//! This module provides mechanisms for exporting and importing database state
//! to/from portable dump files, supporting both full and incremental dumps.

mod format;
mod manager;

pub use format::{CompressionType, DumpFormat, DumpMetadata as FormatMetadata, DUMP_MAGIC_NUMBER, DUMP_VERSION};
pub use manager::{
    DatabaseInterface, DatabaseRestoreInterface, DirtyTracker, DumpManager, DumpMetadata, DumpMode, DumpOptions,
    DumpOutputFormat, DumpReport, DumpType, IndexMetadata, RestoreOptions, RestoreReport,
};
