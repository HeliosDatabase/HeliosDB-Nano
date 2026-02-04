#!/usr/bin/env python3
"""
Unit tests for HeliosDB SQLite Compatibility Checker

Usage:
    pytest test_compatibility_checker.py -v
    pytest test_compatibility_checker.py --cov
"""

import sys
import os
from pathlib import Path

# Add parent directory to path
sys.path.insert(0, str(Path(__file__).parent.parent))

import pytest
from HELIOSDB_SQLITE_COMPATIBILITY_CHECKER import (
    HeliosDBCompatibilityChecker,
    PythonSQLiteAnalyzer,
    SQLSchemaAnalyzer,
    Severity,
    CompatibilityIssue,
    CompatibilityReport,
)
from HELIOSDB_SQLITE_INCOMPATIBILITY_REPORT import (
    ReportPrioritizer,
    HTMLReportGenerator,
    MarkdownReportGenerator,
    ConsoleReportPrinter,
)


class TestPythonSQLiteAnalyzer:
    """Test Python code analysis"""

    def test_detect_sqlite3_import(self):
        """Should detect sqlite3 import"""
        code = "import sqlite3\nconn = sqlite3.connect('db.db')"
        analyzer = PythonSQLiteAnalyzer('test.py', code)
        issues = analyzer.analyze()

        # Should detect import and connect call
        assert len(issues) >= 2
        assert any('import' in i.feature.lower() for i in issues)
        assert any('connect' in i.feature.lower() for i in issues)

    def test_detect_question_mark_placeholders(self):
        """Should detect ? placeholders"""
        code = '''
import sqlite3
conn = sqlite3.connect('db.db')
cursor = conn.cursor()
cursor.execute("SELECT * FROM users WHERE id = ?", (1,))
'''
        analyzer = PythonSQLiteAnalyzer('test.py', code)
        issues = analyzer.analyze()

        placeholder_issues = [i for i in issues if '?' in i.feature]
        assert len(placeholder_issues) > 0
        assert any(i.severity == Severity.CRITICAL for i in placeholder_issues)

    def test_no_issues_for_clean_code(self):
        """Should not flag non-sqlite code"""
        code = '''
import os
import json

def process_data(data):
    return json.dumps(data)
'''
        analyzer = PythonSQLiteAnalyzer('test.py', code)
        issues = analyzer.analyze()

        # Should have no sqlite-related issues
        assert len(issues) == 0

    def test_multiple_sqlite_calls(self):
        """Should detect multiple sqlite3 API calls"""
        code = '''
import sqlite3

conn = sqlite3.connect('db.db')
cursor = conn.cursor()
cursor.execute("SELECT * FROM users WHERE id = ?", (1,))
cursor.execute("INSERT INTO posts VALUES (?, ?)", ('title', 'content'))
cursor.close()
conn.close()
'''
        analyzer = PythonSQLiteAnalyzer('test.py', code)
        issues = analyzer.analyze()

        # Should detect connect and multiple execute calls with placeholders
        assert len(issues) >= 3


