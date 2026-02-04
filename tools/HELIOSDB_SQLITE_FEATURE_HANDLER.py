"""
HeliosDB SQLite Feature Handler
================================

Provides intelligent fallback mechanisms for HeliosDB-Lite features not available in SQLite.
Detects unsupported features and provides graceful degradation with clear warnings.

This module is designed for production use and handles:
- DECIMAL/NUMERIC → FLOAT conversion with precision warnings
- Triggers → execution rejection with guidance
- CHECK constraints → application-level validation option
- AUTOINCREMENT → UUID or explicit sequence fallback
- Foreign key enforcement → application-level validation option
- Feature capability detection
- Graceful degradation with comprehensive logging

Author: HeliosDB Team
License: Apache-2.0
Version: 3.0.1
"""

import re
import logging
import warnings
from typing import Dict, List, Optional, Tuple, Any, Callable
from enum import Enum
from dataclasses import dataclass
from contextlib import contextmanager

from HELIOSDB_SQLITE_WARNINGS import (
    FeatureNotSupportedWarning,
    PrecisionLossWarning,
    FallbackStrategyWarning,
    HeliosDBFeatureWarning,
    FallbackLogger
)


class FeatureSupport(Enum):
    """Feature support levels in HeliosDB-Lite"""
    FULLY_SUPPORTED = "fully_supported"
    PARTIAL_SUPPORT = "partial_support"
    FALLBACK_AVAILABLE = "fallback_available"
    NOT_SUPPORTED = "not_supported"
    DEPRECATED = "deprecated"


class FallbackStrategy(Enum):
    """Strategies for handling unsupported features"""
    TYPE_CONVERSION = "type_conversion"  # Convert to compatible type
    APPLICATION_LEVEL = "application_level"  # Handle in application code
    SILENT_IGNORE = "silent_ignore"  # Ignore with warning
    REJECT_WITH_ERROR = "reject_with_error"  # Fail with helpful error
    UUID_REPLACEMENT = "uuid_replacement"  # Use UUID instead of AUTOINCREMENT
    MANUAL_SEQUENCE = "manual_sequence"  # Manual sequence management


@dataclass
class FeatureCapability:
    """Represents a feature's support level and fallback strategy"""
    name: str
    support_level: FeatureSupport
    fallback_strategy: Optional[FallbackStrategy]
    description: str
    guidance: str
    performance_impact: str
    security_considerations: Optional[str] = None


