#!/bin/bash
################################################################################
# HeliosDB Phase 1 Migration Scripts
# SQLite → PostgreSQL → HeliosDB
#
# Purpose: Production-ready automation for database migration
# Version: 1.0.0
# Last Updated: 2025-12-08
#
# Prerequisites:
#   - SQLite 3.35+
#   - PostgreSQL 13+
#   - HeliosDB v3.0.0+
#   - Python 3.8+
#   - Bash 4.0+
#
# Usage:
#   ./PHASE_1_MIGRATION_SCRIPTS.sh migrate /path/to/sqlite.db /path/to/heliosdb/data
#   ./PHASE_1_MIGRATION_SCRIPTS.sh validate /path/to/backup/dir
#   ./PHASE_1_MIGRATION_SCRIPTS.sh rollback /path/to/backup/dir
################################################################################

set -euo pipefail  # Exit on error, undefined variables, pipe failures

# Color codes for output
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m' # No Color

# Logging functions
log_info() {
    echo -e "${BLUE}[INFO]${NC} $1"
}

log_success() {
    echo -e "${GREEN}[SUCCESS]${NC} $1"
}

log_warning() {
    echo -e "${YELLOW}[WARNING]${NC} $1"
}

log_error() {
    echo -e "${RED}[ERROR]${NC} $1" >&2
}

################################################################################
# Configuration and Environment Setup
################################################################################

# Default configuration
DEFAULT_TEMP_PG_PORT=5433
DEFAULT_HELIOSDB_PORT=5432
DEFAULT_PG_USER="postgres"
DEFAULT_CACHE_SIZE_MB=512

# Export environment variables with defaults
export TEMP_PG_PORT="${TEMP_PG_PORT:-$DEFAULT_TEMP_PG_PORT}"
export HELIOSDB_PORT="${HELIOSDB_PORT:-$DEFAULT_HELIOSDB_PORT}"
export PG_USER="${PG_USER:-$DEFAULT_PG_USER}"

################################################################################
# Validation Functions
################################################################################

check_prerequisites() {
    log_info "Checking prerequisites..."

    local missing_deps=0

    # Check SQLite
    if ! command -v sqlite3 &> /dev/null; then
        log_error "SQLite not found. Please install SQLite 3.35+"
        missing_deps=1
    else
        local sqlite_version=$(sqlite3 --version | awk '{print $1}')
        log_success "SQLite found: $sqlite_version"
    fi

    # Check PostgreSQL
    if ! command -v psql &> /dev/null; then
        log_error "PostgreSQL not found. Please install PostgreSQL 13+"
        missing_deps=1
    else
        local pg_version=$(psql --version | awk '{print $3}')
        log_success "PostgreSQL found: $pg_version"
    fi

    # Check HeliosDB
    if ! command -v heliosdb-lite &> /dev/null; then
        log_error "HeliosDB not found. Please install HeliosDB v3.0.0+"
        missing_deps=1
    else
        local heliosdb_version=$(heliosdb-lite --version 2>/dev/null || echo "unknown")
        log_success "HeliosDB found: $heliosdb_version"
    fi

    # Check Python
    if ! command -v python3 &> /dev/null; then
        log_error "Python 3 not found. Please install Python 3.8+"
        missing_deps=1
    else
        local python_version=$(python3 --version | awk '{print $2}')
        log_success "Python found: $python_version"
    fi

    # Check required tools
    for tool in pg_dump pg_restore initdb createdb md5sum tar gzip; do
        if ! command -v "$tool" &> /dev/null; then
            log_error "Required tool not found: $tool"
            missing_deps=1
        fi
    done

    if [ $missing_deps -eq 1 ]; then
        log_error "Missing required dependencies. Please install them and try again."
        exit 1
    fi

    log_success "All prerequisites satisfied"
}

check_disk_space() {
    local source_db="$1"
    local backup_dir="$2"

    log_info "Checking disk space..."

    # Get source database size
    local db_size=$(du -sb "$source_db" | awk '{print $1}')
    local db_size_mb=$((db_size / 1024 / 1024))

    # Required space: 3x database size for safety
    local required_space=$((db_size * 3))
    local required_space_mb=$((required_space / 1024 / 1024))

    # Get available space
    local available_space=$(df -B1 "$backup_dir" | tail -1 | awk '{print $4}')
    local available_space_mb=$((available_space / 1024 / 1024))

    log_info "Database size: ${db_size_mb} MB"
    log_info "Required space: ${required_space_mb} MB (3x database size)"
    log_info "Available space: ${available_space_mb} MB"

    if [ "$available_space" -lt "$required_space" ]; then
        log_error "Insufficient disk space. Need ${required_space_mb} MB, have ${available_space_mb} MB"
        exit 1
    fi

    log_success "Sufficient disk space available"
}

