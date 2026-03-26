"""
Example: Using HeliosDB Nano Feature Fallback System
=====================================================

This example demonstrates how to use the feature fallback system
in a real-world application with HeliosDB Nano.

Author: HeliosDB Team
Version: 3.0.1
"""

import sys
from typing import List, Dict, Any

# Import our fallback modules
from HELIOSDB_SQLITE_FEATURE_HANDLER import (
    HeliosDBFeatureHandler,
    get_feature_handler,
    is_feature_supported
)
from HELIOSDB_SQLITE_WARNINGS import (
    configure_warning_display,
    warn_precision_loss,
    warn_application_level_required
)


def example_1_basic_usage():
    """Example 1: Basic SQL processing with automatic fallbacks"""
    print("=" * 60)
    print("Example 1: Basic SQL Processing")
    print("=" * 60)

    handler = HeliosDBFeatureHandler(
        enable_fallbacks=True,
        warn_on_fallback=True
    )

    # Test DECIMAL conversion
    sql1 = """
    CREATE TABLE accounts (
        id SERIAL PRIMARY KEY,
        balance DECIMAL(10, 2),
        credit_limit NUMERIC(15, 4)
    )
    """

    print("\nOriginal SQL:")
    print(sql1)

    processed = handler.process_sql(sql1)
    print("\nProcessed SQL (with fallbacks):")
    print(processed)

    # Get report
    report = handler.get_fallback_report()
    print(f"\nFallbacks applied: {report['total_fallbacks']}")
    print("By feature:", report['fallbacks_by_feature'])


def example_2_check_constraints():
    """Example 2: CHECK constraint handling"""
    print("\n" + "=" * 60)
    print("Example 2: CHECK Constraint Handling")
    print("=" * 60)

    handler = get_feature_handler()

    sql = """
    CREATE TABLE users (
        id INT4 PRIMARY KEY,
        age INT2 CHECK (age >= 18 AND age <= 120),
        status TEXT CHECK (status IN ('active', 'inactive', 'suspended'))
    )
    """

    print("\nOriginal SQL with CHECK constraints:")
    print(sql)

    processed = handler.process_sql(sql)
    print("\nProcessed SQL (constraints removed):")
    print(processed)

    print("\nIMPLEMENTATION GUIDANCE:")
    print("Add this validation in your application:")
    print("""
    # Pydantic model
    from pydantic import BaseModel, validator

    class User(BaseModel):
        age: int
        status: str

        @validator('age')
        def validate_age(cls, v):
            if not (18 <= v <= 120):
                raise ValueError('Age must be between 18 and 120')
            return v

        @validator('status')
        def validate_status(cls, v):
            if v not in ('active', 'inactive', 'suspended'):
                raise ValueError('Invalid status')
            return v
    """)


def example_3_foreign_keys():
    """Example 3: Foreign key constraint handling"""
    print("\n" + "=" * 60)
    print("Example 3: Foreign Key Constraint Handling")
    print("=" * 60)

    handler = get_feature_handler()

    sql = """
    CREATE TABLE orders (
        id INT4 PRIMARY KEY,
        user_id INT4,
        product_id INT4,
        amount DECIMAL(10, 2),
        FOREIGN KEY (user_id) REFERENCES users(id),
        FOREIGN KEY (product_id) REFERENCES products(id)
    )
    """

    print("\nOriginal SQL with FOREIGN KEY constraints:")
    print(sql)

    processed = handler.process_sql(sql)
    print("\nProcessed SQL (constraints removed, DECIMAL converted):")
    print(processed)

    print("\nIMPLEMENTATION GUIDANCE:")
    print("Add this validation before INSERT/UPDATE:")
    print("""
    def validate_and_insert_order(db, user_id, product_id, amount):
        '''Validate foreign keys before insert'''
        with db.begin():
            # Validate user exists
            user_check = db.query_params(
                "SELECT 1 FROM users WHERE id = $1",
                [user_id]
            )
            if not user_check:
                raise ValueError(f"User {user_id} not found")

            # Validate product exists
            product_check = db.query_params(
                "SELECT 1 FROM products WHERE id = $1",
                [product_id]
            )
            if not product_check:
                raise ValueError(f"Product {product_id} not found")

            # Insert order
            db.execute(
                "INSERT INTO orders (user_id, product_id, amount) VALUES ($1, $2, $3)",
                [user_id, product_id, amount]
            )

            db.commit()
    """)


def example_4_trigger_replacement():
    """Example 4: Trigger replacement with application logic"""
    print("\n" + "=" * 60)
    print("Example 4: Trigger Replacement")
    print("=" * 60)

    handler = get_feature_handler()

    trigger_sql = """
    CREATE TRIGGER audit_account_changes
    AFTER UPDATE ON accounts
    FOR EACH ROW
    EXECUTE FUNCTION log_account_change();
    """

    print("\nOriginal SQL (TRIGGER - not supported):")
    print(trigger_sql)

    processed = handler.process_sql(trigger_sql)
    print("\nProcessed SQL:")
    print(processed)

    print("\nIMPLEMENTATION GUIDANCE:")
    print("Replace with application-level logic:")
    print("""
    # Django signals
    from django.db.models.signals import post_save
    from django.dispatch import receiver

    @receiver(post_save, sender=Account)
    def log_account_change(sender, instance, created, **kwargs):
        if not created:  # UPDATE operation
            AuditLog.objects.create(
                table='accounts',
                operation='UPDATE',
                record_id=instance.id,
                changed_fields=get_changed_fields(instance)
            )

    # SQLAlchemy events
    from sqlalchemy import event

    @event.listens_for(Account, 'after_update')
    def log_account_change(mapper, connection, target):
        connection.execute(
            audit_log.insert().values(
                table='accounts',
                operation='UPDATE',
                record_id=target.id
            )
        )
    """)


