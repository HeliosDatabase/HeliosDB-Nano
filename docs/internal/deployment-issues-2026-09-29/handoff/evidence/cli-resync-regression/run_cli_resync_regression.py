#!/usr/bin/env python3
"""Owned CLI physical resync regression: NON-SERVING raw evidence only.

Requires prebuilt --binary and --inspector; never builds, deletes data, attaches
SQL to a receiver, kills by name, or reuses an existing database/container. Run
under the campaign shared host lock/no-swap scope. Exit 0 pass, 1 assertion
regression, 2 setup/interruption/cleanup/identity uncertainty.
"""
import argparse
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import re
import signal
import socket
import stat
import subprocess
import tempfile
import time
import traceback
import uuid


def digest(path):
    h = hashlib.sha256()
    with path.open("rb") as source:
        for block in iter(lambda: source.read(1024 * 1024), b""):
            h.update(block)
    return h.hexdigest()


def census(root, allow_empty=False):
    """Complete inventory; missing/unreadable roots never prove preservation."""
    root_metadata = root.lstat()
    if not stat.S_ISDIR(root_metadata.st_mode):
        raise RuntimeError(f"inventory root is not a real nonsymlink directory: {root}")
    result = {".": {"kind": "directory", "mode": stat.S_IMODE(root_metadata.st_mode)}}

    def traversal_error(error):
        raise error

    for directory, dirs, files in os.walk(root, followlinks=False, onerror=traversal_error):
        for name in sorted(dirs + files):
            path = Path(directory) / name
            metadata = path.lstat()
            key = str(path.relative_to(root))
            if stat.S_ISLNK(metadata.st_mode):
                result[key] = {"kind": "symlink", "target": os.readlink(path)}
            elif stat.S_ISDIR(metadata.st_mode):
                result[key] = {"kind": "directory", "mode": stat.S_IMODE(metadata.st_mode)}
            elif stat.S_ISREG(metadata.st_mode):
                result[key] = {"kind": "file", "size": metadata.st_size,
                               "mode": stat.S_IMODE(metadata.st_mode), "sha256": digest(path)}
            else:
                raise RuntimeError(f"unexpected fixture file type: {path}")
    if not allow_empty and not any(item["kind"] == "file" for item in result.values()):
        raise RuntimeError(f"inventory database contains no regular files: {root}")
    return result


def require(condition, message):
    if not condition:
        raise AssertionError(message)


def load_helper(path):
    spec = importlib.util.spec_from_file_location("owned_resync_wire", path)
    helper = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(helper)
    return helper


def owned_listeners(child):
    if child.poll() is not None:
        raise RuntimeError("receiver exited before listener inventory")
    inodes = set()
    for fd in Path(f"/proc/{child.pid}/fd").iterdir():
        try:
            target = os.readlink(fd)
        except FileNotFoundError:
            continue
        if target.startswith("socket:["):
            inodes.add(target[8:-1])
    listeners = []
    for table in ("/proc/net/tcp", "/proc/net/tcp6"):
        for line in Path(table).read_text().splitlines()[1:]:
            columns = line.split()
            if columns[3] == "0A" and columns[9] in inodes:
                listeners.append({"table": table, "address": columns[1], "inode": columns[9]})
    return listeners


