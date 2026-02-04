//! Dump and restore functionality for HeliosDB-Lite
//!
//! This module provides mechanisms for exporting and importing database state
//! to/from portable dump files, supporting both full and incremental dumps.

mod manager;
mod format;

pub use manager::{
    DumpManager, DumpOptions, DumpMode, DumpOutputFormat, RestoreOptions, DumpReport, RestoreReport,
    DumpMetadata, DumpType, IndexMetadata, DirtyTracker,
    DatabaseInterface, DatabaseRestoreInterface,
};
pub use format::{
    DumpMetadata as FormatMetadata, DumpFormat, CompressionType, DUMP_MAGIC_NUMBER, DUMP_VERSION,
};