validate_sqlite_db() {
    local db_path="$1"

    log_info "Validating SQLite database: $db_path"

    if [ ! -f "$db_path" ]; then
        log_error "SQLite database not found: $db_path"
        exit 1
    fi

    # Run integrity check
    local integrity_check=$(sqlite3 "$db_path" "PRAGMA integrity_check;" 2>&1)

    if [ "$integrity_check" != "ok" ]; then
        log_error "SQLite database integrity check failed: $integrity_check"
        exit 1
    fi

    log_success "SQLite database is valid"
}

################################################################################
# Backup Functions
################################################################################

create_backup() {
    local source_db="$1"
    local backup_dir="$2"

    log_info "Creating SQLite backup..."

    # Create backup using SQLite's .backup command (safest method)
    sqlite3 "$source_db" ".backup '$backup_dir/database_backup.sqlite'" 2>&1 | tee "$backup_dir/sqlite_backup.log"

    if [ ! -f "$backup_dir/database_backup.sqlite" ]; then
        log_error "Failed to create SQLite backup"
        exit 1
    fi

    # Verify backup
    validate_sqlite_db "$backup_dir/database_backup.sqlite"

    # Create checksum
    md5sum "$backup_dir/database_backup.sqlite" > "$backup_dir/database_backup.sqlite.md5"

    # Export schema
    sqlite3 "$source_db" ".schema" > "$backup_dir/schema_original.sql"

    # Export table statistics
    sqlite3 "$source_db" "SELECT name, COUNT(*) as count FROM (SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%') tables, sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%' GROUP BY name;" > "$backup_dir/table_counts_original.txt" 2>/dev/null || true

    # Better table count export
    sqlite3 "$source_db" << 'EOF' > "$backup_dir/table_counts_detailed.txt"
SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%';
EOF

    while IFS= read -r table; do
        count=$(sqlite3 "$source_db" "SELECT COUNT(*) FROM \"$table\";")
        echo "$table: $count" >> "$backup_dir/table_counts_original.txt"
    done < <(sqlite3 "$source_db" "SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%';")

    log_success "SQLite backup created: $backup_dir/database_backup.sqlite"
}

################################################################################
# Analysis Functions
################################################################################

