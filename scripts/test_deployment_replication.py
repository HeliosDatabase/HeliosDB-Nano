#!/usr/bin/env python3
"""Bounded live Nano replication checks using private fixtures and exact child PIDs.

Run under the shared-host validation lock. Retains logs, data, SQL observations,
binary identity and JSON results below --output-dir; never reuses user data.
"""

import argparse
import hashlib
import json
import os
from pathlib import Path
import select
import socket
import subprocess
import tempfile
import time
import uuid

import psycopg2
from psycopg2 import extensions

# Public, fixture-only credential; never used with an existing server or dataset.
FIXTURE_PASSWORD = "nano-deployment-fixture-only"


class Fixture:
    def __init__(self, root, binary, auth_mode="password"):
        self.root = root
        self.root.mkdir()
        self.binary = binary
        self.auth_mode = auth_mode
        self.children = []
        self.logs = []
        self.reservations = []

    def ports(self, count):
        ports = []
        for _ in range(count):
            listener = socket.socket()
            listener.bind(("127.0.0.1", 0))
            self.reservations.append(listener)
            ports.append(listener.getsockname()[1])
        return ports

    def release_port(self, port):
        for listener in list(self.reservations):
            if listener.getsockname()[1] == port:
                listener.close()
                self.reservations.remove(listener)

    def start(self, name, pg_port, repl_port, role, primary=None, primary_pg=None, node_id=None):
        cwd = self.root / name
        cwd.mkdir()
        log_path = self.root / f"{name}.log"
        log = log_path.open("wb")
        self.logs.append(log)
        args = [str(self.binary), "start", "--data-dir", str(cwd / "db"),
                "--listen", "127.0.0.1", "--port", str(pg_port), "--http-port", "0",
                "--auth", self.auth_mode, "--replication-role", role,
                "--replication-port", str(repl_port), "--sync-mode", "async"]
        if self.auth_mode == "password":
            args += ["--password", FIXTURE_PASSWORD]
        if primary is not None:
            args += ["--primary-host", f"127.0.0.1:{primary}"]
        if node_id is not None:
            args += ["--node-id", node_id]
        env = os.environ.copy()
        env.pop("HELIOSDB_NANO_READY_FILE", None)
        env["RUST_LOG"] = "info"
        if primary_pg is not None:
            env["HELIOSDB_PRIMARY_PG_PORT"] = str(primary_pg)
        self.release_port(pg_port)
        self.release_port(repl_port)
        process = subprocess.Popen(args, cwd=cwd, env=env, stdin=subprocess.DEVNULL,
                                   stdout=log, stderr=subprocess.STDOUT)
        self.children.append(process)
        return process, log_path

    def alive(self):
        for child in self.children:
            if child.poll() is not None:
                raise RuntimeError(f"owned child {child.pid} exited with status {child.returncode}")

    def close(self):
        for child in reversed(self.children):
            if child.poll() is None:
                child.terminate()
            try:
                child.wait(timeout=5)
            except subprocess.TimeoutExpired:
                child.kill()
                child.wait(timeout=5)
        for listener in self.reservations:
            listener.close()
        for log in self.logs:
            log.close()


def poll_connection(connection, fixture, deadline):
    while True:
        fixture.alive()
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            raise TimeoutError("PostgreSQL operation exceeded its deadline")
        state = connection.poll()
        if state == extensions.POLL_OK:
            return
        readable = [connection] if state == extensions.POLL_READ else []
        writable = [connection] if state == extensions.POLL_WRITE else []
        if not readable and not writable:
            raise RuntimeError(f"unexpected libpq poll state: {state}")
        select.select(readable, writable, [], min(remaining, 0.2))


def connect(port, fixture, seconds=15):
    deadline = time.monotonic() + seconds
    last_error = None
    while time.monotonic() < deadline:
        fixture.alive()
        connection = None
        try:
            connection = psycopg2.connect(host="127.0.0.1", port=port, user="postgres",
                                          dbname="heliosdb", async_=True,
                                          password=FIXTURE_PASSWORD if fixture.auth_mode == "password" else "")
            poll_connection(connection, fixture, min(deadline, time.monotonic() + 3))
            return connection
        except (psycopg2.Error, TimeoutError) as error:
            last_error = repr(error)
            if connection is not None:
                connection.close()
            time.sleep(0.1)
    raise TimeoutError(f"PostgreSQL port {port} did not become ready: {last_error}")


