#!/usr/bin/env python3
"""
HeliosDB SQLite Incompatibility Report Generator
=================================================

Generates comprehensive, actionable reports from compatibility analysis results.
Supports multiple output formats: HTML, JSON, Markdown, and console output.

Features:
- Severity classification and prioritization
- Location tracking with file/line numbers
- Suggested mitigations with code examples
- Priority ranking based on impact
- Summary statistics and dashboards
- Export to HTML, JSON, Markdown formats

Usage:
    from HELIOSDB_SQLITE_INCOMPATIBILITY_REPORT import ReportGenerator

    generator = ReportGenerator(compatibility_report)
    generator.generate_html('report.html')
    generator.generate_markdown('report.md')
    generator.print_summary()

Author: HeliosDB Team
Version: 1.0.0
License: Apache-2.0
"""

import json
from typing import List, Dict, Optional
from dataclasses import dataclass
from datetime import datetime
from collections import defaultdict
from pathlib import Path

# Import from companion module
try:
    from HELIOSDB_SQLITE_COMPATIBILITY_CHECKER import (
        CompatibilityReport, CompatibilityIssue, Severity
    )
except ImportError:
    # Standalone mode - define minimal stubs
    class Severity:
        CRITICAL = "critical"
        WARNING = "warning"
        INFO = "info"


class ReportPrioritizer:
    """
    Prioritizes issues based on severity, impact, and migration complexity
    """

    PRIORITY_WEIGHTS = {
        Severity.CRITICAL: 100,
        Severity.WARNING: 50,
        Severity.INFO: 10,
    }

    CATEGORY_WEIGHTS = {
        'api': 1.5,          # API changes require code refactoring
        'schema': 1.3,       # Schema changes affect data migration
        'syntax': 1.2,       # Syntax changes need query updates
        'datatype': 1.1,     # Type changes may need conversion
        'function': 1.0,     # Function changes usually have alternatives
    }

    @classmethod
    def calculate_priority(cls, issue: CompatibilityIssue) -> float:
        """Calculate priority score for an issue (higher = more urgent)"""
        base_score = cls.PRIORITY_WEIGHTS.get(issue.severity, 0)
        category_mult = cls.CATEGORY_WEIGHTS.get(issue.category, 1.0)
        confidence_mult = issue.confidence

        priority = base_score * category_mult * confidence_mult
        return priority

    @classmethod
    def rank_issues(cls, issues: List[CompatibilityIssue]) -> List[CompatibilityIssue]:
        """Sort issues by priority (highest first)"""
        return sorted(
            issues,
            key=lambda i: cls.calculate_priority(i),
            reverse=True
        )


