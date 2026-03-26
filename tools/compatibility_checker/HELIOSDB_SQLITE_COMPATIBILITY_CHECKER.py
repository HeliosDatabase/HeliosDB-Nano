#!/usr/bin/env python3
"""
HeliosDB SQLite Compatibility Checker
======================================

A comprehensive static code and schema analyzer that detects SQLite-specific
patterns and incompatibilities BEFORE migrating to HeliosDB Nano.

Features:
- Static code analysis for sqlite3 module usage
- SQL schema parsing and validation
- Feature compatibility matrix checking
- Confidence scoring (0-100%)
- Detailed incompatibility reporting
- Integration with pytest/CI/CD

Usage:
    # Analyze Python code and SQL files
    python HELIOSDB_SQLITE_COMPATIBILITY_CHECKER.py /path/to/project

    # Analyze specific files
    python HELIOSDB_SQLITE_COMPATIBILITY_CHECKER.py file1.py schema.sql

    # Generate JSON report
    python HELIOSDB_SQLITE_COMPATIBILITY_CHECKER.py /path/to/project --json report.json

    # Integration with pytest
    pytest --sqlite-compat-check

Author: HeliosDB Team
Version: 1.0.0
License: Apache-2.0
"""

import ast
import re
import os
import sys
import json
import argparse
from pathlib import Path
from typing import List, Dict, Tuple, Optional, Set
from dataclasses import dataclass, field, asdict
from enum import Enum
import sqlparse
from sqlparse.sql import Statement, Token, Identifier, Function
from sqlparse.tokens import Keyword, DML


class Severity(Enum):
    """Issue severity levels"""
    CRITICAL = "critical"      # Migration will fail
    WARNING = "warning"        # May cause runtime issues
    INFO = "info"             # Best practice suggestion
    DEPRECATED = "deprecated"  # Feature exists but deprecated


@dataclass
class CompatibilityIssue:
    """Represents a single compatibility issue"""
    severity: Severity
    category: str              # e.g., "schema", "api", "syntax"
    feature: str               # e.g., "AUTOINCREMENT", "sqlite3.connect"
    message: str               # Human-readable description
    file_path: str
    line_number: int
    code_snippet: str          # Problematic code
    suggestion: str            # How to fix
    confidence: float          # 0.0-1.0 confidence in detection
    heliosdb_alternative: Optional[str] = None
    documentation_url: Optional[str] = None

    def to_dict(self) -> dict:
        """Convert to dictionary for JSON serialization"""
        d = asdict(self)
        d['severity'] = self.severity.value
        return d


@dataclass
class CompatibilityReport:
    """Complete compatibility analysis report"""
    total_files: int = 0
    python_files: int = 0
    sql_files: int = 0
    issues: List[CompatibilityIssue] = field(default_factory=list)
    compatibility_score: float = 100.0  # 0-100%
    critical_issues: int = 0
    warnings: int = 0
    info_items: int = 0

    def add_issue(self, issue: CompatibilityIssue):
        """Add issue and update counters"""
        self.issues.append(issue)
        if issue.severity == Severity.CRITICAL:
            self.critical_issues += 1
        elif issue.severity == Severity.WARNING:
            self.warnings += 1
        elif issue.severity == Severity.INFO:
            self.info_items += 1

    def calculate_score(self):
        """Calculate overall compatibility score (0-100%)"""
        if not self.issues:
            self.compatibility_score = 100.0
            return

        # Weighted scoring
        penalty = (
            self.critical_issues * 10.0 +
            self.warnings * 3.0 +
            self.info_items * 0.5
        )

        self.compatibility_score = max(0.0, 100.0 - penalty)

    def to_dict(self) -> dict:
        """Convert to dictionary for JSON serialization"""
        return {
            'total_files': self.total_files,
            'python_files': self.python_files,
            'sql_files': self.sql_files,
            'compatibility_score': round(self.compatibility_score, 2),
            'critical_issues': self.critical_issues,
            'warnings': self.warnings,
            'info_items': self.info_items,
            'issues': [issue.to_dict() for issue in self.issues]
        }