class HeliosDBFeatureHandler:
    """
    Main handler for feature detection and fallback mechanisms.

    Provides intelligent detection of unsupported features and automatic
    fallback strategies with comprehensive logging and warnings.

    Example usage:
        handler = HeliosDBFeatureHandler(enable_fallbacks=True)

        # Process SQL with automatic fallback
        processed_sql = handler.process_sql(
            "CREATE TABLE users (balance DECIMAL(10,2), id SERIAL)"
        )

        # Check feature support
        if handler.is_feature_supported("DECIMAL"):
            # Feature available
            pass
        else:
            # Use fallback
            pass
    """

    # Feature capability matrix
    FEATURE_MATRIX: Dict[str, FeatureCapability] = {
        "DECIMAL": FeatureCapability(
            name="DECIMAL/NUMERIC",
            support_level=FeatureSupport.FALLBACK_AVAILABLE,
            fallback_strategy=FallbackStrategy.TYPE_CONVERSION,
            description="Arbitrary precision numeric types",
            guidance="Convert to FLOAT8 (64-bit float). Precision loss may occur for very large numbers or financial calculations.",
            performance_impact="Minimal - FLOAT8 is native and fast",
            security_considerations="Financial calculations may have precision issues. Use integer cents/pennies for money."
        ),
        "NUMERIC": FeatureCapability(
            name="NUMERIC",
            support_level=FeatureSupport.FALLBACK_AVAILABLE,
            fallback_strategy=FallbackStrategy.TYPE_CONVERSION,
            description="Arbitrary precision numeric types",
            guidance="Convert to FLOAT8 (64-bit float). Same as DECIMAL - precision loss may occur.",
            performance_impact="Minimal - FLOAT8 is native and fast",
            security_considerations="Financial calculations may have precision issues. Use integer cents/pennies for money."
        ),
        "TRIGGER": FeatureCapability(
            name="TRIGGER",
            support_level=FeatureSupport.NOT_SUPPORTED,
            fallback_strategy=FallbackStrategy.APPLICATION_LEVEL,
            description="Database triggers (BEFORE/AFTER INSERT/UPDATE/DELETE)",
            guidance="Implement trigger logic in application code. Consider using middleware or ORM hooks.",
            performance_impact="Depends on application implementation",
            security_considerations="Ensure application-level logic maintains same security guarantees."
        ),
        "CHECK_CONSTRAINT": FeatureCapability(
            name="CHECK Constraint",
            support_level=FeatureSupport.FALLBACK_AVAILABLE,
            fallback_strategy=FallbackStrategy.APPLICATION_LEVEL,
            description="CHECK constraints for data validation",
            guidance="Implement validation in application layer before INSERT/UPDATE. Consider using ORM validators.",
            performance_impact="Minimal if properly implemented",
            security_considerations="Critical - ensure all entry points validate data."
        ),
        "AUTOINCREMENT": FeatureCapability(
            name="AUTOINCREMENT",
            support_level=FeatureSupport.FALLBACK_AVAILABLE,
            fallback_strategy=FallbackStrategy.UUID_REPLACEMENT,
            description="Auto-incrementing primary keys",
            guidance="Use UUID (universally unique identifiers) or implement manual sequence generation. INT4/INT8 with explicit values.",
            performance_impact="UUID: Slightly larger storage. Manual sequence: Additional query overhead.",
            security_considerations="UUIDs are cryptographically secure. Manual sequences need concurrency handling."
        ),
        "SERIAL": FeatureCapability(
            name="SERIAL (PostgreSQL)",
            support_level=FeatureSupport.FALLBACK_AVAILABLE,
            fallback_strategy=FallbackStrategy.UUID_REPLACEMENT,
            description="PostgreSQL SERIAL type (auto-incrementing INT4)",
            guidance="HeliosDB-Lite partially supports SERIAL as INT4. Use UUID for guaranteed uniqueness across distributed systems.",
            performance_impact="UUID: Slightly larger storage and index overhead.",
            security_considerations="SERIAL without sequence may have gaps. UUID eliminates this concern."
        ),
        "FOREIGN_KEY": FeatureCapability(
            name="Foreign Key Constraint",
            support_level=FeatureSupport.FALLBACK_AVAILABLE,
            fallback_strategy=FallbackStrategy.APPLICATION_LEVEL,
            description="Referential integrity enforcement",
            guidance="Implement foreign key checks in application. Validate references before INSERT/UPDATE. Use transactions.",
            performance_impact="Application queries add overhead. Batch validation can optimize.",
            security_considerations="Critical - ensure orphaned records don't compromise data integrity."
        ),
        "JSONB_OPERATORS": FeatureCapability(
            name="JSONB Operators",
            support_level=FeatureSupport.FULLY_SUPPORTED,
            fallback_strategy=None,
            description="PostgreSQL JSONB operators (@>, ->, ->>, etc.)",
            guidance="Fully supported in HeliosDB-Lite v3.0+",
            performance_impact="Native support with GIN indexes",
            security_considerations=None
        ),
        "VECTOR_SEARCH": FeatureCapability(
            name="Vector Search (pgvector compatible)",
            support_level=FeatureSupport.FULLY_SUPPORTED,
            fallback_strategy=None,
            description="Vector similarity search with HNSW indexing",
            guidance="Fully supported with <->, <=>, <#> operators",
            performance_impact="Optimized HNSW index for efficient nearest neighbor search",
            security_considerations=None
        ),
        "TIME_TRAVEL": FeatureCapability(
            name="Time Travel Queries",
            support_level=FeatureSupport.FULLY_SUPPORTED,
            fallback_strategy=None,
            description="AS OF TIMESTAMP queries for historical data",
            guidance="Fully supported in HeliosDB-Lite v3.0+ with MVCC",
            performance_impact="Minimal overhead with snapshot isolation",
            security_considerations=None
        ),
        "CTE": FeatureCapability(
            name="Common Table Expressions (WITH)",
            support_level=FeatureSupport.FULLY_SUPPORTED,
            fallback_strategy=None,
            description="WITH clause for complex queries",
            guidance="Fully supported in HeliosDB-Lite",
            performance_impact="Optimized execution",
            security_considerations=None
        ),
    }

    def __init__(
        self,
        enable_fallbacks: bool = True,
        warn_on_fallback: bool = True,
        strict_mode: bool = False,
        logger: Optional[logging.Logger] = None
    ):
        """
        Initialize the feature handler.

        Args:
            enable_fallbacks: If True, automatically apply fallback strategies
            warn_on_fallback: If True, emit warnings when fallbacks are used
            strict_mode: If True, reject unsupported features instead of falling back
            logger: Custom logger instance (creates default if None)
        """
        self.enable_fallbacks = enable_fallbacks
        self.warn_on_fallback = warn_on_fallback
        self.strict_mode = strict_mode
        self.logger = logger or FallbackLogger.get_logger()

        # Track fallbacks applied during session
        self.fallback_history: List[Dict[str, Any]] = []

        # Custom fallback handlers (can be extended)
        self.custom_handlers: Dict[str, Callable] = {}

    def is_feature_supported(self, feature_name: str) -> bool:
        """
        Check if a feature is fully supported.

        Args:
            feature_name: Name of the feature (e.g., "DECIMAL", "TRIGGER")

        Returns:
            True if fully supported, False otherwise
        """
        feature = self.FEATURE_MATRIX.get(feature_name.upper())
        if not feature:
            return False
        return feature.support_level == FeatureSupport.FULLY_SUPPORTED

    def get_feature_info(self, feature_name: str) -> Optional[FeatureCapability]:
        """
        Get detailed information about a feature's support level.

        Args:
            feature_name: Name of the feature

        Returns:
            FeatureCapability object with details, or None if not found
        """
        return self.FEATURE_MATRIX.get(feature_name.upper())

    def process_sql(self, sql: str) -> str:
        """
        Process SQL statement and apply fallback strategies.

        Detects unsupported features and automatically applies fallback
        transformations. Logs warnings and tracks all changes.

        Args:
            sql: SQL statement to process

        Returns:
            Modified SQL with fallbacks applied

        Raises:
            ValueError: If strict_mode=True and unsupported features detected
        """
        original_sql = sql
        modified_sql = sql

        # Apply each transformation in sequence
        modified_sql = self._handle_decimal_numeric(modified_sql)
        modified_sql = self._handle_triggers(modified_sql)
        modified_sql = self._handle_check_constraints(modified_sql)
        modified_sql = self._handle_autoincrement(modified_sql)
        modified_sql = self._handle_foreign_keys(modified_sql)

        # Log if SQL was modified
        if modified_sql != original_sql:
            self.logger.info(
                "SQL transformed with fallbacks",
                extra={
                    "original": original_sql[:100],
                    "modified": modified_sql[:100],
                    "fallback_count": len(self.fallback_history)
                }
            )

        return modified_sql

    def _handle_decimal_numeric(self, sql: str) -> str:
        """
        Convert DECIMAL/NUMERIC types to FLOAT8.

        Handles patterns like:
        - DECIMAL
        - DECIMAL(10)
        - DECIMAL(10, 2)
        - NUMERIC
        - NUMERIC(15, 4)
        """
        # Pattern matches DECIMAL or NUMERIC with optional precision/scale
        pattern = r'\b(DECIMAL|NUMERIC)\s*(?:\(\s*\d+\s*(?:,\s*\d+\s*)?\))?'

        matches = list(re.finditer(pattern, sql, re.IGNORECASE))
        if not matches:
            return sql

        if self.strict_mode:
            raise ValueError(
                "DECIMAL/NUMERIC not supported in strict mode. "
                "Use FLOAT8 or enable fallbacks."
            )

        # Emit warning about precision loss
        if self.warn_on_fallback:
            warnings.warn(
                "DECIMAL/NUMERIC converted to FLOAT8. Precision loss may occur. "
                "For financial calculations, consider storing values as integer cents/pennies.",
                PrecisionLossWarning,
                stacklevel=2
            )

        # Track fallback
        self.fallback_history.append({
            "feature": "DECIMAL/NUMERIC",
            "strategy": FallbackStrategy.TYPE_CONVERSION,
            "original": [m.group(0) for m in matches],
            "fallback": "FLOAT8"
        })

        # Replace all occurrences
        modified = re.sub(pattern, 'FLOAT8', sql, flags=re.IGNORECASE)

        self.logger.warning(
            "DECIMAL/NUMERIC converted to FLOAT8",
            extra={
                "count": len(matches),
                "guidance": "For financial data, use integer cents (INT8) to avoid precision loss"
            }
        )

        return modified

    def _handle_triggers(self, sql: str) -> str:
        """
        Detect and handle trigger definitions.

        Triggers are not supported - must be implemented in application logic.
        """
        trigger_pattern = r'\bCREATE\s+(?:OR\s+REPLACE\s+)?TRIGGER\b'

        if re.search(trigger_pattern, sql, re.IGNORECASE):
            if self.strict_mode:
                raise ValueError(
                    "TRIGGER not supported. Implement trigger logic in application code."
                )

            if self.warn_on_fallback:
                warnings.warn(
                    "CREATE TRIGGER detected. Triggers are not supported in HeliosDB-Lite. "
                    "Implement trigger logic in your application layer (ORM hooks, middleware, etc.).",
                    FeatureNotSupportedWarning,
                    stacklevel=2
                )

            self.fallback_history.append({
                "feature": "TRIGGER",
                "strategy": FallbackStrategy.APPLICATION_LEVEL,
                "original": "CREATE TRIGGER",
                "fallback": "Application-level implementation required"
            })

            self.logger.error(
                "TRIGGER statement detected but not supported",
                extra={
                    "guidance": "Move trigger logic to application code",
                    "recommendations": [
                        "Use ORM pre/post save hooks",
                        "Implement validation middleware",
                        "Add application-level event listeners"
                    ]
                }
            )

            # Return original SQL with comment explaining rejection
            return f"-- TRIGGER NOT SUPPORTED: {sql}\n-- Implement this logic in application code"

        return sql

    def _handle_check_constraints(self, sql: str) -> str:
        """
        Detect CHECK constraints and provide guidance.

        CHECK constraints should be implemented in application validation.
        """
        check_pattern = r'\bCHECK\s*\([^)]+\)'

        matches = list(re.finditer(check_pattern, sql, re.IGNORECASE))
        if not matches:
            return sql

        if self.warn_on_fallback:
            for match in matches:
                warnings.warn(
                    f"CHECK constraint detected: {match.group(0)}. "
                    "Implement this validation in application code before INSERT/UPDATE.",
                    FallbackStrategyWarning,
                    stacklevel=2
                )

        self.fallback_history.append({
            "feature": "CHECK_CONSTRAINT",
            "strategy": FallbackStrategy.APPLICATION_LEVEL,
            "original": [m.group(0) for m in matches],
            "fallback": "Application-level validation"
        })

        self.logger.warning(
            "CHECK constraints detected",
            extra={
                "count": len(matches),
                "constraints": [m.group(0) for m in matches],
                "guidance": "Implement validation in application layer before writes"
            }
        )

        # Remove CHECK constraints from SQL (keep table creation)
        if not self.strict_mode:
            modified = re.sub(check_pattern, '', sql, flags=re.IGNORECASE)
            # Clean up extra commas and whitespace
            modified = re.sub(r',\s*,', ',', modified)
            modified = re.sub(r',\s*\)', ')', modified)
            return modified
        else:
            raise ValueError("CHECK constraints not supported in strict mode.")

    def _handle_autoincrement(self, sql: str) -> str:
        """
        Handle AUTOINCREMENT and provide UUID alternative.

        Suggests using UUID for primary keys instead of AUTOINCREMENT.
        """
        autoincrement_pattern = r'\bAUTOINCREMENT\b'

        if re.search(autoincrement_pattern, sql, re.IGNORECASE):
            if self.warn_on_fallback:
                warnings.warn(
                    "AUTOINCREMENT detected. Consider using UUID type for primary keys. "
                    "HeliosDB-Lite SERIAL type provides auto-increment for INT4/INT8, but "
                    "UUIDs are better for distributed systems and avoid sequence gaps.",
                    FallbackStrategyWarning,
                    stacklevel=2
                )

            self.fallback_history.append({
                "feature": "AUTOINCREMENT",
                "strategy": FallbackStrategy.UUID_REPLACEMENT,
                "original": "AUTOINCREMENT",
                "fallback": "UUID or manual sequence management"
            })

            self.logger.warning(
                "AUTOINCREMENT detected",
                extra={
                    "guidance": "Use UUID for guaranteed uniqueness",
                    "alternatives": [
                        "SERIAL (INT4 with auto-increment, may have gaps)",
                        "BIGSERIAL (INT8 with auto-increment)",
                        "UUID (recommended for distributed systems)"
                    ]
                }
            )

            # Remove AUTOINCREMENT keyword but keep column definition
            modified = re.sub(autoincrement_pattern, '', sql, flags=re.IGNORECASE)
            return modified

        return sql

    def _handle_foreign_keys(self, sql: str) -> str:
        """
        Detect foreign key constraints and provide guidance.

        Foreign key enforcement should be done in application layer.
        """
        fk_pattern = r'\bFOREIGN\s+KEY\s*\([^)]+\)\s*REFERENCES\s+\w+\s*\([^)]+\)'

        matches = list(re.finditer(fk_pattern, sql, re.IGNORECASE))
        if not matches:
            return sql

        if self.warn_on_fallback:
            for match in matches:
                warnings.warn(
                    f"FOREIGN KEY detected: {match.group(0)}. "
                    "Referential integrity must be enforced in application code. "
                    "Validate references before INSERT/UPDATE operations.",
                    FallbackStrategyWarning,
                    stacklevel=2
                )

        self.fallback_history.append({
            "feature": "FOREIGN_KEY",
            "strategy": FallbackStrategy.APPLICATION_LEVEL,
            "original": [m.group(0) for m in matches],
            "fallback": "Application-level referential integrity checks"
        })

        self.logger.warning(
            "FOREIGN KEY constraints detected",
            extra={
                "count": len(matches),
                "constraints": [m.group(0)[:50] + "..." for m in matches],
                "guidance": "Validate foreign key references in application before writes",
                "recommendations": [
                    "Use transactions to ensure atomicity",
                    "Implement CASCADE DELETE logic in application",
                    "Add indexes on foreign key columns for performance"
                ]
            }
        )

        # Remove FOREIGN KEY constraints
        if not self.strict_mode:
            modified = re.sub(fk_pattern, '', sql, flags=re.IGNORECASE)
            # Clean up extra commas
            modified = re.sub(r',\s*,', ',', modified)
            modified = re.sub(r',\s*\)', ')', modified)
            return modified
        else:
            raise ValueError("FOREIGN KEY constraints not supported in strict mode.")

    def get_fallback_report(self) -> Dict[str, Any]:
        """
        Generate a comprehensive report of all fallbacks applied.

        Returns:
            Dictionary with fallback statistics and details
        """
        fallback_by_feature = {}
        for item in self.fallback_history:
            feature = item["feature"]
            if feature not in fallback_by_feature:
                fallback_by_feature[feature] = []
            fallback_by_feature[feature].append(item)

        return {
            "total_fallbacks": len(self.fallback_history),
            "fallbacks_by_feature": {
                feature: len(items)
                for feature, items in fallback_by_feature.items()
            },
            "detailed_history": self.fallback_history,
            "recommendations": self._generate_recommendations(fallback_by_feature)
        }

    def _generate_recommendations(
        self,
        fallback_by_feature: Dict[str, List[Dict]]
    ) -> List[str]:
        """Generate recommendations based on fallbacks used."""
        recommendations = []

        if "DECIMAL" in fallback_by_feature or "NUMERIC" in fallback_by_feature:
            recommendations.append(
                "For financial calculations, store monetary values as integer cents "
                "(e.g., $12.34 → 1234 cents) using INT8 to avoid floating-point precision loss."
            )

        if "TRIGGER" in fallback_by_feature:
            recommendations.append(
                "Implement trigger logic using ORM hooks, middleware, or application-level "
                "event listeners. Ensure all data entry points apply the same logic."
            )

        if "CHECK_CONSTRAINT" in fallback_by_feature:
            recommendations.append(
                "Add data validation in your application layer. Use ORM validators, "
                "Pydantic models, or custom validation functions before database writes."
            )

        if "FOREIGN_KEY" in fallback_by_feature:
            recommendations.append(
                "Enforce referential integrity in application code. Always validate "
                "foreign key references before INSERT/UPDATE. Use transactions for "
                "multi-table operations to maintain consistency."
            )

        if "AUTOINCREMENT" in fallback_by_feature:
            recommendations.append(
                "Consider using UUID type for primary keys (UUID4 for random, UUID7 for "
                "time-ordered). This eliminates sequence management and works well in "
                "distributed systems."
            )

        return recommendations

    @contextmanager
    def fallback_context(self, feature_name: str):
        """
        Context manager for tracking fallback operations.

        Example:
            with handler.fallback_context("DECIMAL"):
                # Perform operation with fallback
                result = process_decimal_column()
        """
        self.logger.debug(f"Entering fallback context: {feature_name}")
        try:
            yield
        finally:
            self.logger.debug(f"Exiting fallback context: {feature_name}")

    def register_custom_handler(
        self,
        feature_name: str,
        handler: Callable[[str], str]
    ):
        """
        Register a custom fallback handler for a feature.

        Args:
            feature_name: Name of the feature
            handler: Callable that takes SQL string and returns modified SQL
        """
        self.custom_handlers[feature_name.upper()] = handler
        self.logger.info(f"Registered custom handler for {feature_name}")


