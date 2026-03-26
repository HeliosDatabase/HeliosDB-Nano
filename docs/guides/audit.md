# Audit Module

Comprehensive audit logging system for HeliosDB Nano.

## Features

- **Tamper-proof logging**: Append-only with SHA-256 checksums
- **Async operation**: Non-blocking buffered logging
- **Configurable**: Fine-grained control over what gets logged
- **SQL queryable**: Query audit logs with standard SQL
- **Compliance-ready**: Supports SOC2, HIPAA, GDPR requirements

## Module Structure

```
audit/
├── mod.rs          - Module exports and initialization
├── events.rs       - Event types and operation classification
├── config.rs       - Configuration structures
├── logger.rs       - Main audit logger implementation
├── query.rs        - Query builder and filtering
└── README.md       - This file
```

## Quick Start

```rust
use heliosdb_nano::audit::{AuditLogger, AuditConfig};
use std::sync::Arc;

// Create storage and logger
let storage = Arc::new(storage_engine);
let config = AuditConfig::default();
let logger = AuditLogger::new(storage, config)?;

// Log operations
logger.log_ddl("CREATE TABLE", "users", "CREATE TABLE users (...)", true, None)?;
logger.log_dml("INSERT", "users", "INSERT INTO users ...", 1, true, None)?;
```

## Configuration Presets

- `AuditConfig::default()` - Standard configuration (DDL, DML, no SELECT)
- `AuditConfig::minimal()` - DDL only (lowest overhead)
- `AuditConfig::verbose()` - Everything including SELECT queries
- `AuditConfig::compliance()` - SOC2/HIPAA/GDPR ready (7-year retention)

## Architecture

### Event Flow

1. **Operation occurs** → Logger method called
2. **Event created** → AuditEvent struct with metadata
3. **Checksum calculated** → SHA-256 hash for tamper detection
4. **Buffered** → Sent to async channel
5. **Flushed** → Background task writes to storage

### Storage

Audit events are stored in the `__audit_log` system table:

- Column family: Same as regular tables
- Key format: `data:__audit_log:{event_id}`
- Value format: Serialized Tuple (bincode)

### Performance

- **Async logging**: Operations never block
- **Buffering**: Configurable buffer size (default: 100 events)
- **Selective logging**: Disable verbose operations (SELECT, transactions)
- **Query truncation**: Limit query text length to save space

## Integration Points

### With StorageEngine

```rust
// Initialize audit tables
audit::initialize_audit_tables(&storage)?;

// Logger has storage reference
let logger = AuditLogger::new(Arc::clone(&storage), config)?;
```

### With SQL Executor

```rust
// Before execution
audit_logger.log_operation(...)?;

// After execution
audit_logger.log_operation(/* with results */)?;
```

### With EmbeddedDatabase

```rust
// Optional: Wrap database with audit logging
let db_with_audit = AuditedDatabase::new(db, audit_logger);
```

## Security Considerations

1. **Protect audit table**: Restrict access to `__audit_log`
2. **Verify checksums**: Periodically check event integrity
3. **Secure storage**: Encrypt at rest if required
4. **Access control**: Audit access to audit logs themselves
5. **Retention policy**: Archive old logs, don't just delete

## Testing

Run audit tests:
```bash
cargo test --test audit_tests
```

Run example:
```bash
cargo run --example audit_demo
```

## Documentation

See `/home/claude/HeliosDB/heliosdb-nano/docs/AUDIT_LOGGING.md` for complete documentation.

## Future Enhancements

- [ ] Digital signatures (asymmetric crypto)
- [ ] Separate column family for audit logs
- [ ] Automatic retention/archival
- [ ] Real-time audit event streaming
- [ ] Distributed audit log (multi-node)
- [ ] Audit log compression
- [ ] Custom audit event types
- [ ] Webhook notifications for critical events