class SQLiteFeatureMatrix:
    """
    Feature compatibility matrix between SQLite and HeliosDB Nano
    Based on migration documentation and PostgreSQL compatibility
    """

    # Schema-level incompatibilities
    SCHEMA_INCOMPATIBILITIES = {
        r'\bAUTOINCREMENT\b': {
            'severity': Severity.CRITICAL,
            'message': 'AUTOINCREMENT not supported',
            'suggestion': 'Use SERIAL or BIGSERIAL instead',
            'heliosdb_alternative': 'SERIAL PRIMARY KEY',
            'confidence': 0.95
        },
        r'\bATTACH\s+DATABASE\b': {
            'severity': Severity.CRITICAL,
            'message': 'ATTACH DATABASE not supported',
            'suggestion': 'Use separate database connections or migrate to single database',
            'heliosdb_alternative': 'Create tables in main database',
            'confidence': 1.0
        },
        r'\bIF\s+NOT\s+EXISTS\b': {
            'severity': Severity.WARNING,
            'message': 'IF NOT EXISTS may not work consistently',
            'suggestion': 'Use explicit migration scripts instead',
            'heliosdb_alternative': 'Migration framework with version tracking',
            'confidence': 0.7
        },
        r'\bWITHOUT\s+ROWID\b': {
            'severity': Severity.WARNING,
            'message': 'WITHOUT ROWID not supported',
            'suggestion': 'Add explicit primary key column',
            'heliosdb_alternative': 'id SERIAL PRIMARY KEY',
            'confidence': 0.9
        },
        r'\bON\s+CONFLICT\s+REPLACE\b': {
            'severity': Severity.WARNING,
            'message': 'ON CONFLICT REPLACE has different semantics',
            'suggestion': 'Use ON CONFLICT DO UPDATE (UPSERT) or explicit logic',
            'heliosdb_alternative': 'ON CONFLICT (id) DO UPDATE SET ...',
            'confidence': 0.85
        },
    }

    # Data type incompatibilities
    TYPE_INCOMPATIBILITIES = {
        r'\bINTEGER\s+PRIMARY\s+KEY\b': {
            'severity': Severity.INFO,
            'message': 'INTEGER PRIMARY KEY should be SERIAL or BIGSERIAL',
            'suggestion': 'Use SERIAL for auto-increment or INT for manual IDs',
            'heliosdb_alternative': 'SERIAL PRIMARY KEY',
            'confidence': 0.8
        },
        r'\bREAL\b': {
            'severity': Severity.INFO,
            'message': 'REAL type should be FLOAT4 or FLOAT8',
            'suggestion': 'Use FLOAT4 for single precision, FLOAT8 for double',
            'heliosdb_alternative': 'FLOAT8',
            'confidence': 0.75
        },
        r'\bBLOB\b': {
            'severity': Severity.WARNING,
            'message': 'BLOB type should be BYTEA (coming soon)',
            'suggestion': 'Wait for BYTEA support or encode as TEXT temporarily',
            'heliosdb_alternative': 'BYTEA',
            'confidence': 0.9
        },
    }

    # Function/feature incompatibilities
    FUNCTION_INCOMPATIBILITIES = {
        r'\bSQLITE_VERSION\b': {
            'severity': Severity.INFO,
            'message': 'SQLITE_VERSION() not available',
            'suggestion': 'Use SELECT version() for HeliosDB version',
            'heliosdb_alternative': 'version()',
            'confidence': 1.0
        },
        r'\blast_insert_rowid\b': {
            'severity': Severity.WARNING,
            'message': 'last_insert_rowid() not directly available',
            'suggestion': 'Use RETURNING clause on INSERT',
            'heliosdb_alternative': 'INSERT ... RETURNING id',
            'confidence': 0.95
        },
        r'\bPRAGMA\b': {
            'severity': Severity.WARNING,
            'message': 'PRAGMA statements not supported',
            'suggestion': 'Use HeliosDB configuration system',
            'heliosdb_alternative': 'Configuration via heliosdb.toml or API',
            'confidence': 0.9
        },
    }