def query(connection, sql, fixture, observations):
    record = {"sql": sql}
    observations.append(record)
    try:
        with connection.cursor() as cursor:
            cursor.execute(sql)
            poll_connection(connection, fixture, time.monotonic() + 5)
            rows = [list(row) for row in cursor.fetchall()] if cursor.description else None
            record.update(status=cursor.statusmessage, rows=rows)
            return rows
    except Exception as error:
        record["error"] = repr(error)
        raise


def wait_rows(connection, expected, fixture, observations, seconds=8):
    deadline = time.monotonic() + seconds
    last_rows, last_error = None, None
    while time.monotonic() < deadline:
        try:
            last_rows = query(connection, "SELECT id, note FROM campaign_replication ORDER BY id", fixture,
                              observations)
            last_error = None
            if last_rows == expected:
                return last_rows
        except psycopg2.Error as error:
            last_error = repr(error)
        time.sleep(0.2)
    raise AssertionError(f"expected rows {expected!r}, observed {last_rows!r}; last error {last_error}")



def warm_standby_rows(connection, expected, fixture, observations):
    """Admit the original SELECT to this standby connection's result cache."""
    for attempt in range(1, 6):
        actual = query(connection, "SELECT id, note FROM campaign_replication ORDER BY id", fixture,
                       observations)
        observations[-1].update(phase="standby_pre_barrier_cache_warm", attempt=attempt)
        if actual != expected:
            raise AssertionError(f"standby warmup {attempt} expected {expected!r}, observed {actual!r}")


def assert_query_rows(connection, sql, expected, fixture, observations):
    actual = query(connection, sql, fixture, observations)
    if actual != expected:
        raise AssertionError(f"{sql}: expected {expected!r}, observed {actual!r}")
    return actual

def wait_log(log_path, required, fixture, seconds=15):
    deadline = time.monotonic() + seconds
    while time.monotonic() < deadline:
        fixture.alive()
        content = log_path.read_text(errors="replace")
        if all(fragment in content for fragment in required):
            return
        time.sleep(0.1)
    raise AssertionError(f"log {log_path} did not contain {required!r}")


def check(result, name, operation):
    started = time.monotonic()
    try:
        detail = operation()
        result["checks"].append({"name": name, "status": "pass", "detail": detail,
                                 "elapsed_seconds": round(time.monotonic() - started, 3)})
        return True
    except Exception as error:
        result["checks"].append({"name": name, "status": "fail", "error": repr(error),
                                 "elapsed_seconds": round(time.monotonic() - started, 3)})
        return False


def live_replication(root, binary, result, auth_mode="password"):
    fixture = Fixture(root / "live", binary, auth_mode)
    connections = []
    observations = result["sql"]
    try:
        primary_pg, primary_repl, standby_pg, standby_repl = fixture.ports(4)
        result["ports"] = {"primary_pg": primary_pg, "primary_repl": primary_repl,
                           "standby_pg": standby_pg, "standby_repl": standby_repl}
        _, primary_log = fixture.start("primary", primary_pg, primary_repl, "primary")
        primary = connect(primary_pg, fixture)
        connections.append(primary)
        standby_id = str(uuid.uuid4())
        _, standby_log = fixture.start("standby", standby_pg, standby_repl, "standby",
                                       primary=primary_repl, primary_pg=primary_pg, node_id=standby_id)
        standby = connect(standby_pg, fixture)
        connections.append(standby)
        wait_log(primary_log, [f"Standby {standby_id} registered"], fixture)
        wait_log(standby_log, ["Connected to primary"], fixture)
        result["checks"].append({"name": "native_handshake_registration", "status": "pass",
                                 "standby_node_id": standby_id})
        query(primary, "CREATE TABLE campaign_replication (id INT PRIMARY KEY, note TEXT)", fixture,
              observations)
        check(result, "schema_reaches_standby", lambda: wait_rows(standby, [], fixture, observations))

        def commit_rows():
            query(primary, "BEGIN", fixture, observations)
            query(primary, "INSERT INTO campaign_replication VALUES (1, 'committed-one')", fixture,
                  observations)
            query(primary, "INSERT INTO campaign_replication VALUES (2, 'committed-two')", fixture,
                  observations)
            query(primary, "COMMIT", fixture, observations)
            return wait_rows(primary, [[1, "committed-one"], [2, "committed-two"]], fixture, observations)

        check(result, "explicit_transaction_primary_commit", commit_rows)
        expected = [[1, "committed-one"], [2, "committed-two"]]
        check(result, "committed_exact_rows_on_standby",
              lambda: wait_rows(standby, expected, fixture, observations))

        def rollback_rows():
            query(primary, "BEGIN", fixture, observations)
            query(primary, "INSERT INTO campaign_replication VALUES (3, 'rolled-back')", fixture, observations)
            query(primary, "ROLLBACK", fixture, observations)
            return wait_rows(primary, expected, fixture, observations)

        check(result, "explicit_transaction_primary_rollback", rollback_rows)

        def stable_standby_rows():
            # Use the original standby connection and exact repeated SQL so the
            # later replay must invalidate an already-admitted cached result.
            warm_standby_rows(standby, expected, fixture, observations)
            # A later committed row is a stream barrier: do not accept an early
            # unchanged snapshot before the rolled-back entry could arrive.
            query(primary, "INSERT INTO campaign_replication VALUES (4, 'after-rollback')", fixture, observations)
            final_rows = expected + [[4, "after-rollback"]]
            wait_rows(standby, final_rows, fixture, observations)
            deadline = time.monotonic() + 2
            while time.monotonic() < deadline:
                actual = query(standby, "SELECT id, note FROM campaign_replication ORDER BY id", fixture,
                               observations)
                if actual != final_rows:
                    raise AssertionError(f"post-rollback rows changed: {actual!r}")
                time.sleep(0.2)
            return final_rows

        check(result, "rollback_excluded_after_later_commit", stable_standby_rows)
        # Fresh query shapes isolate index/count visibility from the warmed scan.
        # Run even if the preceding assertion failed: retain every failed check.
        check(result, "standby_primary_key_after_barrier", lambda: assert_query_rows(
            standby, "SELECT id, note FROM campaign_replication WHERE id = 4", [[4, "after-rollback"]],
            fixture, observations))
        check(result, "standby_count_after_barrier", lambda: assert_query_rows(
            standby, "SELECT COUNT(*) FROM campaign_replication", [[3]], fixture, observations))
    finally:
        for connection in connections:
            connection.close()
        fixture.close()


