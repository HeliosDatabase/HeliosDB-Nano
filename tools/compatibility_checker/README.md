# HeliosDB SQLite Compatibility Checker

**Production-ready static analysis tool for detecting SQLite → HeliosDB migration issues**

## Overview

The HeliosDB SQLite Compatibility Checker automatically detects compatibility issues in SQLite code and schemas **BEFORE** migration to HeliosDB-Lite, saving time and preventing runtime errors.

## Features

- **Static Code Analysis**: Detects sqlite3 module usage in Python code
- **Schema Analysis**: Identifies incompatible SQL syntax and features
- **Pre-flight Checks**: Run before migration to identify issues early
- **Automated Reports**: Generate HTML, JSON, and Markdown reports
- **CI/CD Integration**: Integrates with GitHub Actions, GitLab CI, Jenkins
- **False Positive Minimization**: Confidence scoring (0-100%) for each issue
- **Actionable Output**: Specific suggestions and HeliosDB alternatives

## Quick Start

### Installation

```bash
cd tools/compatibility_checker
pip install -r requirements.txt
```

### Basic Usage

Analyze your project:
```bash
python HELIOSDB_SQLITE_COMPATIBILITY_CHECKER.py /path/to/project
```

Generate HTML report:
```bash
python HELIOSDB_SQLITE_COMPATIBILITY_CHECKER.py /path/to/project --html report.html
```

### Example Output

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
  - schema.sql:12 - AUTOINCREMENT not supported → Use SERIAL
  - app.py:45 - ? placeholders not supported → Use $1, $2, ...
  - database.py:23 - sqlite3.connect() incompatible → Use EmbeddedDatabase
```

## What It Detects

### Schema Issues
- `AUTOINCREMENT` → Should use `SERIAL`
- `ATTACH DATABASE` → Not supported
- `PRAGMA` statements → Use configuration
- `WITHOUT ROWID` → Not supported
- Type affinity issues

### Code Issues (Python)
- `sqlite3` module usage
- `?` placeholders → Should use `$1, $2, ...`
- `connect()` API calls
- Dynamic typing patterns
- `last_insert_rowid()` usage

### Data Type Issues
- `INTEGER PRIMARY KEY` → Should use `SERIAL`
- `REAL` → Should use `FLOAT8`
- `BLOB` → Should use `BYTEA` (coming soon)

## Files

| File | Lines | Description |
|------|-------|-------------|
| `HELIOSDB_SQLITE_COMPATIBILITY_CHECKER.py` | 1500+ | Main analyzer with AST-based Python analysis and SQL parsing |
| `HELIOSDB_SQLITE_INCOMPATIBILITY_REPORT.py` | 800+ | Report generation (HTML, JSON, Markdown, console) |
| `HELIOSDB_SQLITE_CHECKER_GUIDE.md` | 700+ | Comprehensive user guide |
| `requirements.txt` | - | Python dependencies |
| `test_examples/` | - | Example test cases |

## Usage Examples

### Command Line

```bash
# Analyze single file
python HELIOSDB_SQLITE_COMPATIBILITY_CHECKER.py myapp.py

# Analyze directory (recursive)
python HELIOSDB_SQLITE_COMPATIBILITY_CHECKER.py /path/to/project

# Generate JSON report
python HELIOSDB_SQLITE_COMPATIBILITY_CHECKER.py . --json report.json

# Fail CI build on critical issues
python HELIOSDB_SQLITE_COMPATIBILITY_CHECKER.py . \
    --fail-on-critical \
    --min-score 80
```

### Programmatic Usage

```python
from HELIOSDB_SQLITE_COMPATIBILITY_CHECKER import HeliosDBCompatibilityChecker

checker = HeliosDBCompatibilityChecker(['/path/to/project'])
report = checker.check()

print(f"Score: {report.compatibility_score}%")
print(f"Critical Issues: {report.critical_issues}")

# Generate reports
from HELIOSDB_SQLITE_INCOMPATIBILITY_REPORT import HTMLReportGenerator
html_gen = HTMLReportGenerator(report)
html_gen.generate('report.html')
```

### CI/CD Integration

**GitHub Actions**:
```yaml
- name: SQLite Compatibility Check
  run: |
    pip install sqlparse
    python tools/compatibility_checker/HELIOSDB_SQLITE_COMPATIBILITY_CHECKER.py . \
      --fail-on-critical --min-score 70 --json report.json
```

**GitLab CI**:
```yaml
sqlite-compat-check:
  script:
    - pip install sqlparse
    - python tools/compatibility_checker/HELIOSDB_SQLITE_COMPATIBILITY_CHECKER.py .
        --fail-on-critical --json report.json
  artifacts:
    reports:
      junit: report.json