class PythonSQLiteAnalyzer(ast.NodeVisitor):
    """
    AST-based analyzer for Python code using sqlite3 module
    Detects sqlite3-specific patterns and API usage
    """

    def __init__(self, file_path: str, source_code: str):
        self.file_path = file_path
        self.source_code = source_code
        self.source_lines = source_code.split('\n')
        self.issues: List[CompatibilityIssue] = []

        # Track imports
        self.imports_sqlite3 = False
        self.sqlite3_alias = 'sqlite3'

        # Track sqlite3 API usage
        self.connection_methods: List[Tuple[int, str]] = []
        self.cursor_methods: List[Tuple[int, str]] = []

    def visit_Import(self, node: ast.Import):
        """Detect sqlite3 imports"""
        for alias in node.names:
            if alias.name == 'sqlite3':
                self.imports_sqlite3 = True
                self.sqlite3_alias = alias.asname or 'sqlite3'

                # Info: Project uses sqlite3
                self.issues.append(CompatibilityIssue(
                    severity=Severity.INFO,
                    category='api',
                    feature='sqlite3 import',
                    message='Code uses sqlite3 module',
                    file_path=self.file_path,
                    line_number=node.lineno,
                    code_snippet=self._get_code_snippet(node.lineno),
                    suggestion='Migrate to HeliosDB EmbeddedDatabase API',
                    confidence=1.0,
                    heliosdb_alternative='from heliosdb_lite import EmbeddedDatabase',
                    documentation_url='https://docs.heliosdb.com/lite/embedded-api'
                ))
        self.generic_visit(node)

    def visit_ImportFrom(self, node: ast.ImportFrom):
        """Detect from sqlite3 import ..."""
        if node.module == 'sqlite3':
            self.imports_sqlite3 = True

            # Check for specific imports
            for alias in node.names:
                if alias.name == 'connect':
                    self.issues.append(CompatibilityIssue(
                        severity=Severity.WARNING,
                        category='api',
                        feature='sqlite3.connect',
                        message='sqlite3.connect() needs migration',
                        file_path=self.file_path,
                        line_number=node.lineno,
                        code_snippet=self._get_code_snippet(node.lineno),
                        suggestion='Use EmbeddedDatabase::new() instead',
                        confidence=1.0,
                        heliosdb_alternative='db = EmbeddedDatabase::new("path.db")',
                    ))
        self.generic_visit(node)

    def visit_Call(self, node: ast.Call):
        """Detect function calls to sqlite3 APIs"""
        if self.imports_sqlite3:
            # Check for connect() calls
            if self._is_sqlite3_call(node, 'connect'):
                self.issues.append(CompatibilityIssue(
                    severity=Severity.CRITICAL,
                    category='api',
                    feature='sqlite3.connect()',
                    message='Connection API incompatible with HeliosDB',
                    file_path=self.file_path,
                    line_number=node.lineno,
                    code_snippet=self._get_code_snippet(node.lineno),
                    suggestion='Replace with HeliosDB EmbeddedDatabase API',
                    confidence=0.95,
                    heliosdb_alternative='EmbeddedDatabase::new("database.db")',
                ))

            # Check for execute() with ? placeholders
            if self._is_method_call(node, 'execute'):
                self._check_placeholder_style(node)

        self.generic_visit(node)

    def _is_sqlite3_call(self, node: ast.Call, method: str) -> bool:
        """Check if call is to sqlite3.method()"""
        if isinstance(node.func, ast.Attribute):
            if isinstance(node.func.value, ast.Name):
                return (node.func.value.id == self.sqlite3_alias and
                       node.func.attr == method)
        return False

    def _is_method_call(self, node: ast.Call, method: str) -> bool:
        """Check if call is a method with given name"""
        return (isinstance(node.func, ast.Attribute) and
                node.func.attr == method)

    def _check_placeholder_style(self, node: ast.Call):
        """Check for SQLite ? placeholders vs PostgreSQL $1 style"""
        # Get SQL string argument
        if node.args:
            arg = node.args[0]
            if isinstance(arg, ast.Constant) and isinstance(arg.value, str):
                sql = arg.value
                if '?' in sql:
                    self.issues.append(CompatibilityIssue(
                        severity=Severity.CRITICAL,
                        category='syntax',
                        feature='? placeholders',
                        message='SQLite ? placeholders not supported',
                        file_path=self.file_path,
                        line_number=node.lineno,
                        code_snippet=self._get_code_snippet(node.lineno),
                        suggestion='Use PostgreSQL-style $1, $2, ... placeholders',
                        confidence=1.0,
                        heliosdb_alternative='Use $1, $2, $3 instead of ?, ?, ?',
                    ))

    def _get_code_snippet(self, line_number: int, context: int = 2) -> str:
        """Get code snippet around line number"""
        start = max(0, line_number - context - 1)
        end = min(len(self.source_lines), line_number + context)
        snippet = '\n'.join(self.source_lines[start:end])
        return snippet

    def analyze(self) -> List[CompatibilityIssue]:
        """Run analysis and return issues"""
        try:
            tree = ast.parse(self.source_code, filename=self.file_path)
            self.visit(tree)
        except SyntaxError as e:
            self.issues.append(CompatibilityIssue(
                severity=Severity.WARNING,
                category='syntax',
                feature='parse error',
                message=f'Failed to parse Python file: {e}',
                file_path=self.file_path,
                line_number=e.lineno or 0,
                code_snippet='',
                suggestion='Fix syntax errors before migration',
                confidence=1.0
            ))
        return self.issues