def make_fixture_class(helper):
    class Fixture(helper.Fixture):
        def __init__(self, root, binary, inspector):
            super().__init__(root, binary)
            self.inspector = inspector
            self.live = set()
            self.serial = 0

        def alive(self):
            for child in self.live:
                if child.poll() is not None:
                    raise RuntimeError(f"expected-live child {child.pid} exited {child.returncode}")

        def spawn(self, name, command, cwd=None, live=False):
            self.serial += 1
            if cwd is None:
                cwd = self.root / f"cwd-{name}-{self.serial}"
                cwd.mkdir(mode=0o700)
            log_path = self.root / f"{self.serial:03d}-{name}.log"
            log = log_path.open("wb")
            self.logs.append(log)
            env = os.environ.copy()
            for key in ("HELIOSDB_NANO_READY_FILE", "HELIOSDB_PRIMARY_PG_PORT"):
                env.pop(key, None)
            env["RUST_LOG"] = "info"
            with helper.own_spawn():
                child = subprocess.Popen(command, cwd=cwd, env=env, stdin=subprocess.DEVNULL,
                                         stdout=log, stderr=subprocess.STDOUT)
                self.children.append(child)
                if live:
                    self.live.add(child)
            helper.emit(self.root / "processes.jsonl", {"pid": child.pid, "argv": command,
                        "cwd": str(cwd), "log": str(log_path), "expected_live": live})
            return child, log_path

        def finish(self, child, timeout=25, expected=None):
            try:
                code = child.wait(timeout=timeout)
            except subprocess.TimeoutExpired as error:
                raise RuntimeError(f"owned child {child.pid} did not finish within {timeout}s") from error
            self.live.discard(child)
            if expected is not None:
                require(code == expected, f"owned child {child.pid} exit {code}, expected {expected}")
            return code

        def stop_clean(self, child):
            require(child.poll() is None, f"owned child {child.pid} stopped before graceful shutdown")
            child.terminate()
            self.finish(child, timeout=30, expected=0)

        def source(self, token, export_dir, data_dir, generation):
            pg, native, physical = self.ports(3)
            for reservation in list(self.reserved):
                if reservation.getsockname()[1] in (pg, native, physical):
                    reservation.close()
                    self.reserved.remove(reservation)
            cwd = self.root / f"source-cwd-{generation}"
            cwd.mkdir(mode=0o700)
            command = [str(self.binary), "start", "--data-dir", str(data_dir),
                       "--listen", "127.0.0.1", "--port", str(pg), "--http-port", "0",
                       "--auth", "password", "--password", helper.PASSWORD,
                       "--replication-role", "primary", "--replication-port", str(native),
                       "--node-id", str(uuid.uuid4()), "--physical-replication=true",
                       "--physical-replication-listen", f"127.0.0.1:{physical}",
                       "--physical-replication-token-file", str(token), "--physical-export-dir", str(export_dir),
                       "--physical-max-sessions", "2", "--physical-max-total-bytes", "1073741824",
                       "--physical-max-file-bytes", "536870912", "--physical-max-chunk-bytes", "65536",
                       "--physical-frame-timeout-secs", "5", "--physical-operation-timeout-secs", "15"]
            child, log = self.spawn(f"source-{generation}", command, cwd=cwd, live=True)
            self.ready(child, log, pg)
            self.wait_native_owner(child, native)
            self.wait_native_owner(child, physical)
            history = json.loads((data_dir / "NANO-PRIMARY-HISTORY.json").read_text())["history_id"]
            wire = helper.Wire(self, child, pg, f"source-{generation}")
            return child, wire, physical, history

        def resync(self, name, port, token, target, history, resume=False, once=False, live=False):
            command = [str(self.binary), "resync", "--primary-host", f"127.0.0.1:{port}",
                       "--data-dir", str(target), "--physical-replication-token-file", str(token),
                       "--expected-history", history, "--physical-frame-timeout-secs", "5",
                       "--physical-operation-timeout-secs", "15", "--poll-interval-ms", "50",
                       "--reconnect-delay-ms", "100", "--max-reconnects", "2"]
            if resume:
                command.append("--resume-existing")
            if once:
                command.append("--once")
            return self.spawn(name, command, live=live)

        def inspect(self, name, path, history, receiver):
            before = census(path)
            log_dir = self.root / f"inspector-logs-{name}"
            log_dir.mkdir(mode=0o700)
            command = [str(self.inspector), "--path", str(path), "--history", history, "--log-dir", str(log_dir)]
            if receiver:
                command.append("--receiver")
            child, log = self.spawn(f"inspect-{name}", command)
            self.finish(child, expected=0)
            after = census(path)
            require(before == after, f"read-only inspector modified {path}")
            lines = log.read_text().splitlines()
            parsed = [json.loads(line) for line in lines if line.startswith('{')]
            require(len(parsed) == 1, f"inspector did not emit exactly one JSON record: {log}")
            helper.emit(self.root / "inspections.jsonl", {"name": name, "before": before,
                        "after": after, "result": parsed[0]})
            return parsed[0]
    return Fixture


