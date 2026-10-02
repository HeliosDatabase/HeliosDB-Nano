"""Filesystem safety regression tests; actual RocksDB coverage lives in tests/."""
import importlib.util
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

SPEC = importlib.util.spec_from_file_location("reseed", Path(__file__).with_name("nano-offline-reseed.py"))
RESEED = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(RESEED)


class OfflineReseedTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.source = self.root / "source"
        self.source.mkdir()
        for name, data in {"LOCK": b"", "CURRENT": b"MANIFEST-000001\n", "MANIFEST-000001": b"fixture",
                           "000001.sst": b"rows", "000002.log": b"durable WAL"}.items():
            (self.source / name).write_bytes(data)
        self.target = self.root / "new"

    def test_copy_preserves_bytes_uses_distinct_inodes_and_does_not_modify_source(self):
        before = {p.name: p.read_bytes() for p in self.source.iterdir()}
        result = RESEED.reseed(self.source, self.target)
        self.assertFalse(result["replication_resume_supported"])
        for name, data in before.items():
            self.assertEqual((self.target / name).read_bytes(), data)
            self.assertEqual((self.source / name).read_bytes(), data)
            self.assertNotEqual((self.source / name).stat().st_ino, (self.target / name).stat().st_ino)
        self.assertEqual(set(before), {p.name for p in self.source.iterdir()})

    def test_scandir_failure_refuses_to_publish_incomplete_copy(self):
        from unittest.mock import patch
        import os
        nested = self.source / "unreadable"
        nested.mkdir()
        (nested / "rows.sst").write_bytes(b"must not be omitted")
        original = os.scandir
        def failing_scandir(path):
            if Path(path) == nested:
                raise PermissionError("injected directory enumeration failure")
            return original(path)
        with patch.object(RESEED.os, "scandir", side_effect=failing_scandir):
            with self.assertRaisesRegex(PermissionError, "injected"):
                RESEED.reseed(self.source, self.target)
        self.assertFalse(self.target.exists())

    def test_empty_directories_are_preserved(self):
        (self.source / "nested" / "empty").mkdir(parents=True)
        RESEED.reseed(self.source, self.target)
        self.assertTrue((self.target / "nested" / "empty").is_dir())

    def test_existing_even_empty_target_is_preserved(self):
        self.target.mkdir()
        with self.assertRaisesRegex(ValueError, "already exists"):
            RESEED.reseed(self.source, self.target)
        self.assertEqual(list(self.target.iterdir()), [])

    def test_source_and_nested_target_are_refused(self):
        for target in (self.source, self.source / "nested", self.root):
            with self.assertRaisesRegex(ValueError, "nonnested"):
                RESEED.reseed(self.source, target)

    def test_symlink_entry_is_refused(self):
        (self.source / "external").symlink_to(self.root, target_is_directory=True)
        with self.assertRaisesRegex(ValueError, "nonregular"):
            RESEED.reseed(self.source, self.target)
        self.assertFalse(self.target.exists())

    def test_symlink_source_or_target_parent_is_refused(self):
        alias = self.root / "alias"
        alias.symlink_to(self.source, target_is_directory=True)
        for source, target in ((alias, self.target), (self.source, alias / "nested")):
            with self.assertRaisesRegex(ValueError, "symlink"):
                RESEED.reseed(source, target)

    def test_live_posix_lock_is_refused(self):
        code = "import fcntl,sys; f=open(sys.argv[1],'r+'); fcntl.lockf(f,fcntl.LOCK_EX); print('ready',flush=True); sys.stdin.read()"
        with subprocess.Popen([sys.executable, "-c", code, str(self.source / "LOCK")],
                              stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True) as child:
            self.assertEqual(child.stdout.readline().strip(), "ready")
            try:
                with self.assertRaisesRegex(ValueError, "open by another process"):
                    RESEED.reseed(self.source, self.target)
            finally:
                child.stdin.close()
                child.wait(timeout=5)
        self.assertFalse(self.target.exists())

    def test_publication_race_never_overwrites_target(self):
        staging = self.root / "stage"
        staging.mkdir()
        self.target.mkdir()
        with self.assertRaises(FileExistsError):
            RESEED.publish(staging, self.target)
        self.assertTrue(staging.is_dir())
        self.assertTrue(self.target.is_dir())

    def test_hard_link_to_lock_is_refused_without_releasing_lock_early(self):
        import os
        os.link(self.source / "LOCK", self.source / "lock-alias")
        with self.assertRaisesRegex(ValueError, "hard-linked"):
            RESEED.reseed(self.source, self.target)
        self.assertFalse(self.target.exists())

    def test_invalid_source_is_refused_without_target(self):
        (self.source / "CURRENT").write_text("../escape\n")
        with self.assertRaisesRegex(ValueError, "manifest reference"):
            RESEED.reseed(self.source, self.target)
        self.assertFalse(self.target.exists())


if __name__ == "__main__":
    unittest.main()
