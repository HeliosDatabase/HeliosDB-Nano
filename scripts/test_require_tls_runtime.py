#!/usr/bin/env python3
"""Bounded real-daemon gate for PostgreSQL TCP --require-tls.

Run only with HELIOSDB_TLS_REGRESSION=1 and HELIOSDB_NANO_BIN set. Requires
psycopg2 and openssl. It owns and reaps every child and never uses fixed ports.
"""

import os
import pathlib
import signal
import socket
import subprocess
import sys
import tempfile
import time

import psycopg2


def free_port():
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


def owned_listen(pid, port):
    owned = set()
    for entry in pathlib.Path(f"/proc/{pid}/fd").iterdir():
        try:
            target = os.readlink(entry)
        except OSError:
            continue
        if target.startswith("socket:[") and target.endswith("]"):
            owned.add(target[8:-1])
    for name in ("tcp", "tcp6"):
        try:
            lines = pathlib.Path(f"/proc/{pid}/net/{name}").read_text().splitlines()[1:]
        except OSError:
            continue
        for line in lines:
            fields = line.split()
            if len(fields) < 10 or fields[3] != "0A":
                continue
            address, hex_port = fields[1].split(":", 1)
            if int(hex_port, 16) != port:
                continue
            # Kernel layouts differ in trailing bookkeeping columns; require
            # the inode to be one of the child's descriptors rather than trust
            # a positional field from a namespace-wide table.
            if any(value in owned for value in fields[9:]):
                return address
    return None


def wait_owned(proc, port):
    deadline = time.monotonic() + 30
    while time.monotonic() < deadline:
        if proc.poll() is not None:
            raise RuntimeError(f"daemon exited with {proc.returncode}")
        if owned_listen(proc.pid, port):
            return
        time.sleep(0.1)
    raise RuntimeError("daemon did not expose an owned PostgreSQL listener")


def start(binary, data, port, cert=None, key=None, require=False):
    args = [binary, "start", "--memory", "--data-dir", str(data), "--listen", "127.0.0.1", "--port", str(port), "--http-port", "0", "--auth", "trust"]
    if cert:
        args += ["--tls-cert", str(cert), "--tls-key", str(key)]
    if require:
        args += ["--require-tls"]
    return subprocess.Popen(args, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.PIPE, env={k: v for k, v in os.environ.items() if k not in {"HELIOSDB_PASSWORD", "HELIOSDB_PASSWORD_FILE", "HELIOSDB_MCP_TOKEN", "MCP_TOKEN"}})


def query(port, sslmode):
    with psycopg2.connect(host="127.0.0.1", port=port, user="postgres", dbname="postgres", sslmode=sslmode, connect_timeout=5) as conn:
        with conn.cursor() as cur:
            cur.execute("SELECT 1")
            assert cur.fetchone()[0] == 1


def main():
    if os.environ.get("HELIOSDB_TLS_REGRESSION") != "1":
        print("SKIP: set HELIOSDB_TLS_REGRESSION=1", file=sys.stderr)
        return 3
    binary = os.environ.get("HELIOSDB_NANO_BIN")
    if not binary:
        raise SystemExit("HELIOSDB_NANO_BIN is required")
    with tempfile.TemporaryDirectory(prefix="nano-require-tls-certs-") as root:
        root = pathlib.Path(root)
        cert, key = root / "server.crt", root / "server.key"
        subprocess.run(["openssl", "req", "-x509", "-newkey", "rsa:2048", "-nodes", "-keyout", str(key), "-out", str(cert), "-subj", "/CN=localhost", "-days", "1"], check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        for require in (True, False):
            port = free_port()
            with tempfile.TemporaryDirectory(prefix="nano-require-tls-data-") as data:
                proc = start(binary, pathlib.Path(data), port, cert, key, require)
                try:
                    wait_owned(proc, port)
                    query(port, "require")
                    if require:
                        try:
                            query(port, "disable")
                        except Exception:
                            query(port, "require")
                        else:
                            raise AssertionError("plaintext PostgreSQL connection succeeded under --require-tls")
                    else:
                        query(port, "disable")
                finally:
                    if proc.poll() is None:
                        proc.send_signal(signal.SIGTERM)
                    proc.wait(timeout=10)
        print("PASS: required-TLS and Prefer real-daemon checks")


if __name__ == "__main__":
    main()