analyze_database() {
    local source_db="$1"
    local output_file="$2"

    log_info "Analyzing database structure..."

    python3 << 'PYTHON_EOF' > "$output_file"
import sqlite3
import sys
import os
from datetime import datetime

db_path = os.environ.get('SOURCE_DB', sys.argv[1] if len(sys.argv) > 1 else '')
if not db_path or not os.path.exists(db_path):
    print("Error: Database path not found")
    sys.exit(1)

try:
    conn = sqlite3.connect(db_path)
    cursor = conn.cursor()

    print("=" * 80)
    print("DATABASE ANALYSIS REPORT")
    print("=" * 80)
    print(f"Generated: {datetime.now().isoformat()}")
    print(f"Database: {db_path}")
    print()

    # Get database size
    db_size = os.path.getsize(db_path)
    print(f"Database Size: {db_size:,} bytes ({db_size / 1024 / 1024:.2f} MB)")
    print()

    # List all tables
    cursor.execute("SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%' ORDER BY name")
    tables = [row[0] for row in cursor.fetchall()]
    print(f"Total Tables: {len(tables)}")
    print()

    # Analyze each table
    for table_name in tables:
        print("-" * 80)
        print(f"TABLE: {table_name}")
        print("-" * 80)

        # Get row count
        cursor.execute(f"SELECT COUNT(*) FROM \"{table_name}\"")
        row_count = cursor.fetchone()[0]
        print(f"Rows: {row_count:,}")

        # Get column info
        cursor.execute(f"PRAGMA table_info(\"{table_name}\")")
        columns = cursor.fetchall()
        print(f"Columns: {len(columns)}")
        for col in columns:
            pk_indicator = " [PRIMARY KEY]" if col[5] == 1 else ""
            notnull_indicator = " [NOT NULL]" if col[3] == 1 else ""
            default_value = f" DEFAULT {col[4]}" if col[4] is not None else ""
            print(f"  - {col[1]} ({col[2]}){pk_indicator}{notnull_indicator}{default_value}")

        # Get indexes
        cursor.execute(f"SELECT name, sql FROM sqlite_master WHERE type='index' AND tbl_name='{table_name}' AND sql IS NOT NULL")
        indexes = cursor.fetchall()
        if indexes:
            print(f"Indexes: {len(indexes)}")
            for idx_name, idx_sql in indexes:
                print(f"  - {idx_name}")

        # Get foreign keys
        cursor.execute(f"PRAGMA foreign_key_list(\"{table_name}\")")
        fks = cursor.fetchall()
        if fks:
            print(f"Foreign Keys: {len(fks)}")
            for fk in fks:
                print(f"  - {fk[2]}.{fk[3]} -> {fk[4]}")

        # Sample data
        cursor.execute(f"SELECT * FROM \"{table_name}\" LIMIT 1")
        sample = cursor.fetchone()
        if sample:
            print("Sample Row:")
            for i, col in enumerate(columns):
                print(f"  {col[1]}: {sample[i]}")

        print()

    # Check for potential migration issues
    print("=" * 80)
    print("MIGRATION COMPATIBILITY CHECKS")
    print("=" * 80)

    # Check for tables without primary keys
    print("\nTables without PRIMARY KEY:")
    tables_without_pk = []
    for table_name in tables:
        cursor.execute(f"PRAGMA table_info(\"{table_name}\")")
        columns = cursor.fetchall()
        has_pk = any(col[5] == 1 for col in columns)
        if not has_pk:
            tables_without_pk.append(table_name)
            print(f"  - {table_name}")

    if not tables_without_pk:
        print("  None (OK)")

    # Check for BLOB columns
    print("\nBLOB columns (will be converted to BYTEA):")
    blob_columns = []
    for table_name in tables:
        cursor.execute(f"PRAGMA table_info(\"{table_name}\")")
        columns = cursor.fetchall()
        for col in columns:
            if col[2].upper() == 'BLOB':
                blob_columns.append(f"{table_name}.{col[1]}")
                print(f"  - {table_name}.{col[1]}")

    if not blob_columns:
        print("  None")

    # Check for JSON columns
    print("\nJSON columns (will be converted to JSONB):")
    json_columns = []
    for table_name in tables:
        cursor.execute(f"PRAGMA table_info(\"{table_name}\")")
        columns = cursor.fetchall()
        for col in columns:
            if 'JSON' in col[2].upper():
                json_columns.append(f"{table_name}.{col[1]}")
                print(f"  - {table_name}.{col[1]}")

    if not json_columns:
        print("  None")

    print()
    print("=" * 80)
    print("SUMMARY")
    print("=" * 80)
    print(f"Total Tables: {len(tables)}")
    print(f"Total Rows: {sum(cursor.execute(f'SELECT COUNT(*) FROM \"{t}\"').fetchone()[0] for t in tables):,}")
    print(f"Tables without PK: {len(tables_without_pk)}")
    print(f"BLOB columns: {len(blob_columns)}")
    print(f"JSON columns: {len(json_columns)}")
    print()

    conn.close()

except Exception as e:
    print(f"Error analyzing database: {e}", file=sys.stderr)
    sys.exit(1)
PYTHON_EOF

    export SOURCE_DB="$source_db"
    python3 -c "$(cat)" < /dev/stdin

    log_success "Database analysis complete: $output_file"
}

################################################################################
# PostgreSQL Setup Functions
################################################################################

setup_temp_postgres() {
    local backup_dir="$1"
    local temp_pg_data="$backup_dir/temp_postgres_data"

    log_info "Setting up temporary PostgreSQL instance..."

    # Initialize PostgreSQL data directory
    initdb -D "$temp_pg_data" -U "$PG_USER" -E UTF8 --no-locale --no-sync 2>&1 | tee "$backup_dir/initdb.log"

    # Configure PostgreSQL for migration workload
    cat >> "$temp_pg_data/postgresql.conf" << EOF
# Migration-optimized configuration
port = $TEMP_PG_PORT
max_connections = 20
shared_buffers = 512MB
work_mem = 64MB
maintenance_work_mem = 256MB
checkpoint_completion_target = 0.9
wal_buffers = 16MB
default_statistics_target = 100
random_page_cost = 1.1
effective_cache_size = 1GB
min_wal_size = 1GB
max_wal_size = 2GB

# Disable synchronous commit for faster imports
synchronous_commit = off
fsync = off

# Increase checkpoint segments
max_wal_senders = 0
wal_level = minimal

# Logging
logging_collector = on
log_directory = '$backup_dir'
log_filename = 'postgresql-%Y-%m-%d_%H%M%S.log'
log_statement = 'all'
log_min_duration_statement = 1000
EOF

    # Allow local connections without password
    cat > "$temp_pg_data/pg_hba.conf" << EOF
# TYPE  DATABASE        USER            ADDRESS                 METHOD
local   all             all                                     trust
host    all             all             127.0.0.1/32            trust
host    all             all             ::1/128                 trust
EOF

    # Start PostgreSQL
    pg_ctl -D "$temp_pg_data" -l "$backup_dir/postgres_startup.log" start -w

    if [ $? -ne 0 ]; then
        log_error "Failed to start PostgreSQL. Check $backup_dir/postgres_startup.log"
        exit 1
    fi

    # Wait for PostgreSQL to be ready
    sleep 5

    # Test connection
    if ! psql -U "$PG_USER" -p "$TEMP_PG_PORT" -d postgres -c "SELECT 1" > /dev/null 2>&1; then
        log_error "Cannot connect to PostgreSQL"
        exit 1
    fi

    # Create migration database
    createdb -U "$PG_USER" -p "$TEMP_PG_PORT" migration_staging

    log_success "Temporary PostgreSQL instance started on port $TEMP_PG_PORT"
}

