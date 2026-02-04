#!/bin/bash
# HeliosDB SQLite Compatibility Checker Demo
# Demonstrates the tool's capabilities

set -e

echo "=========================================="
echo "HeliosDB SQLite Compatibility Checker Demo"
echo "=========================================="
echo ""

# Check Python version
echo "Checking Python version..."
python3 --version
echo ""

# Install dependencies
echo "Installing dependencies..."
pip install -q sqlparse
echo "Dependencies installed."
echo ""

# Run checker on example files
echo "Running compatibility check on example files..."
echo ""
python3 HELIOSDB_SQLITE_COMPATIBILITY_CHECKER.py test_examples/ \
    --json demo_report.json

echo ""
echo "=========================================="
echo "Demo Complete!"
echo "=========================================="
echo ""
echo "Reports generated:"
echo "  - Console output (above)"
echo "  - JSON report: demo_report.json"
echo ""
echo "Try these commands:"
echo "  # Generate HTML report"
echo "  python3 HELIOSDB_SQLITE_COMPATIBILITY_CHECKER.py test_examples/ --html demo_report.html"
echo ""
echo "  # Fail on critical issues (CI/CD mode)"
echo "  python3 HELIOSDB_SQLITE_COMPATIBILITY_CHECKER.py test_examples/ --fail-on-critical"
echo ""
echo "  # Require minimum score"
echo "  python3 HELIOSDB_SQLITE_COMPATIBILITY_CHECKER.py test_examples/ --min-score 80"
echo ""