def wrong_endpoint(root, binary, result, auth_mode="password"):
    fixture = Fixture(root / "wrong-endpoint", binary, auth_mode)
    connections = []
    try:
        primary_pg, primary_repl, standby_pg, standby_repl = fixture.ports(4)
        fixture.start("primary", primary_pg, primary_repl, "primary")
        connections.append(connect(primary_pg, fixture))
        _, log_path = fixture.start("standby", standby_pg, standby_repl, "standby",
                                     primary=primary_pg, primary_pg=primary_pg)
        connections.append(connect(standby_pg, fixture))
        wait_log(log_path, ["Invalid magic", "45000000"], fixture)
        check(result, "wrong_pg_endpoint_actionable_diagnostic", lambda: wait_log(
            log_path, ["PostgreSQL ErrorResponse", "--primary-host", "--replication-port"], fixture, seconds=1))
        return {"observed": "native client rejected PostgreSQL framing", "log": str(log_path)}
    finally:
        for connection in connections:
            connection.close()
        fixture.close()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--output-dir", type=Path, required=True)
    parser.add_argument("--auth-mode", choices=("password", "trust"), default="password",
                        help="PostgreSQL auth for both isolated nodes (default: reported deployment's password auth)")
    args = parser.parse_args()
    binary = args.binary.resolve(strict=True)
    args.output_dir.mkdir(parents=True, exist_ok=True)
    root = Path(tempfile.mkdtemp(prefix="replication-", dir=args.output_dir.resolve()))
    result = {"binary": str(binary), "fixture_root": str(root), "auth_mode": args.auth_mode,
              "checks": [], "sql": []}
    try:
        with binary.open("rb") as stream:
            digest = hashlib.sha256()
            for chunk in iter(lambda: stream.read(1024 * 1024), b""):
                digest.update(chunk)
            result["binary_sha256"] = digest.hexdigest()
        version = subprocess.run([str(binary), "--version"], capture_output=True, text=True, timeout=10)
        result["version"] = {"stdout": version.stdout, "stderr": version.stderr, "returncode": version.returncode}
        check(result, "live_replication_fixture", lambda: live_replication(root, binary, result, args.auth_mode))
        check(result, "wrong_endpoint_fixture", lambda: wrong_endpoint(root, binary, result, args.auth_mode))
    except Exception as error:
        result["checks"].append({"name": "harness_setup", "status": "fail", "error": repr(error)})
    result["passed"] = bool(result["checks"]) and all(row["status"] == "pass" for row in result["checks"])
    report = root / "result.json"
    report.write_text(json.dumps(result, indent=2, default=str) + "\n")
    print(json.dumps({"report": str(report), "passed": result["passed"], "checks": result["checks"]}, indent=2))
    return 0 if result["passed"] else 1


if __name__ == "__main__":
    raise SystemExit(main())
