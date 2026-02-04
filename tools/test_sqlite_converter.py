#!/usr/bin/env python3
"""
Test script for SQLite ↔ HeliosDB-Lite converter.

Demonstrates:
- Transparent conversion on first connection
- Type mapping with warnings
- Data integrity verification
- Progress reporting
"""

import sqlite3
import tempfile
import shutil
from pathlib import Path
from HELIOSDB_SQLITE_CONVERTER import (
    SQLiteDetector,
    SQLiteToHeliosDBConverter,
    TransparentConverter,
    ConversionMode,
    HeliosDBFileFormat
)
from HELIOSDB_SQLITE_TYPE_MAPPER import TypeMapper, ValueConverter


def create_test_sqlite_database(db_path: Path):
    """Create a test SQLite database with various data types."""
    print(f"\n{'='*60}")
    print("Creating test SQLite database...")
    print(f"{'='*60}")

    conn = sqlite3.connect(str(db_path))
    cursor = conn.cursor()

    # Create tables with different types
    cursor.execute("""
        CREATE TABLE users (
            id INTEGER PRIMARY KEY,
            username VARCHAR(50) NOT NULL,
            email TEXT,
            age INTEGER,
            balance DECIMAL(10,2),
            is_active BOOLEAN,
            created_at DATETIME,
            metadata JSON
        )
    """)

    cursor.execute("""
        CREATE TABLE products (
            product_id INTEGER PRIMARY KEY,
            name TEXT NOT NULL,
            description TEXT,
            price REAL,
            stock INTEGER,
            category VARCHAR(100)
        )
    """)

    cursor.execute("""
        CREATE TABLE orders (
            order_id INTEGER PRIMARY KEY,
            user_id INTEGER,
            product_id INTEGER,
            quantity INTEGER,
            total DECIMAL(10,2),
            order_date TIMESTAMP
        )
    """)

    # Insert test data
    print("Inserting test data...")

    # Users
    users_data = [
        (1, 'alice', 'alice@example.com', 30, 1234.56, 1, '2024-01-15 10:30:00', '{"role": "admin"}'),
        (2, 'bob', 'bob@example.com', 25, 567.89, 1, '2024-02-20 14:45:00', '{"role": "user"}'),
        (3, 'charlie', 'charlie@example.com', 35, 9876.54, 0, '2024-03-10 09:15:00', '{"role": "user"}'),
    ]
    cursor.executemany(
        "INSERT INTO users VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        users_data
    )

    # Products
    products_data = [
        (1, 'Laptop', 'High-performance laptop', 1299.99, 50, 'Electronics'),
        (2, 'Mouse', 'Wireless mouse', 29.99, 200, 'Electronics'),
        (3, 'Desk', 'Ergonomic standing desk', 599.99, 20, 'Furniture'),
        (4, 'Chair', 'Office chair with lumbar support', 399.99, 30, 'Furniture'),
    ]
    cursor.executemany(
        "INSERT INTO products VALUES (?, ?, ?, ?, ?, ?)",
        products_data
    )

    # Orders
    orders_data = [
        (1, 1, 1, 1, 1299.99, '2024-04-01 12:00:00'),
        (2, 2, 2, 2, 59.98, '2024-04-02 15:30:00'),
        (3, 1, 3, 1, 599.99, '2024-04-03 10:15:00'),
        (4, 3, 4, 1, 399.99, '2024-04-04 14:20:00'),
    ]
    cursor.executemany(
        "INSERT INTO orders VALUES (?, ?, ?, ?, ?, ?)",
        orders_data
    )

    # Create indexes
    print("Creating indexes...")
    cursor.execute("CREATE INDEX idx_users_email ON users(email)")
    cursor.execute("CREATE INDEX idx_products_category ON products(category)")
    cursor.execute("CREATE UNIQUE INDEX idx_users_username ON users(username)")

    conn.commit()
    conn.close()

    print(f"✓ Created database with 3 tables")
    print(f"  - users: {len(users_data)} rows")
    print(f"  - products: {len(products_data)} rows")
    print(f"  - orders: {len(orders_data)} rows")