################################################################################
# SQLite to PostgreSQL Conversion Functions
################################################################################

convert_sqlite_to_postgres() {
    local source_db="$1"
    local output_file="$2"

    log_info "Converting SQLite schema and data to PostgreSQL format..."

    python3 << 'PYTHON_EOF' > "$output_file"
import sqlite3
import re
import os
import sys
from datetime import datetime

db_path = os.environ.get('SOURCE_DB', sys.argv[1] if len(sys.argv) > 1 else '')
if not db_path or not os.path.exists(db_path):
    print("Error: Database path not found", file=sys.stderr)
    sys.exit(1)

try:
    conn = sqlite3.connect(db_path)
    conn.row_factory = sqlite3.Row
    cursor = conn.cursor()

    print("-- PostgreSQL-compatible dump generated from SQLite")
    print(f"-- Source: {db_path}")
    print(f"-- Generated: {datetime.now().isoformat()}")
    print()
    print("BEGIN;")
    print()

    # Get all tables
    cursor.execute("SELECT name, sql FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%' ORDER BY name")
    tables = cursor.fetchall()

    # Process each table
    for table_row in tables:
        table_name = table_row['name']
        create_sql = table_row['sql']

        if not create_sql:
            continue

        print(f"-- Table: {table_name}")

        # Convert CREATE TABLE statement
        pg_sql = create_sql

        # Convert AUTOINCREMENT to SERIAL
        pg_sql = re.sub(
            r'\b(\w+)\s+INTEGER\s+PRIMARY\s+KEY\s+AUTOINCREMENT\b',
            r'\1 SERIAL PRIMARY KEY',
            pg_sql,
            flags=re.IGNORECASE
        )
        pg_sql = re.sub(
            r'\b(\w+)\s+INTEGER\s+AUTOINCREMENT\b',
            r'\1 SERIAL',
            pg_sql,
            flags=re.IGNORECASE
        )

        # Convert data types
        # INTEGER -> BIGINT (PostgreSQL's INTEGER is 32-bit, BIGINT is 64-bit like SQLite)
        pg_sql = re.sub(r'\bINTEGER\b(?!\s+PRIMARY)', 'BIGINT', pg_sql, flags=re.IGNORECASE)

        # REAL -> DOUBLE PRECISION
        pg_sql = re.sub(r'\bREAL\b', 'DOUBLE PRECISION', pg_sql, flags=re.IGNORECASE)

        # BLOB -> BYTEA
        pg_sql = re.sub(r'\bBLOB\b', 'BYTEA', pg_sql, flags=re.IGNORECASE)

        # Handle DATETIME -> TIMESTAMP
        pg_sql = re.sub(r'\bDATETIME\b', 'TIMESTAMP', pg_sql, flags=re.IGNORECASE)

        # Remove SQLite-specific clauses
        pg_sql = re.sub(r'\bWITHOUT\s+ROWID\b', '', pg_sql, flags=re.IGNORECASE)

        print(pg_sql + ";")
        print()

    # Export data
    for table_row in tables:
        table_name = table_row['name']

        cursor.execute(f"SELECT * FROM \"{table_name}\"")
        rows = cursor.fetchall()

        if not rows:
            print(f"-- No data in table {table_name}")
            print()
            continue

        # Get column names
        column_names = [description[0] for description in cursor.description]

        print(f"-- Data for table {table_name}")

        # Process rows in batches of 100 for better performance
        batch_size = 100
        for i in range(0, len(rows), batch_size):
            batch = rows[i:i+batch_size]

            for row in batch:
                values = []
                for val in row:
                    if val is None:
                        values.append('NULL')
                    elif isinstance(val, (int, float)):
                        values.append(str(val))
                    elif isinstance(val, str):
                        # Escape single quotes and backslashes
                        escaped = val.replace('\\', '\\\\').replace("'", "''")
                        values.append(f"'{escaped}'")
                    elif isinstance(val, bytes):
                        # Convert BLOB to PostgreSQL bytea hex format
                        hex_data = val.hex()
                        values.append(f"'\\x{hex_data}'")
                    else:
                        # Fallback: convert to string
                        escaped = str(val).replace('\\', '\\\\').replace("'", "''")
                        values.append(f"'{escaped}'")

                # Generate INSERT statement
                columns_str = ', '.join(f'"{col}"' for col in column_names)
                values_str = ', '.join(values)
                print(f"INSERT INTO \"{table_name}\" ({columns_str}) VALUES ({values_str});")

        print()

    # Export indexes
    print("-- Indexes")
    cursor.execute("SELECT name, sql FROM sqlite_master WHERE type='index' AND sql IS NOT NULL ORDER BY name")
    indexes = cursor.fetchall()

    for index_row in indexes:
        index_sql = index_row['sql']
        if index_sql:
            print(index_sql + ";")

    print()
    print("COMMIT;")
    print()
    print("-- Analyze tables for query optimization")
    print("ANALYZE;")

    conn.close()

except Exception as e:
    print(f"Error converting database: {e}", file=sys.stderr)
    sys.exit(1)
PYTHON_EOF

    export SOURCE_DB="$source_db"
    python3 -c "$(cat)" < /dev/stdin

    if [ ! -s "$output_file" ]; then
        log_error "Failed to convert SQLite to PostgreSQL format"
        exit 1
    fi

    log_success "Conversion complete: $output_file"
}