def sql(wire, statement):
    result = wire.simple(statement)
    if result["errors"]:
        raise RuntimeError(f"source SQL failed: {statement}: {result}")
    return result


def source_rows(wire, expected):
    response = sql(wire, "SELECT id, note FROM resync_cli_probe ORDER BY id")
    require(response["rows"] == expected, f"committed source rows differ: {response}")


def await_raw_progress(fixture, child, log, target, offset=0, minimum=0):
    """Human progress schedules shutdown only; closed inspector establishes proof."""
    deadline = time.monotonic() + 40
    while time.monotonic() < deadline:
        fixture.alive()
        text = log.read_text(errors="replace")[offset:]
        require("Server ready!" not in text, "resync falsely advertised SQL server readiness")
        sequences = [int(value) for value in re.findall(r"Physical transfer: CaughtUp[^\n]*source_sequence: (\d+)", text)]
        if (target / "NANO-PHYSICAL-REPLICA.json").is_file() and len(sequences) >= 3 and sequences[-1] >= minimum:
            listeners = owned_listeners(child)
            require(not listeners, f"non-serving receiver owns listening sockets: {listeners}")
            return sequences[-1]
        time.sleep(0.05)
    raise RuntimeError(f"owned receiver raw progress timeout; see {log}")


def compare_closed(fixture, source_dir, receiver_dir, history, phase, previous=None):
    source = fixture.inspect(f"source-{phase}", source_dir, history, False)
    receiver = fixture.inspect(f"receiver-{phase}", receiver_dir, history, True)
    require(source["records"] == receiver["records"] and source["record_digest"] == receiver["record_digest"],
            f"closed raw row-prefix mismatch in phase {phase}")
    require(len(receiver["records"]) == 3, f"unexpected physical row count in phase {phase}")
    cursor = receiver["cursor"]
    require(cursor["history"] == history and cursor["snapshot_sequence"] > 0
            and cursor["source_sequence"] <= source["latest_sequence"], "receiver cursor/source relationship invalid")
    marker = json.loads((receiver_dir / "NANO-PHYSICAL-REPLICA.json").read_text())
    if previous:
        require(cursor["source_sequence"] > previous["cursor"]["source_sequence"], "Resume did not advance durable source cursor")
        require(cursor["snapshot_sequence"] == previous["cursor"]["snapshot_sequence"], "Resume replaced original snapshot cut")
        require(marker["snapshot_id"] == previous["snapshot_id"], "Resume silently installed a new snapshot")
    receiver["snapshot_id"] = marker["snapshot_id"]
    return receiver