class TestSQLSchemaAnalyzer:
    """Test SQL schema analysis"""

    def test_detect_autoincrement(self):
        """Should detect AUTOINCREMENT keyword"""
        sql = '''
CREATE TABLE users (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    name TEXT
);
'''
        analyzer = SQLSchemaAnalyzer('test.sql', sql)
        issues = analyzer.analyze()

        autoincrement_issues = [i for i in issues if 'AUTOINCREMENT' in i.feature]
        assert len(autoincrement_issues) > 0
        assert autoincrement_issues[0].severity == Severity.CRITICAL

    def test_detect_attach_database(self):
        """Should detect ATTACH DATABASE statement"""
        sql = "ATTACH DATABASE 'other.db' AS other;"
        analyzer = SQLSchemaAnalyzer('test.sql', sql)
        issues = analyzer.analyze()

        attach_issues = [i for i in issues if 'ATTACH' in i.feature]
        assert len(attach_issues) > 0
        assert attach_issues[0].severity == Severity.CRITICAL

    def test_detect_pragma_statements(self):
        """Should detect PRAGMA statements"""
        sql = '''
PRAGMA foreign_keys = ON;
PRAGMA journal_mode = WAL;
'''
        analyzer = SQLSchemaAnalyzer('test.sql', sql)
        issues = analyzer.analyze()

        pragma_issues = [i for i in issues if 'PRAGMA' in i.feature]
        assert len(pragma_issues) > 0

    def test_detect_blob_type(self):
        """Should detect BLOB data type"""
        sql = '''
CREATE TABLE files (
    id INTEGER PRIMARY KEY,
    content BLOB
);
'''
        analyzer = SQLSchemaAnalyzer('test.sql', sql)
        issues = analyzer.analyze()

        blob_issues = [i for i in issues if 'BLOB' in i.feature]
        assert len(blob_issues) > 0
        assert blob_issues[0].severity == Severity.WARNING

    def test_detect_without_rowid(self):
        """Should detect WITHOUT ROWID tables"""
        sql = '''
CREATE TABLE settings (
    key TEXT PRIMARY KEY,
    value TEXT
) WITHOUT ROWID;
'''
        analyzer = SQLSchemaAnalyzer('test.sql', sql)
        issues = analyzer.analyze()

        rowid_issues = [i for i in issues if 'WITHOUT ROWID' in i.feature or 'ROWID' in i.feature]
        assert len(rowid_issues) > 0

    def test_detect_triggers(self):
        """Should detect trigger usage"""
        sql = '''
CREATE TRIGGER update_timestamp
AFTER UPDATE ON users
FOR EACH ROW
BEGIN
    UPDATE users SET updated_at = CURRENT_TIMESTAMP WHERE id = NEW.id;
END;
'''
        analyzer = SQLSchemaAnalyzer('test.sql', sql)
        issues = analyzer.analyze()

        trigger_issues = [i for i in issues if 'TRIGGER' in i.feature]
        assert len(trigger_issues) > 0


class TestCompatibilityReport:
    """Test report generation and scoring"""

    def test_empty_report(self):
        """Empty report should have 100% score"""
        report = CompatibilityReport()
        report.calculate_score()

        assert report.compatibility_score == 100.0
        assert report.critical_issues == 0

    def test_add_critical_issue(self):
        """Adding critical issue should lower score"""
        report = CompatibilityReport()

        issue = CompatibilityIssue(
            severity=Severity.CRITICAL,
            category='schema',
            feature='AUTOINCREMENT',
            message='Test',
            file_path='test.sql',
            line_number=1,
            code_snippet='',
            suggestion='Fix it',
            confidence=1.0
        )

        report.add_issue(issue)
        report.calculate_score()

        assert report.critical_issues == 1
        assert report.compatibility_score < 100.0

    def test_score_calculation(self):
        """Test score calculation with multiple issues"""
        report = CompatibilityReport()

        # Add 2 critical, 3 warnings, 5 info
        for _ in range(2):
            report.add_issue(CompatibilityIssue(
                severity=Severity.CRITICAL,
                category='schema',
                feature='test',
                message='test',
                file_path='test.sql',
                line_number=1,
                code_snippet='',
                suggestion='fix',
                confidence=1.0
            ))

        for _ in range(3):
            report.add_issue(CompatibilityIssue(
                severity=Severity.WARNING,
                category='schema',
                feature='test',
                message='test',
                file_path='test.sql',
                line_number=1,
                code_snippet='',
                suggestion='fix',
                confidence=1.0
            ))

        for _ in range(5):
            report.add_issue(CompatibilityIssue(
                severity=Severity.INFO,
                category='schema',
                feature='test',
                message='test',
                file_path='test.sql',
                line_number=1,
                code_snippet='',
                suggestion='fix',
                confidence=1.0
            ))

        report.calculate_score()

        assert report.critical_issues == 2
        assert report.warnings == 3
        assert report.info_items == 5
        # Score should be: 100 - (2*10 + 3*3 + 5*0.5) = 100 - 31.5 = 68.5
        assert report.compatibility_score == pytest.approx(68.5, 0.1)