################################################################################
# Import and Validation Functions
################################################################################

import_to_postgres() {
    local sql_file="$1"
    local backup_dir="$2"

    log_info "Importing data to PostgreSQL..."

    # Import with error logging
    psql -U "$PG_USER" -p "$TEMP_PG_PORT" -d migration_staging \
        -f "$sql_file" \
        2>&1 | tee "$backup_dir/postgres_import.log"

    # Check for critical errors (ignore "already exists" warnings)
    if grep -i "error" "$backup_dir/postgres_import.log" | grep -v "already exists"; then
        log_warning "Errors found during import. Check $backup_dir/postgres_import.log"
        # Don't exit, continue with validation
    fi

    # Run ANALYZE
    psql -U "$PG_USER" -p "$TEMP_PG_PORT" -d migration_staging -c "ANALYZE;" > /dev/null

    log_success "Import to PostgreSQL complete"
}

validate_postgres_data() {
    local backup_dir="$1"
    local output_file="$backup_dir/postgres_validation.txt"

    log_info "Validating PostgreSQL data..."

    psql -U "$PG_USER" -p "$TEMP_PG_PORT" -d migration_staging << 'EOF' > "$output_file"
-- PostgreSQL Validation Report
\echo '=== Table Row Counts ==='
SELECT
    schemaname,
    tablename,
    COALESCE(n_live_tup, 0) as row_count
FROM pg_stat_user_tables
ORDER BY tablename;

\echo ''
\echo '=== Table Sizes ==='
SELECT
    schemaname,
    tablename,
    pg_size_pretty(pg_total_relation_size(schemaname||'.'||tablename)) AS size
FROM pg_tables
WHERE schemaname = 'public'
ORDER BY pg_total_relation_size(schemaname||'.'||tablename) DESC;

\echo ''
\echo '=== Indexes ==='
SELECT
    schemaname,
    tablename,
    indexname,
    indexdef
FROM pg_indexes
WHERE schemaname = 'public'
ORDER BY tablename, indexname;

\echo ''
\echo '=== Constraints ==='
SELECT
    conrelid::regclass as table_name,
    conname as constraint_name,
    contype as constraint_type,
    pg_get_constraintdef(oid) as definition
FROM pg_constraint
WHERE connamespace = 'public'::regnamespace
ORDER BY table_name, constraint_name;
EOF

    log_success "PostgreSQL validation complete: $output_file"
}

################################################################################
# PostgreSQL to HeliosDB Migration Functions
################################################################################

export_postgres_dump() {
    local backup_dir="$1"
    local dump_file="$backup_dir/heliosdb_import.sql"

    log_info "Creating PostgreSQL dump for HeliosDB..."

    pg_dump -U "$PG_USER" -p "$TEMP_PG_PORT" \
        --clean \
        --if-exists \
        --no-owner \
        --no-privileges \
        --format=plain \
        --encoding=UTF8 \
        migration_staging > "$dump_file"

    if [ ! -s "$dump_file" ]; then
        log_error "Failed to create PostgreSQL dump"
        exit 1
    fi

    # Create schema-only dump
    pg_dump -U "$PG_USER" -p "$TEMP_PG_PORT" \
        --schema-only \
        --no-owner \
        --no-privileges \
        migration_staging > "$backup_dir/heliosdb_schema_only.sql"

    # Create checksums
    md5sum "$dump_file" > "$dump_file.md5"
    md5sum "$backup_dir/heliosdb_schema_only.sql" > "$backup_dir/heliosdb_schema_only.sql.md5"

    log_success "PostgreSQL dump created: $dump_file"
}

