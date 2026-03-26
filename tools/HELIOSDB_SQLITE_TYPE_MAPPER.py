#!/usr/bin/env python3
"""
HeliosDB Nano SQLite Type Mapper
Bidirectional type conversion between SQLite and HeliosDB Nano data types.

Handles type affinity, DECIMAL → FLOAT fallback with warnings,
BLOB/binary handling, date/time conversions, and custom types.

Author: HeliosDB Team
Version: 1.0.0
License: MIT
"""

import re
import logging
from typing import Tuple, Optional, Any, Dict
from dataclasses import dataclass
from enum import Enum


logger = logging.getLogger('heliosdb.type_mapper')


class TypeAffinity(Enum):
    """SQLite type affinity categories."""
    INTEGER = "INTEGER"
    TEXT = "TEXT"
    BLOB = "BLOB"
    REAL = "REAL"
    NUMERIC = "NUMERIC"


@dataclass
class ConversionWarning:
    """Warning generated during type conversion."""
    message: str
    context: str
    severity: str = "WARNING"  # WARNING, INFO, ERROR

    def __str__(self) -> str:
        return f"[{self.severity}] {self.context}: {self.message}"


class TypeMapper:
    """
    Bidirectional type mapper between SQLite and HeliosDB Nano.

    SQLite Type System:
    - Uses type affinity (INTEGER, TEXT, BLOB, REAL, NUMERIC)
    - Very flexible - any type name is valid
    - Type determined by affinity rules

    HeliosDB Nano Type System (PostgreSQL-compatible):
    - Boolean, Int2, Int4, Int8
    - Float4, Float8, Numeric
    - Varchar, Text, Char
    - Bytea, Date, Time, Timestamp, Timestamptz
    - Interval, Uuid, Json, Jsonb
    - Array, Vector
    """

    # SQLite → HeliosDB type mappings
    SQLITE_TO_HELIOSDB_MAP = {
        # Integer types
        'INTEGER': 'INT8',
        'INT': 'INT4',
        'TINYINT': 'INT2',
        'SMALLINT': 'INT2',
        'MEDIUMINT': 'INT4',
        'BIGINT': 'INT8',
        'UNSIGNED BIG INT': 'INT8',
        'INT2': 'INT2',
        'INT8': 'INT8',

        # Text types
        'CHARACTER': 'TEXT',
        'VARCHAR': 'VARCHAR',
        'VARYING CHARACTER': 'VARCHAR',
        'NCHAR': 'TEXT',
        'NATIVE CHARACTER': 'TEXT',
        'NVARCHAR': 'VARCHAR',
        'TEXT': 'TEXT',
        'CLOB': 'TEXT',

        # Real/Float types
        'REAL': 'FLOAT8',
        'DOUBLE': 'FLOAT8',
        'DOUBLE PRECISION': 'FLOAT8',
        'FLOAT': 'FLOAT4',

        # Numeric types
        'NUMERIC': 'NUMERIC',
        'DECIMAL': 'FLOAT8',  # Fallback - will generate warning
        'BOOLEAN': 'BOOLEAN',
        'DATE': 'DATE',
        'DATETIME': 'TIMESTAMP',
        'TIMESTAMP': 'TIMESTAMP',

        # Binary types
        'BLOB': 'BYTEA',
        'BINARY': 'BYTEA',
        'VARBINARY': 'BYTEA',

        # Special types
        'UUID': 'UUID',
        'JSON': 'JSONB',
    }

    # HeliosDB → SQLite type mappings (for export)
    HELIOSDB_TO_SQLITE_MAP = {
        # Boolean
        'BOOLEAN': 'INTEGER',  # SQLite stores as 0/1

        # Integer types
        'INT2': 'SMALLINT',
        'INT4': 'INTEGER',
        'INT8': 'BIGINT',

        # Float types
        'FLOAT4': 'REAL',
        'FLOAT8': 'REAL',
        'NUMERIC': 'NUMERIC',

        # String types
        'VARCHAR': 'TEXT',
        'TEXT': 'TEXT',
        'CHAR': 'TEXT',

        # Binary types
        'BYTEA': 'BLOB',

        # Date/Time types
        'DATE': 'TEXT',  # SQLite stores as ISO8601 string
        'TIME': 'TEXT',
        'TIMESTAMP': 'TEXT',
        'TIMESTAMPTZ': 'TEXT',
        'INTERVAL': 'TEXT',

        # Special types
        'UUID': 'TEXT',  # SQLite stores as string
        'JSON': 'TEXT',
        'JSONB': 'TEXT',

        # Array and Vector (stored as JSON in SQLite)
        'ARRAY': 'TEXT',
        'VECTOR': 'TEXT',  # Store as JSON array
    }

    def __init__(self):
        """Initialize type mapper."""
        self.warnings: list[ConversionWarning] = []

    def sqlite_to_heliosdb(
        self,
        sqlite_type: str
    ) -> Tuple[str, Optional[ConversionWarning]]:
        """
        Convert SQLite type to HeliosDB Nano type.

        Args:
            sqlite_type: SQLite type name (e.g., 'VARCHAR(100)', 'INTEGER')

        Returns:
            Tuple of (heliosdb_type, warning)
        """
        # Normalize type name
        normalized = sqlite_type.upper().strip()

        # Handle types with parameters (e.g., VARCHAR(100), DECIMAL(10,2))
        match = re.match(r'^(\w+(?:\s+\w+)*)\s*\(([^)]+)\)', normalized)
        if match:
            base_type = match.group(1)
            params = match.group(2)

            # Handle VARCHAR(n) -> VARCHAR(n)
            if base_type == 'VARCHAR':
                return f'VARCHAR({params})', None

            # Handle CHAR(n) -> CHAR(n)
            if base_type in ('CHARACTER', 'CHAR'):
                return f'CHAR({params})', None

            # Handle DECIMAL(p,s) -> FLOAT8 with warning
            if base_type == 'DECIMAL':
                warning = ConversionWarning(
                    message=f"DECIMAL({params}) converted to FLOAT8 - precision may be lost",
                    context=f"Type: {sqlite_type}",
                    severity="WARNING"
                )
                return 'FLOAT8', warning

            # For other parameterized types, use base type
            normalized = base_type

        # Direct mapping lookup
        if normalized in self.SQLITE_TO_HELIOSDB_MAP:
            helios_type = self.SQLITE_TO_HELIOSDB_MAP[normalized]

            # Generate warning for DECIMAL → FLOAT8 conversion
            if normalized == 'DECIMAL':
                warning = ConversionWarning(
                    message="DECIMAL converted to FLOAT8 - precision may be lost",
                    context=f"Type: {sqlite_type}",
                    severity="WARNING"
                )
                return helios_type, warning

            return helios_type, None

        # Determine type affinity for unknown types
        affinity = self._determine_affinity(normalized)

        # Map by affinity
        affinity_map = {
            TypeAffinity.INTEGER: 'INT8',
            TypeAffinity.TEXT: 'TEXT',
            TypeAffinity.BLOB: 'BYTEA',
            TypeAffinity.REAL: 'FLOAT8',
            TypeAffinity.NUMERIC: 'NUMERIC',
        }

        helios_type = affinity_map[affinity]

        warning = ConversionWarning(
            message=f"Unknown SQLite type '{sqlite_type}' mapped to {helios_type} via {affinity.value} affinity",
            context=f"Type: {sqlite_type}",
            severity="INFO"
        )

        return helios_type, warning

    def heliosdb_to_sqlite(
        self,
        heliosdb_type: str
    ) -> Tuple[str, Optional[ConversionWarning]]:
        """
        Convert HeliosDB Nano type to SQLite type (for export).

        Args:
            heliosdb_type: HeliosDB type name (e.g., 'INT4', 'VARCHAR(100)')

        Returns:
            Tuple of (sqlite_type, warning)
        """
        # Normalize type name
        normalized = heliosdb_type.upper().strip()

        # Handle types with parameters
        match = re.match(r'^(\w+)\s*\(([^)]+)\)', normalized)
        if match:
            base_type = match.group(1)
            params = match.group(2)

            # VARCHAR(n) -> TEXT (SQLite doesn't enforce length)
            if base_type == 'VARCHAR':
                warning = ConversionWarning(
                    message=f"VARCHAR({params}) length constraint not enforced in SQLite",
                    context=f"Type: {heliosdb_type}",
                    severity="INFO"
                )
                return 'TEXT', warning

            # CHAR(n) -> TEXT
            if base_type == 'CHAR':
                warning = ConversionWarning(
                    message=f"CHAR({params}) converted to TEXT - padding not enforced",
                    context=f"Type: {heliosdb_type}",
                    severity="INFO"
                )
                return 'TEXT', warning

            # VECTOR(n) -> TEXT (store as JSON)
            if base_type == 'VECTOR':
                warning = ConversionWarning(
                    message=f"VECTOR({params}) stored as JSON TEXT in SQLite",
                    context=f"Type: {heliosdb_type}",
                    severity="INFO"
                )
                return 'TEXT', warning

            # ARRAY -> TEXT (store as JSON)
            if base_type == 'ARRAY':
                warning = ConversionWarning(
                    message="ARRAY stored as JSON TEXT in SQLite",
                    context=f"Type: {heliosdb_type}",
                    severity="INFO"
                )
                return 'TEXT', warning

            # Use base type for other parameterized types
            normalized = base_type

        # Direct mapping lookup
        if normalized in self.HELIOSDB_TO_SQLITE_MAP:
            sqlite_type = self.HELIOSDB_TO_SQLITE_MAP[normalized]

            # Generate warnings for lossy conversions
            warning = None

            if normalized in ('DATE', 'TIME', 'TIMESTAMP', 'TIMESTAMPTZ'):
                warning = ConversionWarning(
                    message=f"{normalized} stored as TEXT in SQLite (ISO8601 format)",
                    context=f"Type: {heliosdb_type}",
                    severity="INFO"
                )

            elif normalized == 'UUID':
                warning = ConversionWarning(
                    message="UUID stored as TEXT in SQLite",
                    context=f"Type: {heliosdb_type}",
                    severity="INFO"
                )

            elif normalized in ('JSON', 'JSONB'):
                warning = ConversionWarning(
                    message=f"{normalized} stored as TEXT in SQLite",
                    context=f"Type: {heliosdb_type}",
                    severity="INFO"
                )

            elif normalized == 'BOOLEAN':
                warning = ConversionWarning(
                    message="BOOLEAN stored as INTEGER in SQLite (0/1)",
                    context=f"Type: {heliosdb_type}",
                    severity="INFO"
                )

            return sqlite_type, warning

        # Unknown type - default to TEXT
        warning = ConversionWarning(
            message=f"Unknown HeliosDB type '{heliosdb_type}' mapped to TEXT",
            context=f"Type: {heliosdb_type}",
            severity="WARNING"
        )

        return 'TEXT', warning

    def _determine_affinity(self, type_name: str) -> TypeAffinity:
        """
        Determine SQLite type affinity for a type name.

        SQLite Affinity Rules:
        1. If type contains "INT" → INTEGER affinity
        2. If type contains "CHAR", "CLOB", or "TEXT" → TEXT affinity
        3. If type contains "BLOB" or no type specified → BLOB affinity
        4. If type contains "REAL", "FLOA", or "DOUB" → REAL affinity
        5. Otherwise → NUMERIC affinity

        Args:
            type_name: Normalized type name

        Returns:
            Type affinity
        """
        type_upper = type_name.upper()

        if 'INT' in type_upper:
            return TypeAffinity.INTEGER

        if any(x in type_upper for x in ('CHAR', 'CLOB', 'TEXT')):
            return TypeAffinity.TEXT

        if 'BLOB' in type_upper:
            return TypeAffinity.BLOB

        if any(x in type_upper for x in ('REAL', 'FLOA', 'DOUB')):
            return TypeAffinity.REAL

        # Default to NUMERIC
        return TypeAffinity.NUMERIC


