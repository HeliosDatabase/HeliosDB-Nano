#!/usr/bin/env python3
"""DRAFT: independent prepared-INSERT/COPY standby write probes; run under fleet lock.

Creates only private retained fixtures beneath --output-dir. Does not reuse data,
kill by name, daemonize, or remove directories. JSONL stores every wire frame.
No claim that a returned command tag/error establishes the final database state.
"""
import argparse
import contextlib
import hashlib
import json
import os
from pathlib import Path
import signal
import socket
import struct
import subprocess
import tempfile
import time
import traceback
import uuid

PASSWORD = "nano-deployment-fixture-only"
TIMEOUT = 5


def emit(path, obj):
    with path.open("a") as out:
        out.write(json.dumps(obj, sort_keys=True) + "\n")


@contextlib.contextmanager
def own_spawn():
    """Defer cancellation until the returned child handle is recorded by owner."""
    pending = []
    previous = {s: signal.getsignal(s) for s in (signal.SIGINT, signal.SIGTERM)}
    for s in previous:
        signal.signal(s, lambda number, _frame: pending.append(number))
    try:
        yield
    finally:
        for s, handler in previous.items():
            signal.signal(s, handler)
        if pending:
            raise InterruptedError(f"cancelled by signal {pending[0]} after child ownership recorded")


class Fixture:
    def __init__(self, root, binary):
        self.root, self.binary = root, binary
        root.mkdir()
        self.children, self.reserved, self.logs, self.connections = [], [], [], []
        self.wire = root / "wire.jsonl"

    def ports(self, n):
        result = []
        for _ in range(n):
            s = socket.socket()
            s.bind(("127.0.0.1", 0))
            self.reserved.append(s)
            result.append(s.getsockname()[1])
        return result

    def start(self, name, pg, native, role, source_native=None, source_pg=None):
        cwd = self.root / name
        cwd.mkdir()
        output = self.root / f"{name}.log"
        log = output.open("wb")
        self.logs.append(log)
        command = [str(self.binary), "start", "--data-dir", str(cwd / "data"),
                   "--listen", "127.0.0.1", "--port", str(pg), "--http-port", "0",
                   "--auth", "password", "--password", PASSWORD,
                   "--replication-role", role, "--replication-port", str(native),
                   "--sync-mode", "async", "--node-id", str(uuid.uuid4())]
        if source_native is not None:
            command += ["--primary-host", f"127.0.0.1:{source_native}"]
        env = os.environ.copy()
        env.pop("HELIOSDB_NANO_READY_FILE", None)
        env.pop("HELIOSDB_PRIMARY_PG_PORT", None)
        env["RUST_LOG"] = "info"
        if source_pg is not None:
            env["HELIOSDB_PRIMARY_PG_PORT"] = str(source_pg)
        for s in list(self.reserved):
            if s.getsockname()[1] in (pg, native):
                s.close()
                self.reserved.remove(s)
        with own_spawn():
            child = subprocess.Popen(command, cwd=cwd, env=env, stdin=subprocess.DEVNULL,
                                     stdout=log, stderr=subprocess.STDOUT)
            self.children.append(child)
        emit(self.root / "processes.jsonl", {"pid": child.pid, "argv": command, "cwd": str(cwd)})
        return child, output

    def alive(self):
        for child in self.children:
            if child.poll() is not None:
                raise RuntimeError(f"owned process {child.pid} exited {child.returncode}")

    @staticmethod
    def owns_listener(child, port):
        # Popen's unreaped live child cannot have its PID reused. Check that its
        # own fd set includes this loopback listening socket before issuing SQL.
        if child.poll() is not None:
            return False
        inodes = set()
        for fd in Path(f"/proc/{child.pid}/fd").iterdir():
            try:
                target = os.readlink(fd)
            except FileNotFoundError:
                continue
            if target.startswith("socket:["):
                inodes.add(target[8:-1])
        for line in Path("/proc/net/tcp").read_text().splitlines()[1:]:
            fields = line.split()
            if (fields[1] == f"0100007F:{port:04X}" and fields[3] == "0A"
                    and fields[9] in inodes):
                return True
        return False

    def ready(self, child, log, port, standby=False):
        deadline = time.monotonic() + 40
        while time.monotonic() < deadline:
            self.alive()
            text = log.read_text(errors="replace")
            if ("Server ready!" in text and (not standby or "Connected to primary" in text)
                    and self.owns_listener(child, port)):
                return
            time.sleep(0.1)
        raise TimeoutError(f"owned listener {port} did not become ready; see {log}")

    def wait_native_owner(self, child, port):
        deadline = time.monotonic() + TIMEOUT
        while time.monotonic() < deadline:
            self.alive()
            if self.owns_listener(child, port):
                return
            time.sleep(0.05)
        raise RuntimeError(f"refusing standby dial to unowned native listener {port}")

    def close(self):
        previous = {s: signal.getsignal(s) for s in (signal.SIGINT, signal.SIGTERM)}
        for s in previous:
            signal.signal(s, signal.SIG_IGN)
        errors = []
        try:
            for connection in self.connections:
                try:
                    connection.sock.close()
                except Exception as error:
                    errors.append(f"connection close: {error!r}")
            for child in reversed(self.children):
                try:
                    if child.poll() is None:
                        child.terminate()
                    try:
                        child.wait(timeout=5)
                    except subprocess.TimeoutExpired:
                        child.kill()
                        child.wait(timeout=5)
                except Exception as error:
                    errors.append(repr(error))
            for s in self.reserved:
                try:
                    s.close()
                except Exception as error:
                    errors.append(f"reservation close: {error!r}")
            for log in self.logs:
                try:
                    log.close()
                except Exception as error:
                    errors.append(f"log close: {error!r}")
            emit(self.root / "cleanup.jsonl", {"errors": errors,
                 "children": [{"pid": c.pid, "returncode": c.poll()} for c in self.children]})
        finally:
            for s, handler in previous.items():
                signal.signal(s, handler)
        if errors:
            raise RuntimeError(f"owned cleanup incomplete: {errors}")


