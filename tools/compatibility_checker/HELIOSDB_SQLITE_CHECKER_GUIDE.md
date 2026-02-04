# HeliosDB SQLite Compatibility Checker Guide

**Version**: 1.0.0
**Last Updated**: 2025-12-08
**Status**: Production Ready

## Overview

The HeliosDB SQLite Compatibility Checker is a production-ready static analysis tool that automatically detects compatibility issues in SQLite code and schemas **BEFORE** migration to HeliosDB-Lite. This guide covers installation, usage, interpretation of results, fixing common issues, and CI/CD integration.

## Table of Contents

1. [Installation](#installation)
2. [Quick Start](#quick-start)
3. [Running Compatibility Checks](#running-compatibility-checks)
4. [Interpreting Results](#interpreting-results)
5. [Fixing Common Issues](#fixing-common-issues)
6. [Integration with CI/CD](#integration-with-cicd)
7. [False Positive Handling](#false-positive-handling)
8. [Advanced Usage](#advanced-usage)
9. [Troubleshooting](#troubleshooting)

---

## Installation

### Prerequisites

- Python 3.7 or higher
- pip package manager

### Install Dependencies

```bash
cd /home/claude/HeliosDB-Lite/tools/compatibility_checker

# Install required packages
pip install sqlparse

# Optional: Install in virtual environment (recommended)
python3 -m venv venv
source venv/bin/activate  # On Windows: venv\Scripts\activate
pip install sqlparse
```

### Verify Installation

```bash
python HELIOSDB_SQLITE_COMPATIBILITY_CHECKER.py --help
```

Expected output:
```
usage: HELIOSDB_SQLITE_COMPATIBILITY_CHECKER.py [-h] [--json FILE] [--html FILE]
                                                 [--no-recursive]
                                                 [--fail-on-critical]
                                                 [--min-score MIN_SCORE]
                                                 paths [paths ...]

HeliosDB SQLite Compatibility Checker
```

---

## Quick Start

### Basic Usage

Analyze a single file:
```bash
python HELIOSDB_SQLITE_COMPATIBILITY_CHECKER.py myapp.py
```

Analyze a directory (recursive):
```bash
python HELIOSDB_SQLITE_COMPATIBILITY_CHECKER.py /path/to/project
```

Analyze multiple files/directories:
```bash
python HELIOSDB_SQLITE_COMPATIBILITY_CHECKER.py app.py schema.sql models/
```

### Generate Reports

JSON report:
```bash
python HELIOSDB_SQLITE_COMPATIBILITY_CHECKER.py /path/to/project --json report.json
```

HTML report (interactive):
```bash
python HELIOSDB_SQLITE_COMPATIBILITY_CHECKER.py /path/to/project --html report.html
```

Both formats:
```bash
python HELIOSDB_SQLITE_COMPATIBILITY_CHECKER.py /path/to/project \
    --json report.json \
    --html report.html
```

### CI/CD Integration

Fail build on critical issues:
```bash
python HELIOSDB_SQLITE_COMPATIBILITY_CHECKER.py /path/to/project \
    --fail-on-critical \
    --json report.json
```

Require minimum score:
```bash
python HELIOSDB_SQLITE_COMPATIBILITY_CHECKER.py /path/to/project \
    --min-score 80 \
    --fail-on-critical
```

---

## Running Compatibility Checks

### Command-Line Options

| Option | Description | Example |
|--------|-------------|---------|
| `paths` | Files or directories to analyze (required) | `app.py schema.sql` |
| `--json FILE` | Export report as JSON | `--json report.json` |
| `--html FILE` | Export interactive HTML report | `--html report.html` |
| `--no-recursive` | Don't recurse into subdirectories | `--no-recursive` |
| `--fail-on-critical` | Exit with error if critical issues found | `--fail-on-critical` |
| `--min-score N` | Minimum compatibility score (0-100) | `--min-score 75` |

### Exit Codes

- `0`: Success (no critical issues or score above minimum)
- `1`: Critical issues found (with `--fail-on-critical`)
- `2`: Compatibility score below minimum (with `--min-score`)

### Analysis Scope

The checker analyzes:

**Python Files (.py)**:
- `sqlite3` module imports and usage
- API calls (connect, execute, etc.)
- Placeholder styles (? vs $1)
- Dynamic type usage patterns

**SQL Files (.sql)**:
- Schema definitions (CREATE TABLE, etc.)
- Data type usage
- SQLite-specific keywords (AUTOINCREMENT, PRAGMA, etc.)
- Function calls
- Constraint syntax

---

## Interpreting Results

### Compatibility Score

The compatibility score (0-100%) indicates overall migration readiness:

| Score Range | Status | Interpretation |
|-------------|--------|----------------|
| 90-100% | Excellent | Ready for migration with minimal changes |
| 70-89% | Good | Migration feasible with moderate effort |
| 50-69% | Warning | Significant refactoring required |
| 0-49% | Poor | Major compatibility issues, extensive work needed |

### Severity Levels

#### CRITICAL (Red)
**Impact**: Migration will fail or cause data loss
**Action**: MUST fix before migration
**Examples**:
- `AUTOINCREMENT` instead of `SERIAL`
- `?` placeholders instead of `$1, $2, ...`
- `ATTACH DATABASE` statements

#### WARNING (Yellow)
**Impact**: May cause runtime issues or data inconsistencies
**Action**: SHOULD fix to avoid problems
**Examples**:
- `BLOB` type (not yet supported)
- `WITHOUT ROWID` tables
- `PRAGMA` statements

#### INFO (Blue)
**Impact**: Best practice suggestions, no functional impact
**Action**: Consider fixing for optimization
**Examples**:
- `INTEGER PRIMARY KEY` → `SERIAL PRIMARY KEY`
- `REAL` → `FLOAT8`
- Using `sqlite3` module (informational)

### Sample Output

```
================================================================================
HeliosDB SQLite Compatibility Check
================================================================================
Files Analyzed: 15 (12 Python, 3 SQL)
Compatibility Score: 72.5%
Issues Found: 18
  Critical: 3
  Warnings: 8
  Info: 7
================================================================================

CRITICAL ISSUES (3):
--------------------------------------------------------------------------------

  app/database.py:45
  ? placeholders: SQLite ? placeholders not supported
  Suggestion: Use PostgreSQL-style $1, $2, ... placeholders
  HeliosDB Alternative: Use $1, $2, $3 instead of ?, ?, ?

  schema/users.sql:12
  AUTOINCREMENT: AUTOINCREMENT not supported
  Suggestion: Use SERIAL or BIGSERIAL instead
  HeliosDB Alternative: SERIAL PRIMARY KEY
```

### Understanding Issue Details

Each issue provides:

1. **Location**: File path and line number
2. **Feature**: SQLite feature causing incompatibility
3. **Message**: What's wrong
4. **Suggestion**: How to fix it
5. **HeliosDB Alternative**: Exact replacement syntax
6. **Confidence**: Detection confidence (0-100%)
7. **Priority**: Calculated urgency score

---

## Fixing Common Issues

### Issue 1: AUTOINCREMENT → SERIAL

**Detected Issue**:
```sql
CREATE TABLE users (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    name TEXT
);
```

**Fix**:
```sql
CREATE TABLE users (
    id SERIAL PRIMARY KEY,
    name TEXT
);
```

**Severity**: CRITICAL
**Effort**: Low (simple find-replace)

---

### Issue 2: ? Placeholders → $1, $2, ...

**Detected Issue** (Python):
```python
cursor.execute("SELECT * FROM users WHERE id = ?", (user_id,))
```

**Fix**:
```python
db.query("SELECT * FROM users WHERE id = $1", &[&user_id])
```

**Severity**: CRITICAL
**Effort**: Medium (requires API migration)

---

### Issue 3: sqlite3.connect() → EmbeddedDatabase

**Detected Issue**:
```python
import sqlite3
conn = sqlite3.connect('mydb.db')
cursor = conn.cursor()
cursor.execute("SELECT * FROM users")
```

**Fix** (Rust):
```rust
use heliosdb_lite::EmbeddedDatabase;

let db = EmbeddedDatabase::new("mydb.db")?;
let results = db.query("SELECT * FROM users", &[])?;
```

**Severity**: CRITICAL
**Effort**: High (language migration)

---

### Issue 4: ATTACH DATABASE

**Detected Issue**:
```sql
ATTACH DATABASE 'other.db' AS other;
SELECT * FROM other.table;
```

**Fix**: Merge databases or use separate connections
```sql
-- Option 1: Merge into single database
CREATE TABLE other_table AS SELECT * FROM other_database.table;

-- Option 2: Use separate connections in application code
let db1 = EmbeddedDatabase::new("main.db")?;
let db2 = EmbeddedDatabase::new("other.db")?;
```

**Severity**: CRITICAL
**Effort**: High (architectural change)

---

### Issue 5: PRAGMA Statements

**Detected Issue**:
```sql
PRAGMA foreign_keys = ON;
PRAGMA journal_mode = WAL;
```

**Fix**: Use HeliosDB configuration
```toml
# heliosdb.toml
[database]
enforce_foreign_keys = true

[storage]
journal_mode = "wal"
```

**Severity**: WARNING
**Effort**: Low (configuration migration)

---

### Issue 6: Type Affinity (Dynamic Typing)

**Detected Issue**:
```python
# SQLite allows this:
cursor.execute("INSERT INTO users (id, name) VALUES (?, ?)", ('1', 123))
```

**Fix**: Use strict types
```python
# HeliosDB requires proper types:
db.execute("INSERT INTO users (id, name) VALUES ($1, $2)", &[&1, &"123"])
```

**Severity**: WARNING
**Effort**: Medium (requires data validation)

---

## Integration with CI/CD

### GitHub Actions

Create `.github/workflows/sqlite-compat-check.yml`:

```yaml
name: SQLite Compatibility Check

on: [push, pull_request]

jobs:
  check-compatibility:
    runs-on: ubuntu-latest

    steps:
      - uses: actions/checkout@v3

      - name: Set up Python
        uses: actions/setup-python@v4
        with:
          python-version: '3.9'

      - name: Install dependencies
        run: |
          pip install sqlparse

      - name: Run compatibility checker
        run: |
          cd tools/compatibility_checker
          python HELIOSDB_SQLITE_COMPATIBILITY_CHECKER.py ../../ \
            --json report.json \
            --html report.html \
            --fail-on-critical \
            --min-score 70

      - name: Upload reports
        if: always()
        uses: actions/upload-artifact@v3
        with:
          name: compatibility-reports
          path: |
            tools/compatibility_checker/report.json
            tools/compatibility_checker/report.html

      - name: Comment on PR
        if: github.event_name == 'pull_request' && failure()
        uses: actions/github-script@v6
        with:
          script: |
            const fs = require('fs');
            const report = JSON.parse(fs.readFileSync('tools/compatibility_checker/report.json'));
            const comment = `## SQLite Compatibility Check Failed

            **Score**: ${report.compatibility_score}%
            **Critical Issues**: ${report.critical_issues}
            **Warnings**: ${report.warnings}

            Please review the [detailed report](${context.payload.pull_request.html_url}/checks).`;

            github.rest.issues.createComment({
              issue_number: context.issue.number,
              owner: context.repo.owner,
              repo: context.repo.repo,
              body: comment
            });
```

### GitLab CI

Create `.gitlab-ci.yml`:

```yaml
sqlite-compat-check:
  stage: test
  image: python:3.9

  before_script:
    - pip install sqlparse

  script:
    - cd tools/compatibility_checker
    - python HELIOSDB_SQLITE_COMPATIBILITY_CHECKER.py ../../
        --json report.json
        --html report.html
        --fail-on-critical
        --min-score 70

  artifacts:
    when: always
    paths:
      - tools/compatibility_checker/report.json
      - tools/compatibility_checker/report.html
    reports:
      junit: tools/compatibility_checker/report.json

  only:
    - merge_requests
    - main
```

### Jenkins

Create `Jenkinsfile`:

```groovy
pipeline {
    agent any

    stages {
        stage('SQLite Compatibility Check') {
            steps {
                sh '''
                    pip install sqlparse
                    cd tools/compatibility_checker
                    python HELIOSDB_SQLITE_COMPATIBILITY_CHECKER.py ../../ \
                        --json report.json \
                        --html report.html \
                        --fail-on-critical \
                        --min-score 70
                '''
            }
        }
    }

    post {
        always {
            archiveArtifacts artifacts: 'tools/compatibility_checker/report.*', allowEmptyArchive: true
            publishHTML([
                reportDir: 'tools/compatibility_checker',
                reportFiles: 'report.html',
                reportName: 'SQLite Compatibility Report'
            ])
        }
    }
}
```

### Pre-commit Hook

Create `.git/hooks/pre-commit`:

```bash
#!/bin/bash

echo "Running SQLite compatibility check..."

cd tools/compatibility_checker
python HELIOSDB_SQLITE_COMPATIBILITY_CHECKER.py ../../ --fail-on-critical

if [ $? -ne 0 ]; then
    echo "❌ Critical SQLite compatibility issues found!"
    echo "Run 'python tools/compatibility_checker/HELIOSDB_SQLITE_COMPATIBILITY_CHECKER.py . --html report.html' for details"
    exit 1
fi

echo "✅ SQLite compatibility check passed"
exit 0
```

Make executable:
```bash
chmod +x .git/hooks/pre-commit
```

---

## False Positive Handling

### Identifying False Positives

False positives may occur when:

1. Code uses `sqlite3` for unrelated purposes (testing, utilities)
2. SQL keywords appear in comments or strings
3. Pattern matching catches similar but valid syntax

### Confidence Scores

Each issue includes a confidence score (0.0-1.0):

- **1.0**: Definite incompatibility (e.g., `AUTOINCREMENT`)
- **0.9-0.95**: Very likely incompatible (e.g., `PRAGMA`)
- **0.7-0.85**: Likely incompatible (e.g., type affinity)
- **0.5-0.65**: Possible issue (e.g., dynamic typing patterns)

**Best Practice**: Focus on issues with confidence ≥ 0.9 first

### Suppressing False Positives

Add suppression comments:

```python
# HELIOSDB_COMPAT_IGNORE: sqlite3 used for test fixtures only
import sqlite3
```

```sql
-- HELIOSDB_COMPAT_IGNORE: backwards compatibility wrapper
CREATE TABLE legacy (id INTEGER PRIMARY KEY AUTOINCREMENT);
```

### Reporting False Positives

If you encounter false positives, please report them:

1. Create an issue at https://github.com/heliosdb/heliosdb-lite/issues
2. Include:
   - Code snippet causing false positive
   - Why it's a false positive
   - Expected behavior

---

## Advanced Usage

### Programmatic Usage

```python
from HELIOSDB_SQLITE_COMPATIBILITY_CHECKER import HeliosDBCompatibilityChecker

checker = HeliosDBCompatibilityChecker(['/path/to/project'], recursive=True)
report = checker.check()

print(f"Score: {report.compatibility_score}%")
print(f"Critical: {report.critical_issues}")

# Filter high-priority issues
from HELIOSDB_SQLITE_INCOMPATIBILITY_REPORT import ReportPrioritizer
high_priority = [i for i in report.issues
                 if ReportPrioritizer.calculate_priority(i) > 100]
```

### Custom Report Generation

```python
from HELIOSDB_SQLITE_INCOMPATIBILITY_REPORT import (
    HTMLReportGenerator, MarkdownReportGenerator, ConsoleReportPrinter
)

# Generate multiple formats
html_gen = HTMLReportGenerator(report)
html_gen.generate('report.html')

md_gen = MarkdownReportGenerator(report)
md_gen.generate('report.md')

console_printer = ConsoleReportPrinter(report)
console_printer.print_summary()
```

### Pytest Integration

Create `conftest.py`:

```python
import pytest
from HELIOSDB_SQLITE_COMPATIBILITY_CHECKER import HeliosDBCompatibilityChecker

@pytest.fixture(scope='session')
def compatibility_report():
    checker = HeliosDBCompatibilityChecker(['.'], recursive=True)
    return checker.check()

def test_no_critical_issues(compatibility_report):
    assert compatibility_report.critical_issues == 0, \
        f"Found {compatibility_report.critical_issues} critical compatibility issues"

def test_min_compatibility_score(compatibility_report):
    min_score = 80.0
    assert compatibility_report.compatibility_score >= min_score, \
        f"Compatibility score {compatibility_report.compatibility_score}% below minimum {min_score}%"
```

Run tests:
```bash
pytest -v
```

---

## Troubleshooting

### Issue: "ModuleNotFoundError: No module named 'sqlparse'"

**Solution**: Install dependencies
```bash
pip install sqlparse
```

### Issue: "UnicodeDecodeError when reading files"

**Solution**: Ensure files are UTF-8 encoded or specify encoding
```bash
# Convert files to UTF-8
iconv -f ISO-8859-1 -t UTF-8 file.sql > file_utf8.sql
```

### Issue: "Too many false positives"

**Solution**:
1. Check confidence scores - focus on ≥0.9
2. Use suppression comments
3. Filter by severity (focus on CRITICAL first)

### Issue: "Checker runs too slowly"

**Solution**:
1. Analyze specific directories instead of entire project
2. Exclude vendor/dependencies: `--exclude node_modules,venv`
3. Use `--no-recursive` for shallow scans

### Issue: "HTML report doesn't open"

**Solution**: Ensure modern browser (Chrome, Firefox, Edge)
```bash
# Open with specific browser
google-chrome report.html
firefox report.html
```

---

## Additional Resources

- **Migration Guide**: `/home/claude/HeliosDB-Lite/docs/migration/MIGRATION.md`
- **HeliosDB Documentation**: https://docs.heliosdb.com/lite
- **Source Code**: `/home/claude/HeliosDB-Lite/tools/compatibility_checker/`
- **Issue Tracker**: https://github.com/heliosdb/heliosdb-lite/issues

---

## Support

For questions or issues:
- **Email**: support@heliosdb.com
- **Discord**: https://discord.gg/heliosdb
- **GitHub Issues**: https://github.com/heliosdb/heliosdb-lite/issues

---

**Document Version**: 1.0.0
**Last Updated**: 2025-12-08
**Maintainer**: HeliosDB Team
