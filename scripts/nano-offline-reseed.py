#!/usr/bin/env python3
"""Copy a stopped Nano database into a NEW directory. Linux, local POSIX disks only.

This prepares an offline physical snapshot; it does not start replication or
establish a replication history/LSN boundary. See docs/guides/offline-reseed.md.
"""
import argparse
import ctypes
import errno
import fcntl
import hashlib
import json
import os
from pathlib import Path
import shutil
import stat
import sys
import tempfile


def plain_path(value):
    path = Path(os.path.abspath(value))
    for part in (path, *path.parents):
        if part.is_symlink():
            raise ValueError(f"symlink path refused: {part}")
    return path


def walk_error(error):
    raise error


def inventory(source):
    files = []
    for root, dirs, names in os.walk(source, followlinks=False, onerror=walk_error):
        for name in dirs + names:
            path = Path(root) / name
            mode = path.lstat().st_mode
            if not (stat.S_ISDIR(mode) or stat.S_ISREG(mode)):
                raise ValueError(f"nonregular entry refused: {path}")
            if stat.S_ISREG(mode):
                if path.stat().st_nlink != 1:
                    raise ValueError(f"hard-linked file refused: {path}")
                files.append(path.relative_to(source))
    return sorted(files)


def digest(path):
    result = hashlib.sha256()
    with path.open("rb") as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b""):
            result.update(block)
    return result.hexdigest()


def publish(staging, target):
    # os.rename can replace an existing empty directory. Never overwrite even
    # when a different process creates target after the initial existence check.
    libc = ctypes.CDLL(None, use_errno=True)
    rename = getattr(libc, "renameat2", None)
    if rename is None:
        raise OSError("renameat2 required for atomic no-replace publication")
    rename.argtypes = [ctypes.c_int, ctypes.c_char_p, ctypes.c_int, ctypes.c_char_p, ctypes.c_uint]
    rename.restype = ctypes.c_int
    if rename(-100, os.fsencode(staging), -100, os.fsencode(target), 1):
        code = ctypes.get_errno()
        raise OSError(code, os.strerror(code), str(target))


def reseed(source, target):
    source, target = plain_path(source), plain_path(target)
    if source == target or source in target.parents or target in source.parents:
        raise ValueError("source and target must be separate, nonnested directories")
    if target.exists():
        raise ValueError("target already exists; choose a NEW path (old state is never replaced)")
    if not target.parent.is_dir():
        raise ValueError("target parent must already exist")
    if not source.is_dir():
        raise ValueError("source must be an existing Nano data directory")
    # Do not create LOCK: an arbitrary empty directory is not a database.
    lock_fd = os.open(source / "LOCK", os.O_RDWR | os.O_NOFOLLOW)
    staging = None
    try:
        if not stat.S_ISREG(os.fstat(lock_fd).st_mode):
            raise ValueError("source LOCK must be a regular file")
        try:
            # RocksDB uses F_SETLK over the entire LOCK file, NOT flock(2).
            fcntl.lockf(lock_fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except OSError as exc:
            if exc.errno in (errno.EACCES, errno.EAGAIN):
                raise ValueError("source is open by another process; stop Nano cleanly first") from exc
            raise
        files = inventory(source)
        if Path("CURRENT") not in files:
            raise ValueError("source has no RocksDB CURRENT file")
        manifest = (source / "CURRENT").read_text().strip()
        if not manifest.startswith("MANIFEST-") or Path(manifest).name != manifest:
            raise ValueError("invalid RocksDB CURRENT manifest reference")
        if Path(manifest) not in files:
            raise ValueError("source CURRENT references a missing manifest")
        if Path("NANO-OFFLINE-SNAPSHOT.json") in files:
            # Re-copying an opened snapshot is fine; regenerate its provenance.
            files.remove(Path("NANO-OFFLINE-SNAPSHOT.json"))
        staging = Path(tempfile.mkdtemp(prefix=f".{target.name}.reseed-", suffix=".partial", dir=target.parent))
        checksums = {}
        for root, dirs, _ in os.walk(source, followlinks=False, onerror=walk_error):
            for name in dirs:
                relative = (Path(root) / name).relative_to(source)
                (staging / relative).mkdir(mode=0o700, parents=True, exist_ok=True)
        for relative in files:
            if relative == Path("LOCK"):
                # Closing ANY other descriptor to the same source inode would
                # drop our POSIX record lock. Never open/copy/hash source LOCK.
                continue
            destination = staging / relative
            destination.parent.mkdir(parents=True, exist_ok=True)
            shutil.copyfile(source / relative, destination, follow_symlinks=False)
            os.chmod(destination, stat.S_IMODE((source / relative).stat().st_mode) & 0o777)
            checksums[str(relative)] = digest(destination)
            if checksums[str(relative)] != digest(source / relative):
                raise ValueError(f"source changed during copy: {relative}")
            with destination.open("rb") as copied:
                os.fsync(copied.fileno())
        # New inode: no source lock or hard links are shared by the snapshot.
        (staging / "LOCK").touch(mode=0o600)
        metadata = {"format": 1, "kind": "offline-physical-snapshot", "source": str(source),
                    "replication_resume_supported": False, "sha256": checksums}
        with (staging / "NANO-OFFLINE-SNAPSHOT.json").open("x") as out:
            json.dump(metadata, out, indent=2, sort_keys=True)
            out.write("\n")
            out.flush()
            os.fsync(out.fileno())
        for root, _, _ in os.walk(staging, topdown=False, onerror=walk_error):
            fd = os.open(root, os.O_RDONLY | os.O_DIRECTORY)
            try:
                os.fsync(fd)
            finally:
                os.close(fd)
        publish(staging, target)
        staging = None
        fd = os.open(target.parent, os.O_RDONLY | os.O_DIRECTORY)
        try:
            os.fsync(fd)
        finally:
            os.close(fd)
        return metadata
    except Exception:
        if staging is not None:
            print(f"Incomplete private snapshot retained at {staging}; do not open it as a database", file=sys.stderr)
        raise
    finally:
        os.close(lock_fd)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--source", required=True, help="stopped local Nano data directory")
    parser.add_argument("--target", required=True, help="new, nonexistent local directory")
    parser.add_argument("--confirm-offline", action="store_true", required=True,
                        help="confirm clean shutdown and no other writer for the duration of the copy")
    args = parser.parse_args()
    try:
        metadata = reseed(args.source, args.target)
    except (OSError, ValueError) as exc:
        parser.exit(1, f"Offline snapshot refused: {exc}\n")
    print(f"Created offline snapshot with {len(metadata['sha256'])} verified files at {args.target}")
    print("Streaming has NOT been started or certified. See docs/guides/offline-reseed.md.")


if __name__ == "__main__":
    main()