```

## Severity Levels

| Level | Impact | Action Required |
|-------|--------|-----------------|
| **CRITICAL** | Migration will fail | MUST fix before migration |
| **WARNING** | May cause runtime issues | SHOULD fix to avoid problems |
| **INFO** | Best practice | OPTIONAL - consider fixing |

## Confidence Scoring

Each issue includes a confidence score (0.0-1.0):

- **1.0**: Definite incompatibility
- **0.9-0.95**: Very likely incompatible
- **0.7-0.85**: Likely incompatible
- **0.5-0.65**: Possible issue (may be false positive)

**Recommendation**: Focus on issues with confidence ≥ 0.9 first

## Report Formats

### HTML Report (Interactive)
- Color-coded severity levels
- Filterable by severity
- Summary by category and file
- Code snippets and suggestions
- Modern, responsive design

### JSON Report (Machine-Readable)
```json
{
  "compatibility_score": 72.5,
  "critical_issues": 3,
  "warnings": 8,
  "info_items": 7,
  "issues": [
    {
      "severity": "critical",
      "feature": "AUTOINCREMENT",
      "message": "AUTOINCREMENT not supported",
      "suggestion": "Use SERIAL or BIGSERIAL instead",
      "heliosdb_alternative": "SERIAL PRIMARY KEY",
      "confidence": 0.95
    }
  ]
}
```

### Console Report (Human-Readable)
- Color-coded output
- Summary statistics
- Top priority issues
- Actionable suggestions

## Documentation

- **User Guide**: [HELIOSDB_SQLITE_CHECKER_GUIDE.md](HELIOSDB_SQLITE_CHECKER_GUIDE.md)
- **Migration Guide**: [../../docs/migration/MIGRATION.md](../../docs/migration/MIGRATION.md)
- **API Documentation**: See docstrings in source files

## Testing

Run unit tests:
```bash
pytest test_compatibility_checker.py -v
```

Run with coverage:
```bash
pytest test_compatibility_checker.py --cov --cov-report=html
```

## Requirements

- Python 3.7+
- sqlparse 0.4.0+

Optional:
- pytest (for testing)
- colorama (for colored output)

## Architecture

### Component Overview

```
┌─────────────────────────────────────────────────────────────┐
│                  HeliosDBCompatibilityChecker               │
│                  (Main Orchestrator)                        │
└────────────────────┬────────────────────────────────────────┘
                     │
         ┌───────────┴───────────┐
         │                       │
┌────────▼──────────┐  ┌────────▼──────────┐
│ PythonSQLite      │  │ SQLSchemaAnalyzer │
│ Analyzer          │  │                   │
│ (AST-based)       │  │ (sqlparse-based)  │
└────────┬──────────┘  └────────┬──────────┘
         │                       │
         └───────────┬───────────┘
                     │
         ┌───────────▼───────────┐
         │  CompatibilityReport  │
         │  (Data Model)         │
         └───────────┬───────────┘
                     │
         ┌───────────▼───────────┐
         │  Report Generators:   │
         │  - HTML               │
         │  - JSON               │
         │  - Markdown           │
         │  - Console            │
         └───────────────────────┘
```

### Detection Patterns

1. **AST-based Python Analysis**
   - Import detection (`import sqlite3`)
   - Function call analysis
   - Placeholder style detection
   - API usage patterns

2. **SQL Pattern Matching**
   - Regex-based feature detection
   - Keyword analysis
   - Type inference
   - Constraint checking

3. **Feature Matrix Lookup**
   - Pre-defined compatibility rules
   - Severity classification
   - Suggestion mapping

## Performance

Typical performance on a medium-sized project:

| Project Size | Files | Time | Memory |
|--------------|-------|------|--------|
| Small | 10-50 | <1s | <50MB |
| Medium | 100-500 | 1-5s | <100MB |
| Large | 1000+ | 10-30s | <200MB |

## Limitations

1. **False Positives**: Pattern matching may catch valid code
   - Solution: Use confidence scores and suppression comments

2. **Dynamic Code**: Cannot analyze dynamically generated SQL
   - Solution: Focus on static schema and common patterns

3. **Language Support**: Currently supports Python and SQL files only
   - Future: Add support for other languages (Java, Go, etc.)

4. **Runtime Behavior**: Cannot detect runtime-only issues
   - Solution: Combine with integration testing

## Roadmap

- [ ] Support for more languages (Java, Go, Node.js)
- [ ] Enhanced dynamic SQL detection
- [ ] Integration with migration tools
- [ ] Auto-fix capability for common issues
- [ ] Machine learning-based pattern detection
- [ ] Performance profiling integration

## Contributing

Contributions welcome! Please:

1. Fork the repository
2. Create a feature branch
3. Add tests for new features
4. Ensure all tests pass
5. Submit a pull request

## License

Apache-2.0 License - See LICENSE file for details

## Support

- **Documentation**: [HELIOSDB_SQLITE_CHECKER_GUIDE.md](HELIOSDB_SQLITE_CHECKER_GUIDE.md)
- **Issues**: https://github.com/heliosdb/heliosdb-lite/issues
- **Email**: support@heliosdb.com
- **Discord**: https://discord.gg/heliosdb

---

**Version**: 1.0.0
**Status**: Production Ready
**Last Updated**: 2025-12-08
**Maintainer**: HeliosDB Team