class SQLSchemaAnalyzer:
    """
    SQL schema analyzer for detecting SQLite-specific syntax and features
    Uses sqlparse for SQL parsing and pattern matching for feature detection
    """

    def __init__(self, file_path: str, sql_content: str):
        self.file_path = file_path
        self.sql_content = sql_content
        self.issues: List[CompatibilityIssue] = []
        self.matrix = SQLiteFeatureMatrix()

    def analyze(self) -> List[CompatibilityIssue]:
        """Run comprehensive SQL schema analysis"""
        # Parse SQL statements
        statements = sqlparse.parse(self.sql_content)

        for stmt in statements:
            stmt_text = str(stmt).strip()
            if not stmt_text:
                continue

            # Get line number (approximate)
            line_num = self._get_line_number(stmt_text)

            # Check against feature matrices
            self._check_schema_patterns(stmt_text, line_num)
            self._check_type_patterns(stmt_text, line_num)
            self._check_function_patterns(stmt_text, line_num)

            # Special checks
            self._check_create_table(stmt, line_num)
            self._check_triggers(stmt, line_num)
            self._check_type_affinity(stmt, line_num)

        return self.issues

    def _check_schema_patterns(self, sql: str, line_num: int):
        """Check schema-level patterns"""
        for pattern, info in self.matrix.SCHEMA_INCOMPATIBILITIES.items():
            if re.search(pattern, sql, re.IGNORECASE):
                self.issues.append(CompatibilityIssue(
                    severity=info['severity'],
                    category='schema',
                    feature=pattern,
                    message=info['message'],
                    file_path=self.file_path,
                    line_number=line_num,
                    code_snippet=sql[:200],
                    suggestion=info['suggestion'],
                    confidence=info['confidence'],
                    heliosdb_alternative=info.get('heliosdb_alternative')
                ))

    def _check_type_patterns(self, sql: str, line_num: int):
        """Check data type patterns"""
        for pattern, info in self.matrix.TYPE_INCOMPATIBILITIES.items():
            if re.search(pattern, sql, re.IGNORECASE):
                self.issues.append(CompatibilityIssue(
                    severity=info['severity'],
                    category='datatype',
                    feature=pattern,
                    message=info['message'],
                    file_path=self.file_path,
                    line_number=line_num,
                    code_snippet=sql[:200],
                    suggestion=info['suggestion'],
                    confidence=info['confidence'],
                    heliosdb_alternative=info.get('heliosdb_alternative')
                ))

    def _check_function_patterns(self, sql: str, line_num: int):
        """Check function/feature patterns"""
        for pattern, info in self.matrix.FUNCTION_INCOMPATIBILITIES.items():
            if re.search(pattern, sql, re.IGNORECASE):
                self.issues.append(CompatibilityIssue(
                    severity=info['severity'],
                    category='function',
                    feature=pattern,
                    message=info['message'],
                    file_path=self.file_path,
                    line_number=line_num,
                    code_snippet=sql[:200],
                    suggestion=info['suggestion'],
                    confidence=info['confidence'],
                    heliosdb_alternative=info.get('heliosdb_alternative')
                ))

    def _check_create_table(self, stmt: Statement, line_num: int):
        """Special handling for CREATE TABLE statements"""
        stmt_text = str(stmt).upper()

        # Check for dynamic typing indicators
        if 'CREATE TABLE' in stmt_text:
            # Warn about type affinity
            if not re.search(r'\b(INT|INTEGER|TEXT|BLOB|REAL|NUMERIC)\b', stmt_text):
                self.issues.append(CompatibilityIssue(
                    severity=Severity.WARNING,
                    category='schema',
                    feature='type affinity',
                    message='Table may rely on SQLite type affinity',
                    file_path=self.file_path,
                    line_number=line_num,
                    code_snippet=str(stmt)[:200],
                    suggestion='Ensure all columns have explicit types',
                    confidence=0.6,
                    heliosdb_alternative='Use explicit INT, TEXT, FLOAT8, etc.'
                ))

    def _check_triggers(self, stmt: Statement, line_num: int):
        """Check for trigger usage"""
        stmt_text = str(stmt).upper()
        if 'CREATE TRIGGER' in stmt_text:
            self.issues.append(CompatibilityIssue(
                severity=Severity.WARNING,
                category='feature',
                feature='TRIGGER',
                message='Triggers not yet supported (Phase 3)',
                file_path=self.file_path,
                line_number=line_num,
                code_snippet=str(stmt)[:200],
                suggestion='Implement trigger logic in application code temporarily',
                confidence=1.0,
                heliosdb_alternative='Planned for Phase 3'
            ))

    def _check_type_affinity(self, stmt: Statement, line_num: int):
        """Check for SQLite type affinity issues"""
        # SQLite allows flexible types; HeliosDB requires strict types
        pass

    def _get_line_number(self, text: str) -> int:
        """Get approximate line number of SQL text in source"""
        index = self.sql_content.find(text)
        if index == -1:
            return 1
        return self.sql_content[:index].count('\n') + 1