class Wire:
    def __init__(self, fixture, child, port, label):
        self.fixture, self.label = fixture, label
        fixture.alive()
        if not fixture.owns_listener(child, port):
            raise RuntimeError(f"refusing SQL to unowned listener {port}")
        self.sock = socket.create_connection(("127.0.0.1", port), TIMEOUT)
        fixture.connections.append(self)
        self.serial = 0
        payload = struct.pack("!I", 196608) + b"user\0postgres\0database\0heliosdb\0\0"
        self.sock.sendall(struct.pack("!I", len(payload) + 4) + payload)
        emit(fixture.wire, {"connection": label, "direction": "send", "type": "startup", "payload_hex": payload.hex()})
        deadline = time.monotonic() + TIMEOUT
        errors = []
        for _ in range(256):
            typ, data = self.recv(deadline)
            if typ == b"R":
                code = struct.unpack("!I", data[:4])[0]
                if code == 3:
                    self.send(b"p", PASSWORD.encode() + b"\0")
                elif code != 0:
                    raise RuntimeError(f"unsupported auth method {code}")
            elif typ == b"E":
                errors.append(self.error(data))
            elif typ == b"Z":
                if errors:
                    raise RuntimeError(f"startup errors {errors}")
                return
        raise RuntimeError("startup frame limit")

    def send(self, typ, payload=b""):
        self.sock.settimeout(TIMEOUT)
        self.sock.sendall(typ + struct.pack("!I", len(payload) + 4) + payload)
        emit(self.fixture.wire, {"connection": self.label, "direction": "send", "type": typ.decode(), "payload_hex": payload.hex()})

    def exact(self, size, deadline):
        chunks = bytearray()
        while len(chunks) < size:
            self.fixture.alive()
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise TimeoutError("wire operation deadline")
            self.sock.settimeout(min(remaining, TIMEOUT))
            chunk = self.sock.recv(size - len(chunks))
            if not chunk:
                raise EOFError("wire peer closed")
            chunks.extend(chunk)
        return bytes(chunks)

    def recv(self, deadline):
        header = self.exact(5, deadline)
        length = struct.unpack("!I", header[1:])[0]
        if not 4 <= length <= 4 * 1024 * 1024:
            raise ValueError(f"invalid frame size {length}")
        data = self.exact(length - 4, deadline)
        emit(self.fixture.wire, {"connection": self.label, "direction": "recv", "type": header[:1].decode(errors="replace"), "payload_hex": data.hex()})
        return header[:1], data

    @staticmethod
    def error(data):
        result = {}
        for field in data.split(b"\0"):
            if field:
                result[field[:1].decode(errors="replace")] = field[1:].decode(errors="replace")
        return result

    @staticmethod
    def row(data):
        count = struct.unpack_from("!H", data)[0]
        offset, row = 2, []
        for _ in range(count):
            size = struct.unpack_from("!i", data, offset)[0]
            offset += 4
            if size == -1:
                row.append(None)
            elif size < 0 or offset + size > len(data):
                raise ValueError("invalid DataRow field")
            else:
                row.append(data[offset:offset + size].decode())
                offset += size
        if offset != len(data):
            raise ValueError("trailing DataRow bytes")
        return row

    def drain(self, copy_payload=None):
        result = {"rows": [], "errors": [], "tags": [], "copy_entered": False}
        deadline = time.monotonic() + TIMEOUT
        for _ in range(512):
            typ, data = self.recv(deadline)
            if typ == b"D":
                result["rows"].append(self.row(data))
            elif typ == b"E":
                result["errors"].append(self.error(data))
            elif typ == b"C":
                result["tags"].append(data.rstrip(b"\0").decode())
            elif typ == b"G":
                result["copy_entered"] = True
                if copy_payload is None:
                    self.send(b"f", b"unexpected COPY mode\0")
                else:
                    self.send(b"d", copy_payload)
                    self.send(b"c")
            elif typ == b"Z":
                result["ready_status"] = data.decode()
                return result
        raise RuntimeError("response frame limit")

    def simple(self, sql):
        self.send(b"Q", sql.encode() + b"\0")
        result = self.drain()
        emit(self.fixture.root / "sql.jsonl", {"connection": self.label, "sql": sql, "result": result})
        return result

    def write(self, mode, row_id, note):
        if mode == "extended":
            self.serial += 1
            statement = f"owned_stmt_{self.serial}".encode() + b"\0"
            portal = f"owned_portal_{self.serial}".encode() + b"\0"
            sql = b"INSERT INTO standby_write_probe (id, note) VALUES ($1, $2)\0"
            self.send(b"P", statement + sql + struct.pack("!HII", 2, 23, 25))
            parameters = [str(row_id).encode(), note.encode()]
            bind = portal + statement + struct.pack("!HH", 0, len(parameters))
            for value in parameters:
                bind += struct.pack("!I", len(value)) + value
            self.send(b"B", bind + struct.pack("!H", 0))
            self.send(b"E", portal + struct.pack("!I", 0))
            self.send(b"S")
            result = self.drain()
        elif mode == "copy":
            self.send(b"Q", b"COPY standby_write_probe (id, note) FROM STDIN\0")
            result = self.drain(copy_payload=f"{row_id}\t{note}\n".encode())
        else:
            raise ValueError(mode)
        emit(self.fixture.root / "sql.jsonl", {"connection": self.label, "mode": mode,
             "row": [row_id, note], "result": result})
        return result

    def rows(self):
        # Vary projection aliases so prior baseline result-cache invalidation
        # defects cannot alone make this fixture claim no row was written.
        self.serial += 1
        result = self.simple(f"SELECT id AS probe_id_{self.serial}, note AS probe_note_{self.serial} FROM standby_write_probe ORDER BY id")
        if result["errors"]:
            raise RuntimeError(f"read errors: {result['errors']}")
        return result["rows"]