class HTMLReportGenerator:
    """
    Generates HTML reports with interactive features
    """

    HTML_TEMPLATE = """
<!DOCTYPE html>
<html lang="en">
<head>
    <meta charset="UTF-8">
    <meta name="viewport" content="width=device-width, initial-scale=1.0">
    <title>HeliosDB SQLite Compatibility Report</title>
    <style>
        * {{ box-sizing: border-box; margin: 0; padding: 0; }}
        body {{
            font-family: -apple-system, BlinkMacSystemFont, 'Segoe UI', Roboto, sans-serif;
            line-height: 1.6;
            color: #333;
            background: #f5f5f5;
            padding: 20px;
        }}
        .container {{
            max-width: 1200px;
            margin: 0 auto;
            background: white;
            box-shadow: 0 2px 10px rgba(0,0,0,0.1);
            border-radius: 8px;
            overflow: hidden;
        }}
        .header {{
            background: linear-gradient(135deg, #667eea 0%, #764ba2 100%);
            color: white;
            padding: 30px;
        }}
        .header h1 {{ font-size: 2.5em; margin-bottom: 10px; }}
        .header .meta {{ opacity: 0.9; }}

        .score-card {{
            display: grid;
            grid-template-columns: repeat(auto-fit, minmax(200px, 1fr));
            gap: 20px;
            padding: 30px;
            background: #f9fafb;
            border-bottom: 1px solid #e5e7eb;
        }}
        .metric {{
            text-align: center;
            padding: 20px;
            background: white;
            border-radius: 8px;
            box-shadow: 0 1px 3px rgba(0,0,0,0.1);
        }}
        .metric-value {{
            font-size: 2.5em;
            font-weight: bold;
            margin-bottom: 5px;
        }}
        .metric-label {{
            color: #6b7280;
            font-size: 0.9em;
        }}
        .score-excellent {{ color: #10b981; }}
        .score-good {{ color: #3b82f6; }}
        .score-warning {{ color: #f59e0b; }}
        .score-poor {{ color: #ef4444; }}

        .issues {{
            padding: 30px;
        }}
        .issue {{
            background: white;
            border: 1px solid #e5e7eb;
            border-left: 4px solid;
            padding: 20px;
            margin-bottom: 20px;
            border-radius: 4px;
        }}
        .issue.critical {{ border-left-color: #ef4444; }}
        .issue.warning {{ border-left-color: #f59e0b; }}
        .issue.info {{ border-left-color: #3b82f6; }}

        .issue-header {{
            display: flex;
            justify-content: space-between;
            align-items: center;
            margin-bottom: 15px;
        }}
        .issue-title {{
            font-size: 1.2em;
            font-weight: 600;
        }}
        .badge {{
            display: inline-block;
            padding: 4px 12px;
            border-radius: 12px;
            font-size: 0.75em;
            font-weight: 600;
            text-transform: uppercase;
        }}
        .badge.critical {{ background: #fee2e2; color: #991b1b; }}
        .badge.warning {{ background: #fef3c7; color: #92400e; }}
        .badge.info {{ background: #dbeafe; color: #1e40af; }}

        .issue-location {{
            color: #6b7280;
            font-family: monospace;
            font-size: 0.9em;
            margin-bottom: 10px;
        }}
        .issue-message {{
            margin-bottom: 15px;
            line-height: 1.6;
        }}
        .code-snippet {{
            background: #1f2937;
            color: #f3f4f6;
            padding: 15px;
            border-radius: 6px;
            font-family: 'Courier New', monospace;
            font-size: 0.9em;
            overflow-x: auto;
            margin: 10px 0;
        }}
        .suggestion {{
            background: #ecfdf5;
            border-left: 3px solid #10b981;
            padding: 15px;
            margin-top: 10px;
            border-radius: 4px;
        }}
        .suggestion-label {{
            font-weight: 600;
            color: #065f46;
            margin-bottom: 5px;
        }}
        .alternative {{
            background: #eff6ff;
            border-left: 3px solid #3b82f6;
            padding: 15px;
            margin-top: 10px;
            border-radius: 4px;
        }}
        .alternative-label {{
            font-weight: 600;
            color: #1e40af;
            margin-bottom: 5px;
        }}
        .summary {{
            padding: 30px;
            background: #f9fafb;
        }}
        .summary h2 {{
            margin-bottom: 20px;
            color: #1f2937;
        }}
        table {{
            width: 100%;
            border-collapse: collapse;
            background: white;
        }}
        th, td {{
            padding: 12px;
            text-align: left;
            border-bottom: 1px solid #e5e7eb;
        }}
        th {{
            background: #f3f4f6;
            font-weight: 600;
            color: #374151;
        }}
        .filter-tabs {{
            display: flex;
            gap: 10px;
            padding: 20px 30px 0;
            border-bottom: 1px solid #e5e7eb;
        }}
        .tab {{
            padding: 10px 20px;
            cursor: pointer;
            border: none;
            background: none;
            color: #6b7280;
            font-weight: 500;
            border-bottom: 2px solid transparent;
            transition: all 0.3s;
        }}
        .tab.active {{
            color: #667eea;
            border-bottom-color: #667eea;
        }}
        .tab:hover {{ color: #667eea; }}
    </style>
</head>
<body>
    <div class="container">
        <div class="header">
            <h1>SQLite Compatibility Report</h1>
            <div class="meta">
                <p>Generated: {timestamp}</p>
                <p>Files Analyzed: {total_files} ({python_files} Python, {sql_files} SQL)</p>
            </div>
        </div>

        <div class="score-card">
            <div class="metric">
                <div class="metric-value {score_class}">{compatibility_score}%</div>
                <div class="metric-label">Compatibility Score</div>
            </div>
            <div class="metric">
                <div class="metric-value" style="color: #ef4444;">{critical_issues}</div>
                <div class="metric-label">Critical Issues</div>
            </div>
            <div class="metric">
                <div class="metric-value" style="color: #f59e0b;">{warnings}</div>
                <div class="metric-label">Warnings</div>
            </div>
            <div class="metric">
                <div class="metric-value" style="color: #3b82f6;">{info_items}</div>
                <div class="metric-label">Info Items</div>
            </div>
        </div>

        <div class="filter-tabs">
            <button class="tab active" onclick="filterIssues('all')">All Issues</button>
            <button class="tab" onclick="filterIssues('critical')">Critical</button>
            <button class="tab" onclick="filterIssues('warning')">Warnings</button>
            <button class="tab" onclick="filterIssues('info')">Info</button>
        </div>

        <div class="issues">
            {issues_html}
        </div>

        <div class="summary">
            <h2>Summary by Category</h2>
            {category_table}

            <h2 style="margin-top: 30px;">Summary by File</h2>
            {file_table}
        </div>
    </div>

    <script>
        function filterIssues(severity) {{
            const issues = document.querySelectorAll('.issue');
            const tabs = document.querySelectorAll('.tab');

            tabs.forEach(tab => tab.classList.remove('active'));
            event.target.classList.add('active');

            issues.forEach(issue => {{
                if (severity === 'all' || issue.classList.contains(severity)) {{
                    issue.style.display = 'block';
                }} else {{
                    issue.style.display = 'none';
                }}
            }});
        }}
    </script>
</body>
</html>
    """

    def __init__(self, report: CompatibilityReport):
        self.report = report

    def generate(self, output_path: str):
        """Generate HTML report"""
        # Determine score class
        score = self.report.compatibility_score
        if score >= 90:
            score_class = 'score-excellent'
        elif score >= 70:
            score_class = 'score-good'
        elif score >= 50:
            score_class = 'score-warning'
        else:
            score_class = 'score-poor'

        # Generate issues HTML
        ranked_issues = ReportPrioritizer.rank_issues(self.report.issues)
        issues_html = self._generate_issues_html(ranked_issues)

        # Generate summary tables
        category_table = self._generate_category_table()
        file_table = self._generate_file_table()

        # Fill template
        html = self.HTML_TEMPLATE.format(
            timestamp=datetime.now().strftime('%Y-%m-%d %H:%M:%S'),
            total_files=self.report.total_files,
            python_files=self.report.python_files,
            sql_files=self.report.sql_files,
            compatibility_score=f"{self.report.compatibility_score:.1f}",
            score_class=score_class,
            critical_issues=self.report.critical_issues,
            warnings=self.report.warnings,
            info_items=self.report.info_items,
            issues_html=issues_html,
            category_table=category_table,
            file_table=file_table
        )

        # Write to file
        with open(output_path, 'w') as f:
            f.write(html)

    def _generate_issues_html(self, issues: List[CompatibilityIssue]) -> str:
        """Generate HTML for issues list"""
        html_parts = []

        for issue in issues:
            priority = ReportPrioritizer.calculate_priority(issue)

            issue_html = f"""
            <div class="issue {issue.severity.value}">
                <div class="issue-header">
                    <div class="issue-title">{issue.feature}</div>
                    <span class="badge {issue.severity.value}">{issue.severity.value}</span>
                </div>
                <div class="issue-location">{issue.file_path}:{issue.line_number}</div>
                <div class="issue-message">{issue.message}</div>

                {f'<pre class="code-snippet">{self._escape_html(issue.code_snippet)}</pre>' if issue.code_snippet else ''}

                <div class="suggestion">
                    <div class="suggestion-label">Suggested Fix:</div>
                    {issue.suggestion}
                </div>

                {f'''<div class="alternative">
                    <div class="alternative-label">HeliosDB Alternative:</div>
                    <code>{self._escape_html(issue.heliosdb_alternative)}</code>
                </div>''' if issue.heliosdb_alternative else ''}

                <div style="margin-top: 10px; color: #6b7280; font-size: 0.85em;">
                    Priority: {priority:.1f} | Confidence: {issue.confidence*100:.0f}%
                </div>
            </div>
            """
            html_parts.append(issue_html)

        return '\n'.join(html_parts)

    def _generate_category_table(self) -> str:
        """Generate category summary table"""
        # Group by category
        by_category = defaultdict(lambda: {'critical': 0, 'warning': 0, 'info': 0, 'total': 0})

        for issue in self.report.issues:
            by_category[issue.category][issue.severity.value] += 1
            by_category[issue.category]['total'] += 1

        # Generate table
        rows = []
        for category, counts in sorted(by_category.items()):
            rows.append(f"""
                <tr>
                    <td>{category.capitalize()}</td>
                    <td style="color: #ef4444;">{counts['critical']}</td>
                    <td style="color: #f59e0b;">{counts['warning']}</td>
                    <td style="color: #3b82f6;">{counts['info']}</td>
                    <td><strong>{counts['total']}</strong></td>
                </tr>
            """)

        table = f"""
        <table>
            <thead>
                <tr>
                    <th>Category</th>
                    <th>Critical</th>
                    <th>Warnings</th>
                    <th>Info</th>
                    <th>Total</th>
                </tr>
            </thead>
            <tbody>
                {''.join(rows)}
            </tbody>
        </table>
        """
        return table

    def _generate_file_table(self) -> str:
        """Generate file summary table"""
        # Group by file
        by_file = defaultdict(lambda: {'critical': 0, 'warning': 0, 'info': 0, 'total': 0})

        for issue in self.report.issues:
            by_file[issue.file_path][issue.severity.value] += 1
            by_file[issue.file_path]['total'] += 1

        # Generate table
        rows = []
        for file_path, counts in sorted(by_file.items(), key=lambda x: x[1]['total'], reverse=True):
            # Shorten path for display
            display_path = str(Path(file_path).name)

            rows.append(f"""
                <tr>
                    <td title="{file_path}">{display_path}</td>
                    <td style="color: #ef4444;">{counts['critical']}</td>
                    <td style="color: #f59e0b;">{counts['warning']}</td>
                    <td style="color: #3b82f6;">{counts['info']}</td>
                    <td><strong>{counts['total']}</strong></td>
                </tr>
            """)

        table = f"""
        <table>
            <thead>
                <tr>
                    <th>File</th>
                    <th>Critical</th>
                    <th>Warnings</th>
                    <th>Info</th>
                    <th>Total</th>
                </tr>
            </thead>
            <tbody>
                {''.join(rows)}
            </tbody>
        </table>
        """
        return table

    @staticmethod
    def _escape_html(text: str) -> str:
        """Escape HTML special characters"""
        if not text:
            return ''
        return (text
                .replace('&', '&amp;')
                .replace('<', '&lt;')
                .replace('>', '&gt;')
                .replace('"', '&quot;')
                .replace("'", '&#39;'))