class ValueConverter:
    """
    Convert data values between SQLite and HeliosDB formats.

    Handles special cases like:
    - BLOB/binary data encoding
    - Date/time parsing and formatting
    - JSON serialization
    - Vector/array encoding
    - Boolean conversions
    """

    @staticmethod
    def sqlite_value_to_heliosdb(
        value: Any,
        sqlite_type: str,
        heliosdb_type: str
    ) -> Any:
        """
        Convert a SQLite value to HeliosDB format.

        Args:
            value: SQLite value
            sqlite_type: Original SQLite type
            heliosdb_type: Target HeliosDB type

        Returns:
            Converted value
        """
        if value is None:
            return None

        heliosdb_base = heliosdb_type.split('(')[0].upper()

        # Boolean conversion
        if heliosdb_base == 'BOOLEAN':
            if isinstance(value, int):
                return bool(value)
            if isinstance(value, str):
                return value.lower() in ('true', '1', 'yes', 't')
            return bool(value)

        # Integer conversions
        if heliosdb_base in ('INT2', 'INT4', 'INT8'):
            return int(value)

        # Float conversions
        if heliosdb_base in ('FLOAT4', 'FLOAT8', 'NUMERIC'):
            return float(value)

        # Text conversions
        if heliosdb_base in ('VARCHAR', 'TEXT', 'CHAR'):
            return str(value)

        # Binary conversions
        if heliosdb_base == 'BYTEA':
            if isinstance(value, bytes):
                return value
            if isinstance(value, str):
                # Try to decode hex string
                if value.startswith('\\x'):
                    return bytes.fromhex(value[2:])
                return value.encode('utf-8')
            return bytes(value)

        # Date/Time conversions
        if heliosdb_base in ('DATE', 'TIME', 'TIMESTAMP', 'TIMESTAMPTZ'):
            # SQLite stores as TEXT or INTEGER (Unix timestamp)
            if isinstance(value, str):
                return value  # Already in ISO8601 format
            if isinstance(value, int):
                # Convert Unix timestamp to ISO8601
                from datetime import datetime
                dt = datetime.fromtimestamp(value)
                return dt.isoformat()
            return str(value)

        # UUID conversion
        if heliosdb_base == 'UUID':
            return str(value)

        # JSON conversion
        if heliosdb_base in ('JSON', 'JSONB'):
            if isinstance(value, str):
                # Validate JSON
                import json
                try:
                    json.loads(value)
                    return value
                except json.JSONDecodeError:
                    # Not valid JSON, wrap in quotes
                    return json.dumps(value)
            import json
            return json.dumps(value)

        # Vector conversion
        if heliosdb_base == 'VECTOR':
            if isinstance(value, str):
                import json
                return json.loads(value)  # Parse JSON array
            return list(value)

        # Default - return as-is
        return value

    @staticmethod
    def heliosdb_value_to_sqlite(
        value: Any,
        heliosdb_type: str,
        sqlite_type: str
    ) -> Any:
        """
        Convert a HeliosDB value to SQLite format.

        Args:
            value: HeliosDB value
            heliosdb_type: Original HeliosDB type
            sqlite_type: Target SQLite type

        Returns:
            Converted value
        """
        if value is None:
            return None

        heliosdb_base = heliosdb_type.split('(')[0].upper()
        sqlite_base = sqlite_type.upper()

        # Boolean to INTEGER conversion
        if heliosdb_base == 'BOOLEAN':
            return 1 if value else 0

        # Vector to TEXT (JSON) conversion
        if heliosdb_base == 'VECTOR':
            import json
            return json.dumps(list(value))

        # Array to TEXT (JSON) conversion
        if heliosdb_base == 'ARRAY':
            import json
            return json.dumps(value)

        # UUID to TEXT conversion
        if heliosdb_base == 'UUID':
            return str(value)

        # JSON/JSONB to TEXT conversion
        if heliosdb_base in ('JSON', 'JSONB'):
            if isinstance(value, str):
                return value
            import json
            return json.dumps(value)

        # Timestamp to TEXT conversion
        if heliosdb_base in ('DATE', 'TIME', 'TIMESTAMP', 'TIMESTAMPTZ'):
            if hasattr(value, 'isoformat'):
                return value.isoformat()
            return str(value)

        # Binary to BLOB conversion
        if heliosdb_base == 'BYTEA':
            if isinstance(value, bytes):
                return value
            if isinstance(value, str):
                return value.encode('utf-8')
            return bytes(value)

        # Numeric conversions
        if heliosdb_base in ('INT2', 'INT4', 'INT8'):
            return int(value)

        if heliosdb_base in ('FLOAT4', 'FLOAT8', 'NUMERIC'):
            return float(value)

        # Default - return as-is
        return value