class TestReportPrioritizer:
    """Test issue prioritization"""

    def test_critical_higher_than_warning(self):
        """Critical issues should have higher priority than warnings"""
        critical = CompatibilityIssue(
            severity=Severity.CRITICAL,
            category='api',
            feature='test',
            message='test',
            file_path='test.py',
            line_number=1,
            code_snippet='',
            suggestion='fix',
            confidence=1.0
        )

        warning = CompatibilityIssue(
            severity=Severity.WARNING,
            category='api',
            feature='test',
            message='test',
            file_path='test.py',
            line_number=1,
            code_snippet='',
            suggestion='fix',
            confidence=1.0
        )

        critical_priority = ReportPrioritizer.calculate_priority(critical)
        warning_priority = ReportPrioritizer.calculate_priority(warning)

        assert critical_priority > warning_priority

    def test_rank_issues(self):
        """Should rank issues by priority"""
        issues = [
            CompatibilityIssue(
                severity=Severity.INFO,
                category='schema',
                feature='test',
                message='test',
                file_path='test.sql',
                line_number=1,
                code_snippet='',
                suggestion='fix',
                confidence=1.0
            ),
            CompatibilityIssue(
                severity=Severity.CRITICAL,
                category='api',
                feature='test',
                message='test',
                file_path='test.py',
                line_number=1,
                code_snippet='',
                suggestion='fix',
                confidence=1.0
            ),
            CompatibilityIssue(
                severity=Severity.WARNING,
                category='schema',
                feature='test',
                message='test',
                file_path='test.sql',
                line_number=1,
                code_snippet='',
                suggestion='fix',
                confidence=1.0
            ),
        ]

        ranked = ReportPrioritizer.rank_issues(issues)

        # First should be critical
        assert ranked[0].severity == Severity.CRITICAL
        # Last should be info
        assert ranked[-1].severity == Severity.INFO


class TestHeliosDBCompatibilityChecker:
    """Test main checker"""

    def test_check_example_files(self):
        """Test against example files"""
        # Get test examples directory
        test_dir = Path(__file__).parent

        if not (test_dir / 'example_sqlite_code.py').exists():
            pytest.skip("Example files not found")

        checker = HeliosDBCompatibilityChecker([str(test_dir)])
        report = checker.check()

        # Should find issues in example files
        assert report.total_files >= 2  # At least .py and .sql examples
        assert len(report.issues) > 0
        assert report.critical_issues > 0

    def test_report_to_dict(self):
        """Test report JSON serialization"""
        report = CompatibilityReport()
        report.total_files = 5
        report.python_files = 3
        report.sql_files = 2

        report_dict = report.to_dict()

        assert report_dict['total_files'] == 5
        assert report_dict['python_files'] == 3
        assert report_dict['sql_files'] == 2
        assert 'compatibility_score' in report_dict


def test_integration_with_real_examples():
    """Integration test with real example files"""
    test_dir = Path(__file__).parent

    if not (test_dir / 'example_sqlite_code.py').exists():
        pytest.skip("Example files not found")

    # Run full check
    checker = HeliosDBCompatibilityChecker([str(test_dir)])
    report = checker.check()

    # Validate report
    assert report.compatibility_score >= 0
    assert report.compatibility_score <= 100
    assert report.total_files > 0

    # Should detect multiple issues
    assert len(report.issues) >= 5

    # Should have critical issues from AUTOINCREMENT and ? placeholders
    assert report.critical_issues >= 2

    # Generate HTML report (just test it doesn't crash)
    html_gen = HTMLReportGenerator(report)
    # Would generate to temp file in real test
    # html_gen.generate('/tmp/test_report.html')

    # Generate console output
    console = ConsoleReportPrinter(report)
    # console.print_summary()  # Uncomment to see output


if __name__ == '__main__':
    pytest.main([__file__, '-v', '--tb=short'])