def example_5_feature_detection():
    """Example 5: Feature detection and conditional logic"""
    print("\n" + "=" * 60)
    print("Example 5: Feature Detection")
    print("=" * 60)

    features_to_check = [
        "DECIMAL",
        "TRIGGER",
        "CHECK_CONSTRAINT",
        "FOREIGN_KEY",
        "JSONB_OPERATORS",
        "VECTOR_SEARCH",
        "TIME_TRAVEL",
        "CTE"
    ]

    print("\nFeature Support Status:")
    print("-" * 60)

    for feature in features_to_check:
        supported = is_feature_supported(feature)
        info = get_feature_handler().get_feature_info(feature)

        status = "✅ SUPPORTED" if supported else "⚠️  FALLBACK"
        print(f"{feature:20s} {status}")

        if info:
            print(f"  Support Level: {info.support_level.value}")
            if info.fallback_strategy:
                print(f"  Fallback: {info.fallback_strategy.value}")
            print(f"  Guidance: {info.guidance[:60]}...")
            print()


def example_6_strict_mode():
    """Example 6: Strict mode - reject unsupported features"""
    print("\n" + "=" * 60)
    print("Example 6: Strict Mode (Reject Unsupported)")
    print("=" * 60)

    # Create handler in strict mode
    strict_handler = HeliosDBFeatureHandler(
        enable_fallbacks=False,
        strict_mode=True
    )

    sql = "CREATE TABLE test (balance DECIMAL(10, 2))"

    print("\nAttempting to process SQL in strict mode:")
    print(sql)

    try:
        processed = strict_handler.process_sql(sql)
        print("\nProcessed successfully (unexpected):", processed)
    except ValueError as e:
        print("\n❌ Rejected (expected in strict mode):")
        print(f"   Error: {e}")
        print("\n   Solution: Use FLOAT8 instead of DECIMAL, or disable strict mode")


def example_7_comprehensive_report():
    """Example 7: Generate comprehensive fallback report"""
    print("\n" + "=" * 60)
    print("Example 7: Comprehensive Fallback Report")
    print("=" * 60)

    handler = HeliosDBFeatureHandler(enable_fallbacks=True)

    # Process multiple SQL statements
    test_sqls = [
        "CREATE TABLE t1 (balance DECIMAL(10, 2))",
        "CREATE TABLE t2 (age INT CHECK (age >= 18))",
        "CREATE TABLE t3 (user_id INT, FOREIGN KEY (user_id) REFERENCES users(id))",
        "CREATE TRIGGER test_trigger AFTER INSERT ON t1 FOR EACH ROW ...",
    ]

    print("\nProcessing multiple SQL statements...")
    for sql in test_sqls:
        print(f"  - {sql[:50]}...")
        handler.process_sql(sql)

    # Generate report
    report = handler.get_fallback_report()

    print("\n" + "-" * 60)
    print("FALLBACK REPORT")
    print("-" * 60)
    print(f"Total fallbacks applied: {report['total_fallbacks']}")
    print(f"\nFallbacks by feature:")
    for feature, count in report['fallbacks_by_feature'].items():
        print(f"  {feature:20s} {count} occurrence(s)")

    print(f"\nRecommendations:")
    for i, rec in enumerate(report['recommendations'], 1):
        print(f"  {i}. {rec}")


def example_8_custom_handler():
    """Example 8: Register custom fallback handler"""
    print("\n" + "=" * 60)
    print("Example 8: Custom Fallback Handler")
    print("=" * 60)

    handler = HeliosDBFeatureHandler(enable_fallbacks=True)

    # Custom handler for hypothetical ENUM type
    def handle_enum_type(sql: str) -> str:
        """Convert ENUM to TEXT with CHECK constraint"""
        import re
        pattern = r"(\w+)\s+ENUM\s*\('([^']+)'\)"

        def replacement(match):
            col_name = match.group(1)
            values = match.group(2)
            return f"{col_name} TEXT -- CHECK: {col_name} IN ('{values}')"

        return re.sub(pattern, replacement, sql)

    # Register custom handler
    handler.register_custom_handler("ENUM", handle_enum_type)

    sql = "CREATE TABLE users (status ENUM('active','inactive'))"
    print("\nOriginal SQL with custom ENUM type:")
    print(sql)

    # Note: This would need integration into the main processor
    processed = handle_enum_type(sql)
    print("\nProcessed SQL (custom handler):")
    print(processed)


def main():
    """Run all examples"""
    print("\n")
    print("*" * 60)
    print("* HeliosDB Nano Feature Fallback System Examples")
    print("* Version 3.0.1")
    print("*" * 60)

    # Configure warnings to show everything
    configure_warning_display(
        show_warnings=True,
        show_guidance=True,
        show_once_per_feature=True
    )

    try:
        example_1_basic_usage()
        example_2_check_constraints()
        example_3_foreign_keys()
        example_4_trigger_replacement()
        example_5_feature_detection()
        example_6_strict_mode()
        example_7_comprehensive_report()
        example_8_custom_handler()

        print("\n" + "=" * 60)
        print("✅ All examples completed successfully!")
        print("=" * 60)

    except Exception as e:
        print(f"\n❌ Error running examples: {e}", file=sys.stderr)
        import traceback
        traceback.print_exc()
        sys.exit(1)


if __name__ == "__main__":
    main()