class CustomTypeRegistry:
    """
    Registry for custom type conversions.

    Allows users to register custom type mappings for domain-specific types.
    """

    def __init__(self):
        """Initialize custom type registry."""
        self.custom_mappings: Dict[str, Tuple[str, callable]] = {}

    def register_custom_type(
        self,
        sqlite_type: str,
        heliosdb_type: str,
        converter: callable
    ):
        """
        Register a custom type mapping.

        Args:
            sqlite_type: SQLite type name
            heliosdb_type: HeliosDB type name
            converter: Function to convert values (value, from_type, to_type) -> converted_value
        """
        self.custom_mappings[sqlite_type.upper()] = (heliosdb_type, converter)
        logger.info(f"Registered custom type: {sqlite_type} → {heliosdb_type}")

    def get_custom_mapping(
        self,
        sqlite_type: str
    ) -> Optional[Tuple[str, callable]]:
        """
        Get custom mapping for a SQLite type.

        Args:
            sqlite_type: SQLite type name

        Returns:
            Tuple of (heliosdb_type, converter) or None
        """
        return self.custom_mappings.get(sqlite_type.upper())


# Global custom type registry
custom_type_registry = CustomTypeRegistry()


def main():
    """Test type mapper."""
    mapper = TypeMapper()

    print("SQLite → HeliosDB Type Mappings:")
    print("=" * 60)

    test_types = [
        'INTEGER', 'TEXT', 'REAL', 'BLOB',
        'VARCHAR(100)', 'DECIMAL(10,2)', 'DATETIME',
        'BOOLEAN', 'UUID', 'JSON',
        'UNKNOWN_TYPE',
    ]

    for sqlite_type in test_types:
        helios_type, warning = mapper.sqlite_to_heliosdb(sqlite_type)
        print(f"{sqlite_type:20} → {helios_type:15}", end='')
        if warning:
            print(f" [{warning.severity}] {warning.message}")
        else:
            print()

    print("\n" + "=" * 60)
    print("HeliosDB → SQLite Type Mappings:")
    print("=" * 60)

    test_helios_types = [
        'INT4', 'INT8', 'FLOAT8', 'TEXT', 'BYTEA',
        'VARCHAR(100)', 'TIMESTAMP', 'BOOLEAN', 'UUID',
        'JSONB', 'VECTOR(768)', 'ARRAY',
    ]

    for helios_type in test_helios_types:
        sqlite_type, warning = mapper.heliosdb_to_sqlite(helios_type)
        print(f"{helios_type:20} → {sqlite_type:15}", end='')
        if warning:
            print(f" [{warning.severity}] {warning.message}")
        else:
            print()


if __name__ == '__main__':
    main()