# Singleton instance for convenience
_default_handler: Optional[HeliosDBFeatureHandler] = None


def get_feature_handler(
    enable_fallbacks: bool = True,
    warn_on_fallback: bool = True,
    strict_mode: bool = False
) -> HeliosDBFeatureHandler:
    """
    Get or create the default feature handler instance.

    Args:
        enable_fallbacks: Enable automatic fallback application
        warn_on_fallback: Emit warnings when fallbacks are used
        strict_mode: Reject unsupported features instead of falling back

    Returns:
        HeliosDBFeatureHandler instance
    """
    global _default_handler
    if _default_handler is None:
        _default_handler = HeliosDBFeatureHandler(
            enable_fallbacks=enable_fallbacks,
            warn_on_fallback=warn_on_fallback,
            strict_mode=strict_mode
        )
    return _default_handler


# Convenience functions
def process_sql(sql: str) -> str:
    """Process SQL with default handler."""
    return get_feature_handler().process_sql(sql)


def is_feature_supported(feature_name: str) -> bool:
    """Check if feature is supported."""
    return get_feature_handler().is_feature_supported(feature_name)


def get_fallback_report() -> Dict[str, Any]:
    """Get fallback report from default handler."""
    return get_feature_handler().get_fallback_report()


if __name__ == "__main__":
    # Example usage and testing
    handler = HeliosDBFeatureHandler(enable_fallbacks=True, warn_on_fallback=True)

    # Test DECIMAL conversion
    sql1 = "CREATE TABLE accounts (id SERIAL, balance DECIMAL(10, 2))"
    print("Original:", sql1)
    print("Processed:", handler.process_sql(sql1))
    print()

    # Test CHECK constraint
    sql2 = "CREATE TABLE users (age INT CHECK (age >= 18))"
    print("Original:", sql2)
    print("Processed:", handler.process_sql(sql2))
    print()

    # Test FOREIGN KEY
    sql3 = "CREATE TABLE orders (id INT, user_id INT, FOREIGN KEY (user_id) REFERENCES users(id))"
    print("Original:", sql3)
    print("Processed:", handler.process_sql(sql3))
    print()

    # Print report
    report = handler.get_fallback_report()
    print("Fallback Report:")
    print(f"Total fallbacks: {report['total_fallbacks']}")
    print("By feature:", report['fallbacks_by_feature'])
    print("\nRecommendations:")
    for rec in report['recommendations']:
        print(f"  - {rec}")