class HeliosDBCompatibilityChecker:
    """
    Main compatibility checker orchestrating all analysis
    """

    def __init__(self, paths: List[str], recursive: bool = True):
        self.paths = [Path(p) for p in paths]
        self.recursive = recursive
        self.report = CompatibilityReport()

    def check(self) -> CompatibilityReport:
        """Run comprehensive compatibility check"""
        files = self._collect_files()

        for file_path in files:
            self._analyze_file(file_path)

        self.report.calculate_score()
        return self.report

    def _collect_files(self) -> List[Path]:
        """Collect all Python and SQL files"""
        files = []

        for path in self.paths:
            if path.is_file():
                files.append(path)
            elif path.is_dir():
                if self.recursive:
                    files.extend(path.rglob('*.py'))
                    files.extend(path.rglob('*.sql'))
                else:
                    files.extend(path.glob('*.py'))
                    files.extend(path.glob('*.sql'))

        self.report.total_files = len(files)
        self.report.python_files = len([f for f in files if f.suffix == '.py'])
        self.report.sql_files = len([f for f in files if f.suffix == '.sql'])

        return files

    def _analyze_file(self, file_path: Path):
        """Analyze single file"""
        try:
            content = file_path.read_text(encoding='utf-8')
        except Exception as e:
            print(f"Error reading {file_path}: {e}", file=sys.stderr)
            return

        if file_path.suffix == '.py':
            analyzer = PythonSQLiteAnalyzer(str(file_path), content)
            issues = analyzer.analyze()
        elif file_path.suffix == '.sql':
            analyzer = SQLSchemaAnalyzer(str(file_path), content)
            issues = analyzer.analyze()
        else:
            return

        for issue in issues:
            self.report.add_issue(issue)


