"""Command-line interface utilities for heliosdb-sqlite."""

import sys


def check_installation() -> int:
    """
    Run installation verification tests.

    This is the entry point for the 'heliosdb-sqlite-check' command.

    Returns:
        int: Exit code (0 for success, non-zero for failure)
    """
    # Import and run the installation test
    try:
        from pathlib import Path
        import subprocess

        # Find INSTALLATION_TEST.py
        package_dir = Path(__file__).parent.parent
        test_script = package_dir / "INSTALLATION_TEST.py"

        if test_script.exists():
            # Run the test script
            result = subprocess.run([sys.executable, str(test_script)])
            return result.returncode
        else:
            print("Error: INSTALLATION_TEST.py not found")
            print(f"Expected at: {test_script}")
            return 2

    except Exception as e:
        print(f"Error running installation check: {e}")
        return 2


if __name__ == "__main__":
    sys.exit(check_installation())
