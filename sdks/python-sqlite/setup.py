#!/usr/bin/env python3
"""
Setup script for heliosdb-sqlite package.

This package provides a SQLite-compatible interface for HeliosDB-Lite,
offering drop-in replacement functionality with enhanced features like
vector search, encryption, and time-travel queries.

Build and Installation:
    python -m pip install .
    python -m pip install -e .  # Development mode
    python -m build             # Build wheel

Platform Support:
    - Linux (x86_64, aarch64) - manylinux2014
    - macOS (x86_64, arm64) - 10.12+
    - Windows (x86_64) - Win10+
"""

import os
import platform
import shutil
import subprocess
import sys
from pathlib import Path
from typing import List

from setuptools import Extension, find_packages, setup
from setuptools.command.build_ext import build_ext
from setuptools.command.install import install

# Package metadata (also defined in pyproject.toml)
PACKAGE_NAME = "heliosdb-sqlite"
PACKAGE_VERSION = "3.0.0"
PACKAGE_DESCRIPTION = (
    "SQLite-compatible interface for HeliosDB-Lite with vector search, "
    "encryption, and time-travel queries"
)

# Paths
ROOT_DIR = Path(__file__).parent.absolute()
HELIOSDB_ROOT = ROOT_DIR.parent.parent
CARGO_MANIFEST = HELIOSDB_ROOT / "Cargo.toml"
TARGET_DIR = HELIOSDB_ROOT / "target" / "release"


class BuildHeliosDBExtension(build_ext):
    """Custom build extension to compile HeliosDB-Lite binary."""

    def run(self) -> None:
        """Build HeliosDB-Lite binary using Cargo."""
        if not CARGO_MANIFEST.exists():
            raise FileNotFoundError(
                f"Cargo.toml not found at {CARGO_MANIFEST}. "
                "Ensure HeliosDB-Lite repository structure is intact."
            )

        self.announce("Building HeliosDB-Lite binary with Cargo...", level=3)

        # Check for Rust toolchain
        try:
            subprocess.run(
                ["rustc", "--version"],
                check=True,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
            )
        except (subprocess.CalledProcessError, FileNotFoundError):
            raise RuntimeError(
                "Rust toolchain not found. Install from https://rustup.rs/\n"
                "Run: curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh"
            )

        # Build command
        cargo_args = [
            "cargo",
            "build",
            "--release",
            "--manifest-path",
            str(CARGO_MANIFEST),
            "--bin",
            "heliosdb-lite",
        ]

        # Add target triple for cross-compilation if needed
        target_triple = os.environ.get("CARGO_BUILD_TARGET")
        if target_triple:
            cargo_args.extend(["--target", target_triple])

        # Run cargo build
        try:
            self.announce(f"Running: {' '.join(cargo_args)}", level=3)
            subprocess.run(cargo_args, check=True, cwd=HELIOSDB_ROOT)
        except subprocess.CalledProcessError as e:
            raise RuntimeError(f"Failed to build HeliosDB-Lite binary: {e}")

        # Copy binary to package
        self._copy_binary_to_package()

        # Continue with standard build_ext
        super().run()

    def _copy_binary_to_package(self) -> None:
        """Copy compiled binary to package binaries directory."""
        platform_name = platform.system().lower()
        arch = platform.machine().lower()

        # Map architecture names
        arch_map = {
            "x86_64": "x86_64",
            "amd64": "x86_64",
            "arm64": "arm64",
            "aarch64": "aarch64",
        }
        arch = arch_map.get(arch, arch)

        # Binary name
        binary_name = f"heliosdb-lite-{platform_name}-{arch}"
        if platform_name == "windows":
            source_binary = TARGET_DIR / "heliosdb-lite.exe"
            binary_name += ".exe"
        else:
            source_binary = TARGET_DIR / "heliosdb-lite"

        if not source_binary.exists():
            raise FileNotFoundError(
                f"Built binary not found at {source_binary}. Build may have failed."
            )

        # Destination directory
        dest_dir = ROOT_DIR / "heliosdb_sqlite" / "binaries"
        dest_dir.mkdir(parents=True, exist_ok=True)

        dest_binary = dest_dir / binary_name

        self.announce(f"Copying binary: {source_binary} -> {dest_binary}", level=3)
        shutil.copy2(source_binary, dest_binary)

        # Make executable on Unix
        if platform_name != "windows":
            os.chmod(dest_binary, 0o755)

        self.announce(f"Binary copied successfully: {dest_binary}", level=3)


class InstallWithBinaryCheck(install):
    """Custom install command to verify binary installation."""

    def run(self) -> None:
        """Run installation and verify binary."""
        super().run()
        self._verify_binary_installation()

    def _verify_binary_installation(self) -> None:
        """Verify that HeliosDB-Lite binary was installed correctly."""
        try:
            import heliosdb_sqlite

            binary_path = heliosdb_sqlite.get_binary_path()
            if not binary_path.exists():
                self.warn(
                    f"Warning: HeliosDB-Lite binary not found at {binary_path}. "
                    "Package may not function correctly."
                )
            else:
                self.announce(f"Binary verified at: {binary_path}", level=3)
        except ImportError:
            self.warn("Warning: Could not import heliosdb_sqlite to verify binary.")


def get_long_description() -> str:
    """Read long description from README.md."""
    readme_path = ROOT_DIR / "README.md"
    if readme_path.exists():
        return readme_path.read_text(encoding="utf-8")
    return PACKAGE_DESCRIPTION