class MarkdownReportGenerator:
    """
    Generates Markdown reports for documentation/GitHub
    """

    def __init__(self, report: CompatibilityReport):
        self.report = report

    def generate(self, output_path: str):
        """Generate Markdown report"""
        md_parts = []

        # Header
        md_parts.append(f"# SQLite → HeliosDB Compatibility Report\n")
        md_parts.append(f"**Generated**: {datetime.now().strftime('%Y-%m-%d %H:%M:%S')}\n")
        md_parts.append(f"**Files Analyzed**: {self.report.total_files} ({self.report.python_files} Python, {self.report.sql_files} SQL)\n")

        # Score card
        md_parts.append("\n## Compatibility Score\n")
        md_parts.append(f"**Overall Score**: {self.report.compatibility_score:.1f}%\n")
        md_parts.append(f"- Critical Issues: {self.report.critical_issues}\n")
        md_parts.append(f"- Warnings: {self.report.warnings}\n")
        md_parts.append(f"- Info Items: {self.report.info_items}\n")

        # Issues by severity
        for severity in [Severity.CRITICAL, Severity.WARNING, Severity.INFO]:
            issues = [i for i in self.report.issues if i.severity == severity]
            if not issues:
                continue

            md_parts.append(f"\n## {severity.value.upper()} Issues ({len(issues)})\n")

            for issue in ReportPrioritizer.rank_issues(issues):
                md_parts.append(f"\n### {issue.feature}\n")
                md_parts.append(f"**Location**: `{issue.file_path}:{issue.line_number}`\n")
                md_parts.append(f"**Message**: {issue.message}\n")

                if issue.code_snippet:
                    md_parts.append(f"\n**Code**:\n```\n{issue.code_snippet}\n```\n")

                md_parts.append(f"\n**Suggestion**: {issue.suggestion}\n")

                if issue.heliosdb_alternative:
                    md_parts.append(f"\n**HeliosDB Alternative**:\n```\n{issue.heliosdb_alternative}\n```\n")

        # Write to file
        with open(output_path, 'w') as f:
            f.write('\n'.join(md_parts))