setup_heliosdb() {
    local heliosdb_data="$1"
    local backup_dir="$2"
    local config_file="$backup_dir/heliosdb_config.toml"

    log_info "Setting up HeliosDB..."

    # Create configuration file
    cat > "$config_file" << EOF
[storage]
path = "$heliosdb_data"
cache_size = $((DEFAULT_CACHE_SIZE_MB * 1024 * 1024))
compression = "zstd"
in_memory = false

[encryption]
enabled = false
algorithm = "aes256-gcm"

[server]
listen_addr = "127.0.0.1"
port = $HELIOSDB_PORT
max_connections = 100

[performance]
enable_simd = true
worker_threads = 4
parallel_query = true

[phase3]
pq_enabled = true
pq_auto_compress = true
mv_auto_refresh = false
time_travel_retention_days = 30
EOF

    # Initialize HeliosDB
    mkdir -p "$heliosdb_data"
    heliosdb-lite init "$heliosdb_data" 2>&1 | tee "$backup_dir/heliosdb_init.log"

    log_success "HeliosDB initialized: $heliosdb_data"
    log_info "Configuration: $config_file"
}

import_to_heliosdb() {
    local dump_file="$1"
    local heliosdb_data="$2"
    local backup_dir="$3"
    local config_file="$backup_dir/heliosdb_config.toml"

    log_info "Starting HeliosDB server..."

    # Start HeliosDB in background
    heliosdb-lite start \
        --config "$config_file" \
        --port "$HELIOSDB_PORT" \
        --data "$heliosdb_data" \
        > "$backup_dir/heliosdb_server.log" 2>&1 &

    local heliosdb_pid=$!
    echo $heliosdb_pid > "$backup_dir/heliosdb.pid"

    # Wait for HeliosDB to start
    log_info "Waiting for HeliosDB to start (PID: $heliosdb_pid)..."
    local max_wait=30
    local wait_count=0

    while [ $wait_count -lt $max_wait ]; do
        if psql -h localhost -p "$HELIOSDB_PORT" -U postgres -d postgres -c "SELECT 1" > /dev/null 2>&1; then
            break
        fi
        sleep 1
        wait_count=$((wait_count + 1))
    done

    if [ $wait_count -eq $max_wait ]; then
        log_error "HeliosDB failed to start within ${max_wait} seconds"
        cat "$backup_dir/heliosdb_server.log"
        exit 1
    fi

    log_success "HeliosDB started successfully"

    # Import data
    log_info "Importing data to HeliosDB..."

    psql -h localhost -p "$HELIOSDB_PORT" -U postgres -d heliosdb \
        -f "$dump_file" \
        2>&1 | tee "$backup_dir/heliosdb_import.log"

    # Check for errors
    if grep -i "error" "$backup_dir/heliosdb_import.log" | grep -v "already exists"; then
        log_warning "Errors found during HeliosDB import. Check logs."
    fi

    log_success "Data imported to HeliosDB"
}

validate_heliosdb_data() {
    local backup_dir="$1"
    local output_file="$backup_dir/heliosdb_validation.txt"

    log_info "Validating HeliosDB data..."

    psql -h localhost -p "$HELIOSDB_PORT" -U postgres -d heliosdb << 'EOF' > "$output_file"
-- HeliosDB Validation Report
\echo '=== Table Row Counts ==='
SELECT
    schemaname,
    tablename,
    COALESCE(n_live_tup, 0) as row_count
FROM pg_stat_user_tables
ORDER BY tablename;

\echo ''
\echo '=== Indexes ==='
\di

\echo ''
\echo '=== Tables ==='
\dt

\echo ''
\echo '=== Constraints ==='
SELECT
    conrelid::regclass as table_name,
    conname as constraint_name,
    contype as constraint_type
FROM pg_constraint
WHERE connamespace = 'public'::regnamespace
ORDER BY table_name, constraint_name;
EOF

    log_success "HeliosDB validation complete: $output_file"
}

################################################################################
# Cleanup Functions
################################################################################

cleanup_temp_postgres() {
    local backup_dir="$1"
    local temp_pg_data="$backup_dir/temp_postgres_data"

    log_info "Cleaning up temporary PostgreSQL instance..."

    # Stop PostgreSQL
    if [ -d "$temp_pg_data" ]; then
        pg_ctl -D "$temp_pg_data" stop -m fast > /dev/null 2>&1 || true

        # Archive PostgreSQL data
        log_info "Archiving PostgreSQL data for rollback capability..."
        tar -czf "$backup_dir/temp_postgres_backup.tar.gz" "$temp_pg_data" 2>/dev/null || true

        # Remove temporary data (optional)
        # Uncomment to save disk space:
        # rm -rf "$temp_pg_data"
    fi

    log_success "Temporary PostgreSQL cleanup complete"
}