def get_version() -> str:
    """Get version from _version.py or fallback to default."""
    version_file = ROOT_DIR / "heliosdb_sqlite" / "_version.py"
    if version_file.exists():
        namespace = {}
        exec(version_file.read_text(), namespace)
        return namespace.get("__version__", PACKAGE_VERSION)
    return PACKAGE_VERSION


def get_requirements() -> List[str]:
    """Get runtime requirements."""
    # No runtime requirements - all dependencies bundled
    return []


def get_dev_requirements() -> List[str]:
    """Get development requirements."""
    requirements_dev = ROOT_DIR / "requirements-dev.txt"
    if requirements_dev.exists():
        return [
            line.strip()
            for line in requirements_dev.read_text().splitlines()
            if line.strip() and not line.startswith("#")
        ]
    return [
        "pytest>=7.0.0",
        "pytest-cov>=4.0.0",
        "pytest-asyncio>=0.21.0",
        "mypy>=1.0.0",
        "ruff>=0.1.0",
        "black>=23.0.0",
    ]


# Main setup configuration
if __name__ == "__main__":
    setup(
        # Basic metadata
        name=PACKAGE_NAME,
        version=get_version(),
        description=PACKAGE_DESCRIPTION,
        long_description=get_long_description(),
        long_description_content_type="text/markdown",
        # Author information
        author="HeliosDB Team",
        author_email="team@heliosdb.io",
        maintainer="HeliosDB Team",
        maintainer_email="team@heliosdb.io",
        # URLs
        url="https://github.com/heliosdb/heliosdb-lite",
        project_urls={
            "Homepage": "https://github.com/heliosdb/heliosdb-lite",
            "Documentation": "https://docs.heliosdb.io/sqlite-compat",
            "Repository": "https://github.com/heliosdb/heliosdb-lite",
            "Source Code": "https://github.com/heliosdb/heliosdb-lite/tree/main/sdks/python-sqlite",
            "Issues": "https://github.com/heliosdb/heliosdb-lite/issues",
            "Changelog": "https://github.com/heliosdb/heliosdb-lite/blob/main/CHANGELOG.md",
            "Discord": "https://discord.gg/heliosdb",
        },
        # License
        license="Apache-2.0",
        license_files=["LICENSE"],
        # Python version support
        python_requires=">=3.8",
        # Package discovery
        packages=find_packages(exclude=["tests", "tests.*", "docs", "examples"]),
        package_data={
            "heliosdb_sqlite": [
                "py.typed",
                "binaries/*",
                "*.pyi",
            ],
        },
        include_package_data=True,
        zip_safe=False,
        # Dependencies
        install_requires=get_requirements(),
        extras_require={
            "vector": [
                "numpy>=1.20.0,<2.0",
                "scipy>=1.7.0",
            ],
            "pandas": [
                "pandas>=1.3.0",
                "pyarrow>=10.0.0",
            ],
            "async": [
                "aiofiles>=23.0.0",
            ],
            "types": [
                "types-setuptools>=65.0.0",
            ],
            "dev": get_dev_requirements(),
            "docs": [
                "sphinx>=6.0.0",
                "sphinx-rtd-theme>=1.3.0",
                "myst-parser>=2.0.0",
            ],
            "all": [
                "numpy>=1.20.0,<2.0",
                "scipy>=1.7.0",
                "pandas>=1.3.0",
                "pyarrow>=10.0.0",
                "aiofiles>=23.0.0",
            ],
        },
        # Entry points
        entry_points={
            "console_scripts": [
                "heliosdb-sqlite-check=heliosdb_sqlite.cli:check_installation",
            ],
        },
        # Build extensions
        ext_modules=[],  # No Python C extensions
        cmdclass={
            "build_ext": BuildHeliosDBExtension,
            "install": InstallWithBinaryCheck,
        },
        # PyPI classifiers
        classifiers=[
            # Development Status
            "Development Status :: 5 - Production/Stable",
            # Audience
            "Intended Audience :: Developers",
            "Intended Audience :: Science/Research",
            "Intended Audience :: Information Technology",
            # License
            "License :: OSI Approved :: Apache Software License",
            # Operating Systems
            "Operating System :: OS Independent",
            "Operating System :: POSIX :: Linux",
            "Operating System :: MacOS :: MacOS X",
            "Operating System :: Microsoft :: Windows",
            # Programming Languages
            "Programming Language :: Python :: 3",
            "Programming Language :: Python :: 3.8",
            "Programming Language :: Python :: 3.9",
            "Programming Language :: Python :: 3.10",
            "Programming Language :: Python :: 3.11",
            "Programming Language :: Python :: 3.12",
            "Programming Language :: Python :: 3.13",
            "Programming Language :: Python :: 3 :: Only",
            "Programming Language :: Rust",
            "Programming Language :: SQL",
            # Topics
            "Topic :: Database",
            "Topic :: Database :: Database Engines/Servers",
            "Topic :: Software Development :: Libraries :: Python Modules",
            "Topic :: Scientific/Engineering :: Artificial Intelligence",
            "Typing :: Typed",
        ],
        keywords=[
            "sqlite",
            "database",
            "embedded",
            "vector-search",
            "encryption",
            "postgresql",
            "compatibility",
            "sql",
            "ai",
            "machine-learning",
            "rag",
            "semantic-search",
            "time-travel",
        ],
    )