class ConsoleReportPrinter:
    """
    Pretty-prints reports to console with colors
    """

    COLORS = {
        'RED': '\033[91m',
        'YELLOW': '\033[93m',
        'BLUE': '\033[94m',
        'GREEN': '\033[92m',
        'CYAN': '\033[96m',
        'BOLD': '\033[1m',
        'END': '\033[0m'
    }

    def __init__(self, report: CompatibilityReport):
        self.report = report

    def print_summary(self):
        """Print summary to console"""
        c = self.COLORS

        print(f"\n{c['BOLD']}{'='*80}{c['END']}")
        print(f"{c['BOLD']}HeliosDB SQLite Compatibility Report{c['END']}")
        print(f"{'='*80}\n")

        # Score
        score = self.report.compatibility_score
        if score >= 90:
            color = c['GREEN']
        elif score >= 70:
            color = c['BLUE']
        elif score >= 50:
            color = c['YELLOW']
        else:
            color = c['RED']

        print(f"Compatibility Score: {color}{c['BOLD']}{score:.1f}%{c['END']}")
        print(f"Files Analyzed: {self.report.total_files}")
        print(f"  - Python: {self.report.python_files}")
        print(f"  - SQL: {self.report.sql_files}")
        print()

        # Issue counts
        print(f"{c['RED']}Critical Issues: {self.report.critical_issues}{c['END']}")
        print(f"{c['YELLOW']}Warnings: {self.report.warnings}{c['END']}")
        print(f"{c['BLUE']}Info Items: {self.report.info_items}{c['END']}")
        print()

        # Top issues
        if self.report.issues:
            print(f"{c['BOLD']}Top Priority Issues:{c['END']}")
            ranked = ReportPrioritizer.rank_issues(self.report.issues)
            for issue in ranked[:5]:
                severity_color = {
                    Severity.CRITICAL: c['RED'],
                    Severity.WARNING: c['YELLOW'],
                    Severity.INFO: c['BLUE']
                }.get(issue.severity, '')

                print(f"\n  {severity_color}[{issue.severity.value.upper()}]{c['END']} {issue.feature}")
                print(f"  Location: {issue.file_path}:{issue.line_number}")
                print(f"  Message: {issue.message}")

        print(f"\n{'='*80}\n")


def main():
    """Standalone usage for testing"""
    import sys

    if len(sys.argv) < 2:
        print("Usage: python HELIOSDB_SQLITE_INCOMPATIBILITY_REPORT.py <report.json>")
        sys.exit(1)

    # Load JSON report
    with open(sys.argv[1]) as f:
        data = json.load(f)

    # Create mock report object
    report = type('Report', (), data)()

    # Generate reports
    console = ConsoleReportPrinter(report)
    console.print_summary()


if __name__ == '__main__':
    main()