def main():
    """Command-line interface"""
    parser = argparse.ArgumentParser(
        description='HeliosDB SQLite Compatibility Checker',
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog=__doc__
    )
    parser.add_argument(
        'paths',
        nargs='+',
        help='Files or directories to analyze'
    )
    parser.add_argument(
        '--json',
        metavar='FILE',
        help='Export report as JSON'
    )
    parser.add_argument(
        '--html',
        metavar='FILE',
        help='Export report as HTML'
    )
    parser.add_argument(
        '--no-recursive',
        action='store_true',
        help='Do not recurse into subdirectories'
    )
    parser.add_argument(
        '--fail-on-critical',
        action='store_true',
        help='Exit with error code if critical issues found'
    )
    parser.add_argument(
        '--min-score',
        type=float,
        default=0,
        help='Minimum compatibility score required (0-100)'
    )

    args = parser.parse_args()

    # Run checker
    checker = HeliosDBCompatibilityChecker(
        args.paths,
        recursive=not args.no_recursive
    )
    report = checker.check()

    # Print summary
    print(f"\n{'='*80}")
    print(f"HeliosDB SQLite Compatibility Check")
    print(f"{'='*80}")
    print(f"Files Analyzed: {report.total_files} ({report.python_files} Python, {report.sql_files} SQL)")
    print(f"Compatibility Score: {report.compatibility_score:.1f}%")
    print(f"Issues Found: {len(report.issues)}")
    print(f"  Critical: {report.critical_issues}")
    print(f"  Warnings: {report.warnings}")
    print(f"  Info: {report.info_items}")
    print(f"{'='*80}\n")

    # Print issues by severity
    for severity in [Severity.CRITICAL, Severity.WARNING, Severity.INFO]:
        issues = [i for i in report.issues if i.severity == severity]
        if issues:
            print(f"\n{severity.value.upper()} ISSUES ({len(issues)}):")
            print("-" * 80)
            for issue in issues[:10]:  # Limit output
                print(f"\n  {issue.file_path}:{issue.line_number}")
                print(f"  {issue.feature}: {issue.message}")
                print(f"  Suggestion: {issue.suggestion}")
                if issue.heliosdb_alternative:
                    print(f"  HeliosDB Alternative: {issue.heliosdb_alternative}")

    # Export reports
    if args.json:
        with open(args.json, 'w') as f:
            json.dump(report.to_dict(), f, indent=2)
        print(f"\nJSON report exported to: {args.json}")

    if args.html:
        # Import HTML report generator
        from HELIOSDB_SQLITE_INCOMPATIBILITY_REPORT import HTMLReportGenerator
        generator = HTMLReportGenerator(report)
        generator.generate(args.html)
        print(f"HTML report exported to: {args.html}")

    # Exit code handling
    exit_code = 0
    if args.fail_on_critical and report.critical_issues > 0:
        exit_code = 1
    if report.compatibility_score < args.min_score:
        exit_code = 2

    sys.exit(exit_code)


if __name__ == '__main__':
    main()
