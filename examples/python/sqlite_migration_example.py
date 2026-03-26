#!/usr/bin/env python3
"""
Example: Migrating from SQLite to HeliosDB Nano

This example demonstrates how to use the SQLite converter in a real application.
Scenarios covered:
1. Transparent migration (zero code changes)
2. Explicit migration with progress monitoring
3. Batch migration of multiple databases
4. Production migration with verification
"""

import sys
from pathlib import Path

# Add tools directory to path
sys.path.insert(0, str(Path(__file__).parent.parent / 'tools'))

from HELIOSDB_SQLITE_CONVERTER import (
    TransparentConverter,
    SQLiteToHeliosDBConverter,
    ConversionMode
)


def example_1_transparent_migration():
    """
    Example 1: Transparent Migration (Simplest)

    Your existing application uses SQLite. You want to migrate to HeliosDB
    without changing any code. Just call connect() and it automatically
    converts on first use.
    """
    print("\n" + "="*70)
    print("Example 1: Transparent Migration")
    print("="*70)

    # Your existing SQLite database
    db_path = Path("app_database.sqlite")

    # Normally you would do: conn = sqlite3.connect(db_path)
    # Instead, use transparent converter:

    success, heliosdb_conn, messages = TransparentConverter.connect_with_auto_conversion(
        db_path
    )

    if success:
        print("\n✓ Database ready!")
        print("\nConversion messages:")
        for msg in messages:
            print(f"  {msg}")

        # Use heliosdb_conn just like SQLite connection
        # (In production, this would be actual HeliosDB connection object)
    else:
        print("\n✗ Migration failed!")
        for msg in messages:
            print(f"  ERROR: {msg}")


def example_2_monitored_migration():
    """
    Example 2: Monitored Migration with Progress Bar

    For large databases, you want to show users a progress bar
    and estimated time remaining during migration.
    """
    print("\n" + "="*70)
    print("Example 2: Monitored Migration with Progress")
    print("="*70)

    sqlite_path = Path("large_database.sqlite")
    heliosdb_path = Path("large_database.heliosdb")

    # Create converter with streaming mode (for large files)
    converter = SQLiteToHeliosDBConverter(
        sqlite_path=sqlite_path,
        heliosdb_path=heliosdb_path,
        mode=ConversionMode.STREAMING,
        verify_integrity=True
    )

    # Define progress callback for UI updates
    def update_progress_bar(progress):
        """Called periodically during conversion."""
        pct = progress.progress_percentage()
        eta = progress.estimated_time_remaining()

        # Calculate progress bar
        bar_width = 50
        filled = int(bar_width * pct / 100)
        bar = '█' * filled + '░' * (bar_width - filled)

        # Format ETA
        if eta:
            eta_str = f"ETA: {int(eta // 60)}m {int(eta % 60)}s"
        else:
            eta_str = "Calculating..."

        # Print progress bar (overwrite previous line)
        print(f"\r{bar} {pct:5.1f}% | {progress.current_table:20} | {eta_str}", end='')

    # Run conversion
    print("\nStarting migration...")
    success = converter.convert(progress_callback=update_progress_bar)
    print()  # New line after progress bar

    if success:
        print("\n✓ Migration completed successfully!")
        print(f"  Tables: {converter.progress.converted_tables}")
        print(f"  Rows: {converter.progress.converted_rows:,}")
        print(f"  Time: {converter.progress.elapsed_time():.1f}s")

        # Show warnings if any
        if converter.progress.warnings:
            print(f"\n⚠ Warnings ({len(converter.progress.warnings)}):")
            for warning in converter.progress.warnings:
                print(f"  - {warning}")
    else:
        print("\n✗ Migration failed!")
        for error in converter.progress.errors:
            print(f"  ERROR: {error}")


def example_3_batch_migration():
    """
    Example 3: Batch Migration

    Migrate multiple SQLite databases at once, useful for
    multi-tenant applications or batch processing.
    """
    print("\n" + "="*70)
    print("Example 3: Batch Migration of Multiple Databases")
    print("="*70)

    # List of databases to migrate
    databases = [
        "tenant_001.sqlite",
        "tenant_002.sqlite",
        "tenant_003.sqlite",
    ]

    results = []

    for db_file in databases:
        sqlite_path = Path(db_file)
        heliosdb_path = Path(f"{sqlite_path.stem}.heliosdb")

        print(f"\nMigrating {db_file}...")

        converter = SQLiteToHeliosDBConverter(
            sqlite_path=sqlite_path,
            heliosdb_path=heliosdb_path,
            mode=ConversionMode.STREAMING
        )

        success = converter.convert()

        results.append({
            'database': db_file,
            'success': success,
            'tables': converter.progress.converted_tables,
            'rows': converter.progress.converted_rows,
            'time': converter.progress.elapsed_time()
        })

        if success:
            print(f"  ✓ Completed in {converter.progress.elapsed_time():.1f}s")
        else:
            print(f"  ✗ Failed: {converter.progress.errors}")

    # Summary
    print("\n" + "="*70)
    print("Migration Summary")
    print("="*70)

    successful = sum(1 for r in results if r['success'])
    failed = len(results) - successful

    print(f"\nTotal: {len(results)} databases")
    print(f"  ✓ Successful: {successful}")
    print(f"  ✗ Failed: {failed}")

    print("\nDetails:")
    for result in results:
        status = "✓" if result['success'] else "✗"
        print(f"  {status} {result['database']:30} "
              f"{result['tables']:2} tables, "
              f"{result['rows']:6} rows, "
              f"{result['time']:6.1f}s")