def exact_rows(wire, expected, seconds=10):
    deadline = time.monotonic() + seconds
    last = None
    while time.monotonic() < deadline:
        try:
            last = wire.rows()
            if last == expected:
                return last
        except RuntimeError as error:
            last = repr(error)
        time.sleep(0.1)
    raise AssertionError(f"expected exact rows {expected}, observed {last}")


def run_mode(root, binary, mode):
    fixture = Fixture(root / mode, binary)
    outcome = {"mode": mode, "status": "inconclusive"}
    try:
        ppg, prepl, spg, srepl = fixture.ports(4)
        outcome["ports"] = {"primary_pg": ppg, "primary_native": prepl, "standby_pg": spg, "standby_native": srepl}
        primary, plog = fixture.start("primary", ppg, prepl, "primary")
        fixture.ready(primary, plog, ppg)
        fixture.wait_native_owner(primary, prepl)
        standby, slog = fixture.start("standby", spg, srepl, "standby", prepl, ppg)
        fixture.ready(standby, slog, spg, standby=True)
        source = Wire(fixture, primary, ppg, "primary")
        receiver = Wire(fixture, standby, spg, "standby")
        for sql in ["CREATE TABLE standby_write_probe (id INT PRIMARY KEY, note TEXT)",
                    "BEGIN", "INSERT INTO standby_write_probe VALUES (1, 'source-seed')", "COMMIT"]:
            response = source.simple(sql)
            if response["errors"]:
                raise RuntimeError(f"source setup failed: {response}")
        seed = [["1", "source-seed"]]
        exact_rows(source, seed)
        exact_rows(receiver, seed)
        outcome["initial_replication"] = "exact seed rows observed on both nodes"
        control = source.write(mode, 2, "source-control")
        outcome["source_control"] = control
        before = seed + [["2", "source-control"]]
        exact_rows(source, before)
        exact_rows(receiver, before)
        if control["errors"] or (mode == "copy" and not control["copy_entered"]):
            raise AssertionError("source protocol control reported errors; fixture is inconclusive")
        outcome["standby_attempt"] = receiver.write(mode, 99, "standby-local-probe")
        # Observe repeatedly for a bounded period to distinguish immediate local
        # mutation from forwarding and subsequent source-to-standby delivery.
        samples = []
        for _ in range(8):
            samples.append({"source": source.rows(), "standby": receiver.rows()})
            time.sleep(0.25)
        outcome["row_samples"] = samples
        extra = ["99", "standby-local-probe"]
        if any(extra in sample["source"] for sample in samples):
            outcome["status"] = "source_write_or_forwarding_observed"
        elif all(sample["source"] == before and sample["standby"] == before + [extra] for sample in samples):
            outcome["status"] = "standby_local_write_observed"
        elif all(sample["source"] == before and sample["standby"] == before for sample in samples):
            outcome["status"] = ("refused_without_observed_mutation" if outcome["standby_attempt"]["errors"]
                                 else "acknowledged_without_observed_mutation")
        else:
            outcome["status"] = "unexpected_rows_inconclusive"
    except InterruptedError as error:
        outcome["status"] = "interrupted"
        outcome["error"] = repr(error)
        raise
    except Exception as error:
        outcome["error"] = repr(error)
        outcome["traceback"] = traceback.format_exc()
    finally:
        try:
            fixture.close()
        except Exception as error:
            outcome["cleanup_error"] = repr(error)
            outcome["status"] = "cleanup_failed"
        (fixture.root / "result.json").write_text(json.dumps(outcome, indent=2) + "\n")
    return outcome