def run(helper, root, binary, inspector):
    fixture = make_fixture_class(helper)(root / "fixture", binary, inspector)
    result = {"status": "inconclusive", "checks": [], "scope": "NON-SERVING closed raw row-prefix/history/cursor proof only"}
    try:
        destinations = fixture.root / "destinations"
        destinations.mkdir(mode=0o700)
        export = fixture.root / "exports"
        export.mkdir(mode=0o700)
        source_dir = fixture.root / "source-data"
        receiver_dir = destinations / "receiver"
        token, bad_token = fixture.root / "token", fixture.root / "bad-token"
        token.write_bytes(os.urandom(32))
        bad_token.write_bytes(bytes(value ^ 0xA5 for value in token.read_bytes()))
        token.chmod(0o600)
        bad_token.chmod(0o600)
        primary, wire, port, history = fixture.source(token, export, source_dir, 1)
        sql(wire, "CREATE TABLE resync_cli_probe (id INT PRIMARY KEY, note TEXT)")
        sql(wire, "BEGIN")
        sql(wire, "INSERT INTO resync_cli_probe VALUES (1, 'seed'), (10, 'snapshot-only')")
        sql(wire, "COMMIT")
        source_rows(wire, [["1", "seed"], ["10", "snapshot-only"]])
        # Both failures happen before staging is created; exact destination-parent
        # census excludes logs/cwds, which are separately owned outside this root.
        for name, supplied_token, supplied_history in [
                ("bad-auth", bad_token, history), ("bad-history", token, str(uuid.uuid4()))]:
            before = census(destinations, allow_empty=True)
            child, log = fixture.resync(name, port, supplied_token, destinations / name, supplied_history, once=True)
            require(fixture.finish(child) != 0, f"{name} unexpectedly succeeded")
            require(census(destinations, allow_empty=True) == before, f"{name} changed/adopted a destination")
            require("Server ready!" not in log.read_text(errors="replace"), f"{name} advertised serving readiness")
            result["checks"].append(f"{name}: nonzero exit, no destination adoption")
        existing = destinations / "existing"
        existing.mkdir(mode=0o700)
        (existing / "preserve.me").write_bytes(b"pre-existing data must survive")
        before = census(destinations, allow_empty=True)
        child, _ = fixture.resync("preserve-existing", port, token, existing, history, once=True)
        require(fixture.finish(child) != 0 and census(destinations, allow_empty=True) == before, "fresh resync changed existing destination")
        result["checks"].append("fresh existing destination refused byte-for-byte")
        follow, follow_log = fixture.resync("follow", port, token, receiver_dir, history, live=True)
        first_sequence = await_raw_progress(fixture, follow, follow_log, receiver_dir)
        sql(wire, "BEGIN")
        sql(wire, "UPDATE resync_cli_probe SET note='followed' WHERE id=1")
        sql(wire, "INSERT INTO resync_cli_probe VALUES (2, 'stream-only')")
        sql(wire, "COMMIT")
        source_rows(wire, [["1", "followed"], ["2", "stream-only"], ["10", "snapshot-only"]])
        offset = len(follow_log.read_text(errors="replace"))
        await_raw_progress(fixture, follow, follow_log, receiver_dir, offset, first_sequence + 1)
        fixture.stop_clean(follow)
        require("Physical state retained closed" in follow_log.read_text(), "Follow did not report a closed retained state")
        wire.sock.close()
        fixture.stop_clean(primary)
        phase1 = compare_closed(fixture, source_dir, receiver_dir, history, "follow")
        result["checks"].append("snapshot-only seed plus later committed UPDATE/INSERT match closed raw prefix")
        result["phase1"] = phase1
        primary, wire, port, history2 = fixture.source(token, export, source_dir, 2)
        require(history2 == history, "ordinary primary restart changed dataset history")
        source_rows(wire, [["1", "followed"], ["2", "stream-only"], ["10", "snapshot-only"]])
        before = census(receiver_dir)
        child, _ = fixture.resync("bad-resume-history", port, token, receiver_dir, str(uuid.uuid4()), resume=True, once=True)
        require(fixture.finish(child) != 0 and census(receiver_dir) == before, "wrong-history Resume modified retained receiver")
        follow, follow_log = fixture.resync("resume-follow", port, token, receiver_dir, history, resume=True, live=True)
        sequence = await_raw_progress(fixture, follow, follow_log, receiver_dir, minimum=phase1["cursor"]["source_sequence"])
        sql(wire, "BEGIN")
        sql(wire, "DELETE FROM resync_cli_probe WHERE id=1")
        sql(wire, "INSERT INTO resync_cli_probe VALUES (3, 'resumed-only')")
        sql(wire, "COMMIT")
        source_rows(wire, [["2", "stream-only"], ["3", "resumed-only"], ["10", "snapshot-only"]])
        offset = len(follow_log.read_text(errors="replace"))
        await_raw_progress(fixture, follow, follow_log, receiver_dir, offset, sequence + 1)
        fixture.stop_clean(follow)
        wire.sock.close()
        fixture.stop_clean(primary)
        phase2 = compare_closed(fixture, source_dir, receiver_dir, history, "resume", previous=phase1)
        result["phase2"] = phase2
        result["checks"].append("restart Resume preserves history/snapshot, advances cursor, copies committed DELETE/INSERT")
        # Ordinary Nano startup must reject the physical marker before recovery.
        before = census(receiver_dir)
        pg, = fixture.ports(1)
        for reservation in list(fixture.reserved):
            if reservation.getsockname()[1] == pg:
                reservation.close()
                fixture.reserved.remove(reservation)
        child, log = fixture.spawn("no-sql-publication", [str(binary), "start", "--data-dir", str(receiver_dir),
                "--listen", "127.0.0.1", "--port", str(pg), "--http-port", "0", "--replication-role", "standalone"])
        require(fixture.finish(child) != 0, "ordinary SQL startup adopted physical receiver")
        refusal = log.read_text(errors="replace")
        require("physical replica requires" in refusal and "Server ready!" not in refusal and census(receiver_dir) == before,
                "ordinary startup lacked marker refusal, advertised ready, or changed receiver bytes")
        result["checks"].append("normal SQL start refuses physical receiver without mutation/readiness")
        result["status"] = "pass"
    except AssertionError as error:
        result.update(status="regression_failed", error=str(error), traceback=traceback.format_exc())
    except (Exception, KeyboardInterrupt) as error:
        result.update(status="inconclusive", error=repr(error), traceback=traceback.format_exc())
    finally:
        try:
            fixture.close()
        except BaseException as error:
            result.update(status="inconclusive", cleanup_error=repr(error))
        (fixture.root / "resync-result.json").write_text(json.dumps(result, indent=2) + "\n")
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--inspector", type=Path, required=True)
    parser.add_argument("--output-dir", type=Path, required=True)
    parser.add_argument("--helper", type=Path, default=Path(__file__).resolve().parent.parent / "probe_standby_wire_writes.py")
    args = parser.parse_args()
    binary, inspector, helper_path = (path.resolve(strict=True) for path in (args.binary, args.inspector, args.helper))
    script = Path(__file__).resolve(strict=True)
    paths = (binary, inspector, helper_path, script)
    before = {str(path): digest(path) for path in paths}
    helper = load_helper(helper_path)
    args.output_dir.mkdir(parents=True, exist_ok=True)
    root = Path(tempfile.mkdtemp(prefix="cli-raw-resync-", dir=args.output_dir.resolve()))
    result = {"identities_before": before, "fixture_root": str(root), "scope": "NON-SERVING raw CLI transfer only"}
    def interrupted(number, _frame):
        raise InterruptedError(f"interrupted by signal {number}")
    for sig in (signal.SIGINT, signal.SIGTERM):
        signal.signal(sig, interrupted)
    try:
        result["regression"] = run(helper, root, binary, inspector)
    finally:
        try:
            result["identities_after"] = {str(path): digest(path) for path in paths}
            result["identity_unchanged"] = result["identities_after"] == before
        except Exception as error:
            result.update(identity_unchanged=False, identity_error=repr(error))
        (root / "result.json").write_text(json.dumps(result, indent=2) + "\n")
        print(root, flush=True)
    if not result["identity_unchanged"] or result["regression"]["status"] == "inconclusive":
        return 2
    return 0 if result["regression"]["status"] == "pass" else 1


if __name__ == "__main__":
    try:
        code = main()
    except (Exception, KeyboardInterrupt) as error:
        print(f"inconclusive setup: {error!r}", flush=True)
        code = 2
    raise SystemExit(code)