def example_4_production_migration():
    """
    Example 4: Production Migration with Verification

    For production environments, you want:
    - Pre-migration validation
    - Post-migration verification
    - Detailed logging
    - Rollback on failure
    """
    print("\n" + "="*70)
    print("Example 4: Production Migration with Verification")
    print("="*70)

    sqlite_path = Path("production.sqlite")
    heliosdb_path = Path("production.heliosdb")

    print("\n1. Pre-migration validation...")

    # Check SQLite database integrity
    from HELIOSDB_SQLITE_CONVERTER import SQLiteDetector

    is_valid, error = SQLiteDetector.validate_sqlite_database(sqlite_path)

    if not is_valid:
        print(f"  ✗ SQLite validation failed: {error}")
        return

    print("  ✓ SQLite database is valid")

    # Get database info
    info = SQLiteDetector.get_database_info(sqlite_path)
    print(f"  - Size: {info['file_size'] / (1024*1024):.1f} MB")
    print(f"  - Tables: {len(info['tables'])}")
    print(f"  - Total rows: {info['total_rows']:,}")

    # Check disk space (simplified)
    import shutil
    free_space = shutil.disk_usage(sqlite_path.parent).free
    required_space = info['file_size'] * 2.5  # 250% of original

    if free_space < required_space:
        print(f"  ✗ Insufficient disk space: {free_space / (1024**3):.1f} GB available, "
              f"{required_space / (1024**3):.1f} GB required")
        return

    print(f"  ✓ Sufficient disk space: {free_space / (1024**3):.1f} GB available")

    print("\n2. Starting migration...")

    converter = SQLiteToHeliosDBConverter(
        sqlite_path=sqlite_path,
        heliosdb_path=heliosdb_path,
        mode=ConversionMode.STREAMING,
        verify_integrity=True  # Enable integrity verification
    )

    # Track detailed progress
    conversion_log = []

    def log_progress(progress):
        """Log progress for audit trail."""
        conversion_log.append({
            'timestamp': progress.elapsed_time(),
            'table': progress.current_table,
            'rows': progress.converted_rows,
            'percentage': progress.progress_percentage()
        })

        # Print progress
        pct = progress.progress_percentage()
        print(f"  Progress: {pct:5.1f}% - {progress.current_table}")

    success = converter.convert(progress_callback=log_progress)

    if success:
        print("\n3. Post-migration verification...")

        # Verify integrity was already done by converter
        print("  ✓ Row counts verified")
        print("  ✓ Schema integrity verified")

        # Check HeliosDB directory
        from HELIOSDB_SQLITE_CONVERTER import HeliosDBFileFormat

        if HeliosDBFileFormat.is_heliosdb_file(heliosdb_path):
            print("  ✓ HeliosDB database created successfully")

        print("\n4. Migration Summary:")
        print(f"  ✓ Status: SUCCESSFUL")
        print(f"  - Tables: {converter.progress.converted_tables}")
        print(f"  - Rows: {converter.progress.converted_rows:,}")
        print(f"  - Time: {converter.progress.elapsed_time():.1f}s")
        print(f"  - Average speed: {converter.progress.converted_rows / converter.progress.elapsed_time():.0f} rows/sec")

        if converter.progress.warnings:
            print(f"\n  ⚠ Warnings ({len(converter.progress.warnings)}):")
            for warning in converter.progress.warnings:
                print(f"    - {warning}")

        print("\n5. Next steps:")
        print("  1. Test HeliosDB database with your application")
        print("  2. Run application smoke tests")
        print("  3. Compare query results with SQLite")
        print("  4. Archive original SQLite database")
        print("  5. Update connection strings to use HeliosDB")

    else:
        print("\n✗ Migration failed!")
        print(f"  Status: {converter.progress.status.value}")
        print("\nErrors:")
        for error in converter.progress.errors:
            print(f"  - {error}")

        print("\nRollback:")
        print("  - Partial HeliosDB database removed automatically")
        print("  - Original SQLite database preserved")
        print("\nRecommendations:")
        print("  1. Review error messages above")
        print("  2. Check SQLite database integrity")
        print("  3. Ensure sufficient disk space")
        print("  4. Try row-by-row mode for better error isolation")


def main():
    """Run all examples."""
    print("="*70)
    print("SQLite to HeliosDB Nano Migration Examples")
    print("="*70)
    print("\nThis script demonstrates various migration scenarios.")
    print("Note: Examples use mock databases for demonstration.")

    # In a real application, you would uncomment the examples you want to run:

    # example_1_transparent_migration()
    # example_2_monitored_migration()
    # example_3_batch_migration()
    # example_4_production_migration()

    print("\n" + "="*70)
    print("To run examples, uncomment the function calls in main()")
    print("="*70)


if __name__ == '__main__':
    main()