def digest(path):
    checksum = hashlib.sha256()
    with path.open("rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            checksum.update(chunk)
    return checksum.hexdigest()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--output-dir", type=Path, required=True)
    parser.add_argument("--mode", choices=["both", "extended", "copy"], default="both")
    args = parser.parse_args()
    binary = args.binary.resolve(strict=True)
    args.output_dir.mkdir(parents=True, exist_ok=True)
    root = Path(tempfile.mkdtemp(prefix="standby-wire-writes-", dir=args.output_dir.resolve()))
    script = Path(__file__).resolve(strict=True)
    result = {"binary": str(binary), "binary_sha256_before": digest(binary),
              "script": str(script), "script_sha256_before": digest(script),
              "fixture_root": str(root), "modes": [], "scope": "bounded wire mutation observation, not full read-only certification"}
    def interrupted(number, _frame):
        raise InterruptedError(f"cancelled by signal {number}")
    for s in (signal.SIGINT, signal.SIGTERM):
        signal.signal(s, interrupted)
    try:
        for mode in (["extended", "copy"] if args.mode == "both" else [args.mode]):
            result["modes"].append(run_mode(root, binary, mode))
    except InterruptedError as error:
        result["status"] = "interrupted_inconclusive"
        result["error"] = repr(error)
        result["modes"].append({"mode": mode, "status": "interrupted", "error": repr(error)})
    except Exception as error:
        result["status"] = "harness_error_inconclusive"
        result["error"] = repr(error)
        result["traceback"] = traceback.format_exc()
    finally:
        # Artifact identity is part of the evidence, including exceptional runs.
        # Missing/unreadable or changed artifacts invalidate all mode verdicts.
        try:
            result["binary_sha256_after"] = digest(binary)
            result["script_sha256_after"] = digest(script)
            result["artifact_identity_unchanged"] = (
                result["binary_sha256_before"] == result["binary_sha256_after"]
                and result["script_sha256_before"] == result["script_sha256_after"])
        except Exception as error:
            result["artifact_identity_unchanged"] = False
            result["artifact_identity_error"] = repr(error)
        if not result["artifact_identity_unchanged"]:
            result["status"] = "artifact_identity_changed_inconclusive"
        (root / "result.json").write_text(json.dumps(result, indent=2) + "\n")
        print(root, flush=True)
    # A successful probe execution means evidence is complete, never "safe".
    # Distinct nonzero statuses make observed bypass/inconclusive impossible to
    # confuse with a passing read-only guard regression.
    if not result["artifact_identity_unchanged"] or result.get("status") in (
            "interrupted_inconclusive", "harness_error_inconclusive"):
        return 2
    statuses = [entry["status"] for entry in result["modes"]]
    if any(s in ("standby_local_write_observed", "source_write_or_forwarding_observed") for s in statuses):
        return 1
    if len(statuses) != (2 if args.mode == "both" else 1) or any(s != "refused_without_observed_mutation" for s in statuses):
        return 2
    return 0


if __name__ == "__main__":
    try:
        exit_code = main()
    except (Exception, KeyboardInterrupt) as error:
        # Even failures before fixture/result initialization are inconclusive,
        # never the exit code reserved for an observed write bypass.
        import sys
        print(f"probe harness inconclusive: {error!r}", file=sys.stderr)
        exit_code = 2
    raise SystemExit(exit_code)