def test_sqlite_detection(db_path: Path):
    """Test SQLite file detection and validation."""
    print(f"\n{'='*60}")
    print("Testing SQLite Detection")
    print(f"{'='*60}")

    # Test file detection
    is_sqlite = SQLiteDetector.is_sqlite_file(db_path)
    print(f"Is SQLite file: {is_sqlite}")
    assert is_sqlite, "Should detect SQLite file"

    # Test validation
    is_valid, error = SQLiteDetector.validate_sqlite_database(db_path)
    print(f"Is valid: {is_valid}")
    if error:
        print(f"Error: {error}")
    assert is_valid, "Should be valid SQLite database"

    # Get database info
    info = SQLiteDetector.get_database_info(db_path)
    print(f"\nDatabase Info:")
    print(f"  File size: {info['file_size']:,} bytes")
    print(f"  Page size: {info['page_size']} bytes")
    print(f"  Schema version: {info['schema_version']}")
    print(f"  Total rows: {info['total_rows']}")
    print(f"  Tables:")
    for table in info['tables']:
        print(f"    - {table['name']}: {table['row_count']} rows")

    print("\n✓ SQLite detection tests passed")


def test_type_mapping():
    """Test type mapping between SQLite and HeliosDB."""
    print(f"\n{'='*60}")
    print("Testing Type Mapping")
    print(f"{'='*60}")

    mapper = TypeMapper()

    # Test SQLite → HeliosDB mappings
    print("\nSQLite → HeliosDB:")
    test_cases = [
        ('INTEGER', 'INT8', False),
        ('VARCHAR(100)', 'VARCHAR(100)', False),
        ('DECIMAL(10,2)', 'FLOAT8', True),  # Should warn
        ('TEXT', 'TEXT', False),
        ('BOOLEAN', 'BOOLEAN', False),
        ('DATETIME', 'TIMESTAMP', False),
        ('BLOB', 'BYTEA', False),
    ]

    for sqlite_type, expected_helios, should_warn in test_cases:
        helios_type, warning = mapper.sqlite_to_heliosdb(sqlite_type)
        print(f"  {sqlite_type:20} → {helios_type:15}", end='')

        assert helios_type == expected_helios, \
            f"Expected {expected_helios}, got {helios_type}"

        if should_warn:
            assert warning is not None, "Expected warning"
            print(f" [WARNING: {warning.message}]")
        else:
            print()

    # Test HeliosDB → SQLite mappings
    print("\nHeliosDB → SQLite:")
    reverse_cases = [
        ('INT4', 'INTEGER'),
        ('FLOAT8', 'REAL'),
        ('TEXT', 'TEXT'),
        ('BOOLEAN', 'INTEGER'),
        ('TIMESTAMP', 'TEXT'),
        ('BYTEA', 'BLOB'),
    ]

    for helios_type, expected_sqlite in reverse_cases:
        sqlite_type, warning = mapper.heliosdb_to_sqlite(helios_type)
        print(f"  {helios_type:20} → {sqlite_type:15}")
        assert sqlite_type == expected_sqlite, \
            f"Expected {expected_sqlite}, got {sqlite_type}"

    print("\n✓ Type mapping tests passed")


def test_manual_conversion(sqlite_path: Path, heliosdb_path: Path):
    """Test manual conversion with progress reporting."""
    print(f"\n{'='*60}")
    print("Testing Manual Conversion")
    print(f"{'='*60}")

    # Create converter
    converter = SQLiteToHeliosDBConverter(
        sqlite_path=sqlite_path,
        heliosdb_path=heliosdb_path,
        mode=ConversionMode.STREAMING,
        verify_integrity=True
    )

    # Track progress updates
    progress_updates = []

    def on_progress(progress):
        """Capture progress updates."""
        progress_updates.append({
            'percentage': progress.progress_percentage(),
            'table': progress.current_table,
            'rows': progress.converted_rows,
            'total': progress.total_rows
        })

        pct = progress.progress_percentage()
        eta = progress.estimated_time_remaining()
        eta_str = f", ETA: {eta:.1f}s" if eta else ""

        print(f"  Progress: {pct:5.1f}% - {progress.current_table:20} "
              f"({progress.converted_rows}/{progress.total_rows} rows){eta_str}")

    # Run conversion
    print("\nStarting conversion...")
    success = converter.convert(progress_callback=on_progress)

    # Check results
    assert success, "Conversion should succeed"
    assert len(progress_updates) > 0, "Should have progress updates"

    print(f"\nConversion Summary:")
    print(f"  Status: {converter.progress.status.value}")
    print(f"  Tables: {converter.progress.converted_tables}/{converter.progress.total_tables}")
    print(f"  Rows: {converter.progress.converted_rows}/{converter.progress.total_rows}")
    print(f"  Time: {converter.progress.elapsed_time():.2f}s")

    if converter.progress.warnings:
        print(f"\nWarnings ({len(converter.progress.warnings)}):")
        for warning in converter.progress.warnings:
            print(f"  - {warning}")

    print("\n✓ Manual conversion test passed")