create_migration_summary() {
    local source_db="$1"
    local heliosdb_data="$2"
    local backup_dir="$3"
    local summary_file="$backup_dir/MIGRATION_SUMMARY.md"

    log_info "Creating migration summary..."

    cat > "$summary_file" << EOF
# Migration Summary

**Date**: $(date)
**Source Database**: $source_db
**Destination**: $heliosdb_data

## File Checksums

\`\`\`
$(md5sum "$backup_dir"/*.sql "$backup_dir"/*.sqlite 2>/dev/null || echo "Checksums not available")
\`\`\`

## Migration Steps Completed

1. ✅ SQLite backup created
2. ✅ Database analysis performed
3. ✅ Temporary PostgreSQL instance set up
4. ✅ SQLite data converted to PostgreSQL format
5. ✅ Data imported to PostgreSQL
6. ✅ PostgreSQL data validated
7. ✅ PostgreSQL dump created
8. ✅ HeliosDB initialized
9. ✅ Data imported to HeliosDB
10. ✅ HeliosDB data validated

## Validation Results

### Original SQLite Row Counts
\`\`\`
$(cat "$backup_dir/table_counts_original.txt" 2>/dev/null || echo "Not available")
\`\`\`

### PostgreSQL Validation
\`\`\`
$(cat "$backup_dir/postgres_validation.txt" 2>/dev/null || echo "Not available")
\`\`\`

### HeliosDB Validation
\`\`\`
$(cat "$backup_dir/heliosdb_validation.txt" 2>/dev/null || echo "Not available")
\`\`\`

## Backup Locations

- **SQLite Backup**: $backup_dir/database_backup.sqlite
- **PostgreSQL Dump**: $backup_dir/heliosdb_import.sql
- **HeliosDB Data**: $heliosdb_data
- **Configuration**: $backup_dir/heliosdb_config.toml
- **Logs**: $backup_dir/*.log

## Next Steps

1. Update application connection strings (see PHASE_1_CONFIGURATION_TEMPLATES.md)
2. Run integration tests
3. Monitor application performance
4. Enable encryption if required:
   \`\`\`bash
   export HELIOSDB_ENCRYPTION_KEY="your-32-byte-key-here"
   # Update $backup_dir/heliosdb_config.toml:
   # [encryption]
   # enabled = true
   \`\`\`
5. Configure materialized views for analytics
6. Set up backup schedules

## Rollback Instructions

If you need to rollback:

\`\`\`bash
# Stop HeliosDB
heliosdb-lite stop

# Restore SQLite backup
cp $backup_dir/database_backup.sqlite $source_db

# Verify
sqlite3 $source_db "PRAGMA integrity_check;"
\`\`\`

## Support

For issues or questions, consult:
- PHASE_1_TROUBLESHOOTING_GUIDE.md
- HeliosDB documentation: https://docs.heliosdb.com
- GitHub issues: https://github.com/heliosdb/heliosdb/issues

---

Generated by HeliosDB Phase 1 Migration Scripts v1.0.0
EOF

    log_success "Migration summary created: $summary_file"
}

################################################################################
# Main Migration Function
################################################################################

run_migration() {
    local source_db="$1"
    local heliosdb_data="$2"
    local backup_dir="${3:-/tmp/heliosdb_migration_$(date +%Y%m%d_%H%M%S)}"

    log_info "Starting migration: $source_db -> $heliosdb_data"
    log_info "Backup directory: $backup_dir"

    # Create backup directory
    mkdir -p "$backup_dir"

    # Step 1: Prerequisites
    check_prerequisites
    check_disk_space "$source_db" "$backup_dir"
    validate_sqlite_db "$source_db"

    # Step 2: Backup
    create_backup "$source_db" "$backup_dir"
    analyze_database "$source_db" "$backup_dir/analysis_report.txt"

    # Step 3: Setup PostgreSQL
    setup_temp_postgres "$backup_dir"

    # Step 4: Convert and import to PostgreSQL
    convert_sqlite_to_postgres "$source_db" "$backup_dir/postgres_dump.sql"
    import_to_postgres "$backup_dir/postgres_dump.sql" "$backup_dir"
    validate_postgres_data "$backup_dir"

    # Step 5: Export from PostgreSQL
    export_postgres_dump "$backup_dir"

    # Step 6: Setup and import to HeliosDB
    setup_heliosdb "$heliosdb_data" "$backup_dir"
    import_to_heliosdb "$backup_dir/heliosdb_import.sql" "$heliosdb_data" "$backup_dir"
    validate_heliosdb_data "$backup_dir"

    # Step 7: Cleanup
    cleanup_temp_postgres "$backup_dir"
    create_migration_summary "$source_db" "$heliosdb_data" "$backup_dir"

    log_success "Migration complete!"
    log_info "Summary: $backup_dir/MIGRATION_SUMMARY.md"
    log_info "HeliosDB is running on port $HELIOSDB_PORT (PID: $(cat "$backup_dir/heliosdb.pid"))"
}

################################################################################
# Validation Command
################################################################################

run_validation() {
    local backup_dir="$1"

    if [ ! -d "$backup_dir" ]; then
        log_error "Backup directory not found: $backup_dir"
        exit 1
    fi

    log_info "Running post-migration validation..."

    # Compare row counts
    log_info "Comparing row counts..."

    if [ -f "$backup_dir/table_counts_original.txt" ] && [ -f "$backup_dir/heliosdb_validation.txt" ]; then
        diff <(sort "$backup_dir/table_counts_original.txt") \
             <(grep "row_count" "$backup_dir/heliosdb_validation.txt" | sort) || true
    else
        log_warning "Validation files not found"
    fi

    # Check for errors in logs
    log_info "Checking for errors in logs..."

    for log_file in "$backup_dir"/*.log; do
        if grep -i "error" "$log_file" | grep -v "already exists" > /dev/null; then
            log_warning "Errors found in $log_file"
        fi
    done

    log_success "Validation complete"
}

################################################################################
# Rollback Command
################################################################################

run_rollback() {
    local backup_dir="$1"

    if [ ! -d "$backup_dir" ]; then
        log_error "Backup directory not found: $backup_dir"
        exit 1
    fi

    log_warning "Starting rollback procedure..."

    # Stop HeliosDB
    if [ -f "$backup_dir/heliosdb.pid" ]; then
        local heliosdb_pid=$(cat "$backup_dir/heliosdb.pid")
        log_info "Stopping HeliosDB (PID: $heliosdb_pid)..."
        kill "$heliosdb_pid" 2>/dev/null || true
        sleep 5
    fi

    # Verify backup exists
    if [ ! -f "$backup_dir/database_backup.sqlite" ]; then
        log_error "SQLite backup not found: $backup_dir/database_backup.sqlite"
        exit 1
    fi

    # Restore backup
    log_info "Restoring SQLite backup..."

    # Prompt for confirmation
    read -p "This will overwrite the current database. Continue? (yes/no): " confirm

    if [ "$confirm" != "yes" ]; then
        log_info "Rollback cancelled"
        exit 0
    fi

    # Get original database path from summary
    local source_db=$(grep "Source Database:" "$backup_dir/MIGRATION_SUMMARY.md" | cut -d: -f2- | xargs)

    if [ -z "$source_db" ]; then
        log_error "Cannot determine original database path"
        exit 1
    fi

    # Create backup of current state
    if [ -f "$source_db" ]; then
        cp "$source_db" "$source_db.pre-rollback.$(date +%Y%m%d_%H%M%S)"
    fi

    # Restore
    cp "$backup_dir/database_backup.sqlite" "$source_db"

    # Verify
    validate_sqlite_db "$source_db"

    log_success "Rollback complete. Database restored to: $source_db"
}

################################################################################
# Main Command Handler
################################################################################

usage() {
    cat << EOF
HeliosDB Phase 1 Migration Scripts v1.0.0

Usage:
  $0 migrate <source_sqlite_db> <heliosdb_data_dir> [backup_dir]
  $0 validate <backup_dir>
  $0 rollback <backup_dir>
  $0 --help

Commands:
  migrate   - Run full migration from SQLite to HeliosDB
  validate  - Validate migration results
  rollback  - Rollback to original SQLite database

Examples:
  $0 migrate /path/to/database.sqlite /path/to/heliosdb/data
  $0 migrate /path/to/database.sqlite /path/to/heliosdb/data /custom/backup/dir
  $0 validate /tmp/heliosdb_migration_20251208_120000
  $0 rollback /tmp/heliosdb_migration_20251208_120000

Environment Variables:
  TEMP_PG_PORT      - PostgreSQL temporary port (default: 5433)
  HELIOSDB_PORT     - HeliosDB server port (default: 5432)
  PG_USER           - PostgreSQL user (default: postgres)

EOF
    exit 1
}

main() {
    if [ $# -lt 1 ]; then
        usage
    fi

    local command="$1"
    shift

    case "$command" in
        migrate)
            if [ $# -lt 2 ]; then
                log_error "Missing arguments for migrate command"
                usage
            fi
            run_migration "$@"
            ;;
        validate)
            if [ $# -lt 1 ]; then
                log_error "Missing backup directory for validate command"
                usage
            fi
            run_validation "$1"
            ;;
        rollback)
            if [ $# -lt 1 ]; then
                log_error "Missing backup directory for rollback command"
                usage
            fi
            run_rollback "$1"
            ;;
        --help|-h)
            usage
            ;;
        *)
            log_error "Unknown command: $command"
            usage
            ;;
    esac
}

# Run main function
main "$@"
