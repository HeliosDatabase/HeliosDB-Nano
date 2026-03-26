"""Utility functions for heliosdb-sqlite package."""

import os
import platform
import shutil
from pathlib import Path
from typing import Optional


def get_binary_path() -> Path:
    """
    Get the path to the HeliosDB Nano binary.

    The binary is extracted from the package on first use and cached
    in ~/.heliosdb/bin/ directory.

    Returns:
        Path: Path to the HeliosDB Nano binary

    Raises:
        FileNotFoundError: If binary is not found in package or cache
        RuntimeError: If platform is not supported

    Example:
        >>> binary_path = get_binary_path()
        >>> print(f"Binary located at: {binary_path}")
    """
    # Determine platform-specific binary name
    platform_name = platform.system().lower()
    arch = platform.machine().lower()

    # Map architecture names to canonical form
    arch_map = {
        "x86_64": "x86_64",
        "amd64": "x86_64",
        "arm64": "arm64",
        "aarch64": "aarch64",
    }
    arch = arch_map.get(arch, arch)

    binary_name = f"heliosdb-nano-{platform_name}-{arch}"
    if platform_name == "windows":
        binary_name += ".exe"

    # Check cache directory first
    cache_dir = Path.home() / ".heliosdb" / "bin"
    cached_binary = cache_dir / binary_name

    if cached_binary.exists():
        return cached_binary

    # Extract from package if not in cache
    cache_dir.mkdir(parents=True, exist_ok=True)

    # Find bundled binary
    package_dir = Path(__file__).parent
    bundled_binary = package_dir / "binaries" / binary_name

    if not bundled_binary.exists():
        # Try to find any binary (for development/testing)
        binaries_dir = package_dir / "binaries"
        if binaries_dir.exists():
            available = list(binaries_dir.glob("heliosdb-nano-*"))
            if available:
                raise RuntimeError(
                    f"Binary for {platform_name}-{arch} not found. "
                    f"Available: {[b.name for b in available]}"
                )

        raise FileNotFoundError(
            f"HeliosDB Nano binary not found for {platform_name}-{arch}. "
            f"Expected at: {bundled_binary}\n"
            f"Your platform may not be supported, or you may need to build from source."
        )

    # Copy to cache
    shutil.copy2(bundled_binary, cached_binary)

    # Make executable on Unix-like systems
    if platform_name != "windows":
        os.chmod(cached_binary, 0o755)

    return cached_binary


def get_binary_version() -> Optional[str]:
    """
    Get the version of the HeliosDB Nano binary.

    Returns:
        Optional[str]: Version string, or None if unable to determine

    Example:
        >>> version = get_binary_version()
        >>> print(f"HeliosDB Nano binary version: {version}")
    """
    import subprocess

    try:
        binary_path = get_binary_path()
        result = subprocess.run(
            [str(binary_path), "--version"],
            capture_output=True,
            text=True,
            timeout=5,
        )
        if result.returncode == 0:
            # Parse version from output (e.g., "heliosdb-nano 3.0.0")
            version_line = result.stdout.strip()
            if version_line:
                parts = version_line.split()
                if len(parts) >= 2:
                    return parts[-1]
        return None
    except Exception:
        return None