def test_transparent_conversion(sqlite_path: Path):
    """Test transparent automatic conversion."""
    print(f"\n{'='*60}")
    print("Testing Transparent Conversion")
    print(f"{'='*60}")

    # First connection - should trigger conversion
    print("\nFirst connection (should convert):")
    success, conn, messages = TransparentConverter.connect_with_auto_conversion(
        sqlite_path
    )

    for msg in messages:
        print(f"  {msg}")

    assert success, "Should succeed on first connection"

    # Check HeliosDB directory was created
    heliosdb_path = sqlite_path.parent / f"{sqlite_path.stem}.heliosdb"
    assert heliosdb_path.exists(), "HeliosDB directory should exist"

    # Verify it's a HeliosDB database
    is_heliosdb = HeliosDBFileFormat.is_heliosdb_file(heliosdb_path)
    print(f"\nIs HeliosDB: {is_heliosdb}")

    # Second connection - should use existing database
    print("\nSecond connection (should use existing):")
    success2, conn2, messages2 = TransparentConverter.connect_with_auto_conversion(
        heliosdb_path
    )

    for msg in messages2:
        print(f"  {msg}")

    assert success2, "Should succeed on second connection"

    print("\n✓ Transparent conversion test passed")


def test_value_conversion():
    """Test value conversion between formats."""
    print(f"\n{'='*60}")
    print("Testing Value Conversion")
    print(f"{'='*60}")

    converter = ValueConverter()

    # Test SQLite → HeliosDB value conversions
    print("\nValue Conversions:")

    test_values = [
        (123, 'INTEGER', 'INT8', 123),
        ('hello', 'TEXT', 'TEXT', 'hello'),
        (1, 'BOOLEAN', 'BOOLEAN', True),
        (0, 'BOOLEAN', 'BOOLEAN', False),
        (3.14159, 'REAL', 'FLOAT8', 3.14159),
    ]

    for sqlite_val, sqlite_type, helios_type, expected in test_values:
        result = converter.sqlite_value_to_heliosdb(
            sqlite_val, sqlite_type, helios_type
        )
        print(f"  {sqlite_val} ({sqlite_type}) → {result} ({helios_type})")
        assert result == expected, f"Expected {expected}, got {result}"

    print("\n✓ Value conversion tests passed")


def run_all_tests():
    """Run all converter tests."""
    print("\n" + "="*60)
    print("HeliosDB-Lite SQLite Converter Test Suite")
    print("="*60)

    # Create temporary directory for tests
    with tempfile.TemporaryDirectory() as tmpdir:
        tmpdir_path = Path(tmpdir)

        # Test files
        sqlite_db = tmpdir_path / "test.sqlite"
        heliosdb_dir = tmpdir_path / "test.heliosdb"

        # Run tests
        try:
            # 1. Create test database
            create_test_sqlite_database(sqlite_db)

            # 2. Test detection
            test_sqlite_detection(sqlite_db)

            # 3. Test type mapping
            test_type_mapping()

            # 4. Test value conversion
            test_value_conversion()

            # 5. Test manual conversion
            test_manual_conversion(sqlite_db, heliosdb_dir)

            # 6. Test transparent conversion (using a copy)
            sqlite_db_copy = tmpdir_path / "test_transparent.sqlite"
            shutil.copy(sqlite_db, sqlite_db_copy)
            test_transparent_conversion(sqlite_db_copy)

            # All tests passed
            print("\n" + "="*60)
            print("✓ ALL TESTS PASSED")
            print("="*60)
            print("\nConverter Features Verified:")
            print("  ✓ SQLite file detection and validation")
            print("  ✓ Type mapping (SQLite ↔ HeliosDB)")
            print("  ✓ Value conversion with type coercion")
            print("  ✓ Manual conversion with progress reporting")
            print("  ✓ Transparent automatic conversion")
            print("  ✓ Data integrity verification")
            print("  ✓ Warning generation for lossy conversions")

            return True

        except AssertionError as e:
            print(f"\n✗ TEST FAILED: {e}")
            return False
        except Exception as e:
            print(f"\n✗ UNEXPECTED ERROR: {e}")
            import traceback
            traceback.print_exc()
            return False


if __name__ == '__main__':
    import sys
    success = run_all_tests()
    sys.exit(0 if success else 1)
