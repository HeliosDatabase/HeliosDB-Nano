"""
Setup script for HeliosDB SQLite Compatibility Layer

Provides drop-in replacement for Python's sqlite3 module that routes
all operations to HeliosDB-Lite while maintaining 100% API compatibility.

Installation:
    pip install heliosdb-sqlite

Development:
    pip install -e .[dev]
"""

from setuptools import setup, find_packages
from pathlib import Path

# Read README for long description
readme_path = Path(__file__).parent / "heliosdb_sqlite" / "README.md"
if readme_path.exists():
    long_description = readme_path.read_text(encoding='utf-8')
else:
    long_description = "HeliosDB SQLite Compatibility Layer - Drop-in replacement for sqlite3 module"

# Version
VERSION = "3.0.1"

setup(
    name="heliosdb-sqlite",
    version=VERSION,
    author="HeliosDB Team",
    author_email="support@heliosdb.io",
    description="Drop-in sqlite3 replacement for HeliosDB-Lite with advanced features",
    long_description=long_description,
    long_description_content_type="text/markdown",
    url="https://github.com/heliosdb/heliosdb-nano",
    project_urls={
        "Bug Tracker": "https://github.com/heliosdb/heliosdb-nano/issues",
        "Documentation": "https://github.com/heliosdb/heliosdb-nano/tree/main/sdks/python",
        "Source Code": "https://github.com/heliosdb/heliosdb-nano",
    },
    packages=find_packages(include=['heliosdb_sqlite', 'heliosdb_sqlite.*']),
    classifiers=[
        "Development Status :: 4 - Beta",
        "Intended Audience :: Developers",
        "Topic :: Database",
        "Topic :: Database :: Database Engines/Servers",
        "Topic :: Software Development :: Libraries :: Python Modules",
        "License :: OSI Approved :: MIT License",
        "Programming Language :: Python :: 3",
        "Programming Language :: Python :: 3.8",
        "Programming Language :: Python :: 3.9",
        "Programming Language :: Python :: 3.10",
        "Programming Language :: Python :: 3.11",
        "Programming Language :: Python :: 3.12",
        "Operating System :: OS Independent",
        "Operating System :: POSIX :: Linux",
        "Operating System :: MacOS :: MacOS X",
        "Typing :: Typed",
    ],
    keywords=[
        "database",
        "sqlite",
        "sqlite3",
        "heliosdb",
        "vector-search",
        "vector-database",
        "embedding",
        "time-travel",
        "branching",
        "encryption",
        "postgresql",
        "sql",
    ],
    python_requires=">=3.8",
    install_requires=[
        # Core dependencies (minimal for embedded mode)
    ],
    extras_require={
        # Development dependencies
        "dev": [
            "pytest>=7.0",
            "pytest-cov>=4.0",
            "black>=23.0",
            "mypy>=1.0",
            "flake8>=6.0",
            "isort>=5.0",
        ],
        # Daemon mode dependencies (optional)
        "daemon": [
            "psycopg2-binary>=2.9",  # For PostgreSQL protocol connection
        ],
        # All optional dependencies
        "all": [
            "psycopg2-binary>=2.9",
        ],
    },
    entry_points={
        "console_scripts": [
            # Future: heliosdb-sqlite-cli for command-line tools
        ],
    },
    include_package_data=True,
    package_data={
        "heliosdb_sqlite": [
            "*.md",
            "py.typed",  # PEP 561 typed package marker
        ],
    },
    zip_safe=False,
)
