#!/usr/bin/env python3
"""Real-binary acceptance gate for PostgreSQL TCP ``--require-tls`` (Issue 53).

What this proves
----------------
This harness launches the *actual* ``heliosdb-nano`` binary from an absolute
path and captures retained stdout/stderr evidence for every case. It covers:

* ``--require-tls`` with a SCRAM credential supplied only through a temporary
  password *file* (never on the harness command line): a plaintext PostgreSQL
  TCP client is refused and the server records the exact text ``SSL is required
  but no SSL request was received``; the *same* PID then completes a TLS
  handshake and answers ``SELECT 1`` with the correct password, while the wrong
  password is rejected by authentication (not by a transport error).
* ``SslMode::Prefer`` (the default when ``--tls-cert``/``--tls-key`` are given
  but ``--require-tls`` is not): both a TLS client and a plaintext client
  succeed against the same PID.
* The authenticated PostgreSQL Unix-socket exception: with ``--require-tls``
  and ``--pg-socket-dir`` the plaintext local socket still demands the SCRAM
  credential and works, while the TCP listener stays TLS-only.
* Foreground *and* ``--daemon``/PID-file identity: the worker named by the PID
  file is the process that owns the listener and serves TLS, its argv carries
  the exact ``--node-id`` UUID, and it is not the launcher PID. A launcher that
  dies without writing a PID file is handled by recovering the orphan worker
  from the exact ``--node-id`` marker, validating exe/starttime, and only then
  signalling it.
* ``--require-tls`` without ``--tls-cert``/``--tls-key`` is refused *before* the
  data directory is created or any listener is bound, in foreground **and**
  ``--daemon`` mode; the daemon refusal writes no PID file and leaves no
  ``--node-id`` orphan worker behind.

Client deadlines
----------------
``psycopg2`` offers no client-side per-statement deadline: ``statement_timeout``
is a *server* setting, and ``connect_timeout`` bounds only the connect phase.
Every SQL proof (connect, execute and fetch) is therefore executed inside an
owned ``multiprocessing`` child bounded by a single ``time.monotonic()``
deadline. If the child does not produce a result in time it is TERM/KILLed and
the run fails; no unbounded blocking path into the database remains. The
harness audits its own AST at runtime so a future edit cannot reintroduce a bare
``psycopg2.connect`` outside the bounded worker.

Exact invocation
----------------
::

    HELIOSDB_TLS_REGRESSION=1 \
    HELIOSDB_NANO_BIN=/abs/path/to/heliosdb-nano \
    HELIOSDB_TLS_EVIDENCE_DIR=/abs/path/to/evidence \
    python3 scripts/test_require_tls_runtime.py

Prerequisites (any missing one exits 3): Linux ``/proc``, ``psycopg2``,
``openssl``, an absolute executable ``HELIOSDB_NANO_BIN`` and an absolute,
writable ``HELIOSDB_TLS_EVIDENCE_DIR``.

Evidence
--------
Per-case stdout/stderr is retained as ``<case>.launch.log`` and the run summary
as ``require-tls-manifest.json`` inside a unique ``0700`` run directory created
under the caller-selected evidence directory. The caller's own files are never
reused or removed. Private fixtures (the self-signed cert/key, the SCRAM
password file, PID files and the UDS directory) live in a unique, short ``0700``
temp directory outside the evidence tree so Unix-socket paths stay under the
``sun_path`` limit; that directory alone is removed after capture. The manifest
retains the raw log names and the binary's before/after SHA-256 digests.

Exit codes
----------
0   ran (or the gate was not requested -> visible skip), every assertion passed
1   ran, at least one assertion failed
3   requested but a prerequisite was missing/invalid (never a silent green)

The binary is hashed before and after the suite and the two digests are asserted
equal, so the evidence is bound to the exact executable under test. No
production source, build, or test target is touched.

Limitation
----------
The daemon worker's own stdout/stderr are ``/dev/null`` (production behaviour in
``start_server_daemon``), so the exact refusal log text is asserted on the
foreground cases (whose child env forces a controlled ``RUST_LOG``); the daemon
case proves refusal by the connection failing and proves the same PID still
serves TLS.
"""

import ast
import hashlib
import json
import multiprocessing
import os
import shutil
import signal
import socket
import stat
import subprocess
import sys
import tempfile
import time
import uuid
from typing import Dict, List, Optional, Tuple

# Importing/executing must not leave a scripts/__pycache__ behind.
sys.dont_write_bytecode = True

try:
    import psycopg2
except Exception:  # pragma: no cover - import failure is an "unavailable" result
    psycopg2 = None

EXIT_OK = 0
EXIT_FAIL = 1
EXIT_UNAVAILABLE = 3

LAUNCH_TIMEOUT = 30.0
READY_TIMEOUT = 20.0
LOG_TIMEOUT = 10.0
SQL_CONNECT_TIMEOUT = 5
# One monotonic deadline covers the owned child's connect + execute + fetch.
SQL_DEADLINE = 15.0
TERM_WAIT = 5.0
KILL_WAIT = 5.0
POLL_INTERVAL = 0.1

# Conservative ceiling for the PostgreSQL `.s.PGSQL.<port>` Unix-socket path
# (Linux `sun_path` is 108 bytes including the terminating NUL).
MAX_UDS_PATH = 100

# Credential channels that must not survive into a launched child.
SECRET_ENV_KEYS = ("HELIOSDB_PASSWORD", "HELIOSDB_PASSWORD_FILE", "HELIOSDB_MCP_TOKEN")

# Force a controlled tracing filter so the refusal diagnostic is never hidden by
# an operator's ambient RUST_LOG. `error!` (the connection-error log) passes a
# global `warn` filter.
CHILD_RUST_LOG = "warn"

# The exact server-side diagnostic emitted when a plaintext client reaches a
# listener configured with SslMode::Require (src/protocol/postgres/server.rs).
REQUIRE_TLS_REFUSAL = "SSL is required but no SSL request was received"

SQL_USER = "postgres"
SQL_DB = "postgres"

CERT_NAME = "server.crt"
KEY_NAME = "server.key"


class Unavailable(Exception):
    """A prerequisite is missing; a requested run must fail closed (exit 3)."""


class Case:
    """One started (or attempted) server and the evidence gathered about it."""

    def __init__(self, name: str) -> None:
        self.name = name
        self.pg_port = 0
        self.node_id = ""
        self.pid = 0
        self.pidfile = ""
        self.logfile = ""
        self.uds_dir = ""
        self.uds_path = ""
        self.launch_rc: Optional[int] = None
        self.launch_out = b""
        self.timed_out = False
        self.starttime = ""
        self.exe = ""
        self.argv: List[str] = []
        self.raw_cmdline = b""
        self.checks: List[Tuple[bool, str]] = []
        self.identity_proven = False
        self.pidfile_seen = False
        self.is_daemon = False
        self.launcher_pid = 0
        self.proc: Optional[subprocess.Popen] = None
        self.stopped = False

    def check(self, ok: bool, label: str) -> bool:
        self.checks.append((bool(ok), label))
        return bool(ok)

    def failed(self) -> bool:
        return any(not ok for ok, _ in self.checks)


def eprint(*parts: object) -> None:
    sys.stderr.write(" ".join(str(p) for p in parts) + "\n")


def now() -> str:
    return time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())


def sha256_file(path: str) -> str:
    h = hashlib.sha256()
    with open(path, "rb") as fh:
        for chunk in iter(lambda: fh.read(1024 * 1024), b""):
            h.update(chunk)
    return h.hexdigest()


def ephemeral_port() -> int:
    """A free loopback port via bind(127.0.0.1, 0). Never a PID-derived range."""
    sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    try:
        sock.bind(("127.0.0.1", 0))
        return int(sock.getsockname()[1])
    finally:
        sock.close()


def proc_exe(pid: int) -> str:
    return os.path.realpath(os.readlink("/proc/%d/exe" % pid))


def proc_starttime(pid: int) -> str:
    """Field 22 (starttime) of /proc/<pid>/stat, robust to spaces in comm."""
    with open("/proc/%d/stat" % pid, "r") as fh:
        data = fh.read()
    rest = data[data.rfind(")") + 1:].split()
    # Field 3 (state) is rest[0], so field 22 (starttime) is rest[19].
    return rest[19]


def proc_cmdline_raw(pid: int) -> bytes:
    with open("/proc/%d/cmdline" % pid, "rb") as fh:
        return fh.read()


def proc_cmdline(pid: int) -> List[str]:
    return [part.decode("utf-8", "replace") for part in proc_cmdline_raw(pid).split(b"\0") if part]


def proc_pids() -> List[int]:
    """Every numeric /proc entry (candidate PIDs) as a sorted list."""
    try:
        entries = os.listdir("/proc")
    except OSError:
        return []
    return sorted(int(e) for e in entries if e.isdigit())


def is_alive(pid: int) -> bool:
    """True only for a live, non-zombie process we can observe."""
    if pid <= 0:
        return False
    try:
        with open("/proc/%d/stat" % pid, "r") as fh:
            data = fh.read()
    except OSError:
        try:
            os.kill(pid, 0)
        except ProcessLookupError:
            return False
        except PermissionError:
            return True
        return True
    rest = data[data.rfind(")") + 1:].split()
    return bool(rest) and rest[0] != "Z"


def unix_socket_inode(pid: int, path: str) -> Optional[int]:
    """Kernel AF_UNIX socket inode bound to `path`, via /proc/<pid>/net/unix.

    ``os.stat`` is used *only* to confirm `path` is a socket: the filesystem
    ``st_ino`` is a different number from the kernel socket inode shown in
    ``/proc/net/unix`` and in ``/proc/<pid>/fd`` as ``socket:[<inode>]``, and is
    never equated with it. The kernel inode is read from field 6 (after
    ``line.split(maxsplit=7)``) of the row whose path field matches `path`
    exactly; the value is decimal.
    """
    try:
        st = os.stat(path)
    except OSError:
        return None
    if not stat.S_ISSOCK(st.st_mode):
        return None
    try:
        with open("/proc/%d/net/unix" % pid, "r", encoding="ascii", errors="replace") as fh:
            next(fh, None)
            for line in fh:
                parts = line.rstrip("\n").split(maxsplit=7)
                if len(parts) < 8:
                    continue
                if parts[7] != path:
                    continue
                try:
                    return int(parts[6])
                except ValueError:
                    continue
    except OSError:
        return None
    return None


def audit_no_blocking_paths(source_text: str) -> List[str]:
    """Structural check that the harness has no unbounded SQL path.

    Every ``psycopg2.connect(...)`` call must live inside ``_child_sql`` (the
    owned, deadline-bounded worker), and the bounded runner must be present.
    Returns a list of human-readable violations (empty == clean).
    """
    violations: List[str] = []
    try:
        tree = ast.parse(source_text)
    except SyntaxError as exc:
        return ["could not parse harness source: %s" % exc]
    if "_run_bounded_sql" not in source_text:
        violations.append("missing _run_bounded_sql bounded-deadline runner")
    allowed = {"_child_sql"}
    stack: List[str] = []

    class Visitor(ast.NodeVisitor):
        def visit_FunctionDef(self, node: ast.FunctionDef) -> None:
            stack.append(node.name)
            self.generic_visit(node)
            stack.pop()

        visit_AsyncFunctionDef = visit_FunctionDef

        def visit_Call(self, node: ast.Call) -> None:
            func = node.func
            if (
                isinstance(func, ast.Attribute)
                and func.attr == "connect"
                and isinstance(func.value, ast.Name)
                and func.value.id == "psycopg2"
            ):
                enclosing = stack[-1] if stack else "<module>"
                if enclosing not in allowed:
                    violations.append(
                        "psycopg2.connect called from %s; it must run inside the "
                        "deadline-bounded _child_sql worker" % enclosing
                    )
            self.generic_visit(node)

    Visitor().visit(tree)
    return violations


def _child_sql(kwargs: Dict[str, object], send_conn) -> None:
    """Owned worker: connect, execute and fetch all under the parent deadline.

    This is the *only* place the harness is permitted to open a PostgreSQL
    connection; ``audit_no_blocking_paths`` enforces that.
    """
    conn = None
    try:
        conn = psycopg2.connect(**kwargs)
        info = getattr(conn, "info", None)
        ssl_in_use = bool(getattr(info, "ssl_in_use", False))
        cur = conn.cursor()
        cur.execute("SELECT 1")
        rows = [tuple(r) for r in cur.fetchall()]
        cur.close()
        send_conn.send(("ok", {"rows": rows, "ssl_in_use": ssl_in_use}))
    except Exception as exc:  # noqa: BLE001 - any failure is reported, never raised
        pgcode = getattr(exc, "pgcode", None)
        if pgcode is None:
            diag = getattr(exc, "diag", None)
            pgcode = getattr(diag, "sqlstate", None)
        try:
            send_conn.send(
                ("error", {"type": type(exc).__name__, "msg": str(exc), "pgcode": pgcode})
            )
        except Exception as report_exc:  # noqa: BLE001 - report transport failure to retained stderr
            sys.stderr.write("SQL worker could not report database error: %s\n" % report_exc)
    finally:
        if conn is not None:
            try:
                conn.close()
            except Exception as close_exc:  # noqa: BLE001 - cleanup failures stay visible
                sys.stderr.write("SQL worker connection close failed: %s\n" % close_exc)
        try:
            send_conn.close()
        except Exception as close_exc:  # noqa: BLE001 - cleanup failures stay visible
            sys.stderr.write("SQL worker pipe close failed: %s\n" % close_exc)


class TlsHarness:
    def __init__(self) -> None:
        self.bin = ""
        self.bin_real = ""
        self.bin_sha = ""
        self.bin_sha_after = ""
        self.binary_hash_ok = True
        self.version_out = ""
        self.evidence_dir = ""
        self.run_dir = ""
        self.output_dir = ""
        self.private_dir = ""
        self.removable_dirs: List[str] = []
        self.cert = ""
        self.key = ""
        self.pw_file = ""
        self.password = ""
        self.wrong_password = ""
        self.registry: Dict[int, Case] = {}
        self.redact_values: List[str] = []
        self.cleanup_failures: List[str] = []
        self.bounded_path_violations: List[str] = []
        self.cases: List[Case] = []

    # ── setup ────────────────────────────────────────────────────────────
    @staticmethod
    def _mkdtemp_0700(prefix: str, base: str) -> str:
        path = tempfile.mkdtemp(prefix=prefix, dir=base)
        os.chmod(path, 0o700)
        return path

    def prepare(self) -> None:
        if os.uname().sysname != "Linux" or not os.access("/proc/self/cmdline", os.R_OK):
            raise Unavailable("requires Linux with /proc mounted")
        if psycopg2 is None:
            raise Unavailable("python3 psycopg2 is not importable (needed for the SELECT 1 proof)")

        raw_bin = os.environ.get("HELIOSDB_NANO_BIN", "")
        if not raw_bin:
            raise Unavailable("HELIOSDB_NANO_BIN is required (absolute path to a built heliosdb-nano)")
        if not os.path.isabs(raw_bin):
            raise Unavailable("HELIOSDB_NANO_BIN must be an absolute path, got %r" % raw_bin)
        if not (os.path.isfile(raw_bin) and os.access(raw_bin, os.X_OK)):
            raise Unavailable("HELIOSDB_NANO_BIN is not an executable regular file: %s" % raw_bin)
        self.bin = raw_bin
        self.bin_real = os.path.realpath(raw_bin)
        self.bin_sha = sha256_file(raw_bin)

        evidence = os.environ.get("HELIOSDB_TLS_EVIDENCE_DIR", "")
        if not evidence:
            raise Unavailable("HELIOSDB_TLS_EVIDENCE_DIR is required (caller-selected absolute evidence directory)")
        if not os.path.isabs(evidence):
            raise Unavailable("HELIOSDB_TLS_EVIDENCE_DIR must be an absolute path, got %r" % evidence)
        os.makedirs(evidence, exist_ok=True)
        if not os.access(evidence, os.W_OK):
            raise Unavailable("HELIOSDB_TLS_EVIDENCE_DIR is not writable: %s" % evidence)
        self.evidence_dir = evidence
        # Unique run dir for logs + manifest; the caller's own files are never
        # reused or removed.
        self.run_dir = self._mkdtemp_0700("hdb-tls-run-", evidence)
        self.output_dir = self.run_dir

        try:
            proc = subprocess.run(
                [self.bin, "--version"],
                stdout=subprocess.PIPE,
                stderr=subprocess.STDOUT,
                timeout=LAUNCH_TIMEOUT,
            )
        except (OSError, subprocess.TimeoutExpired) as exc:
            raise Unavailable("could not run %s --version: %s" % (self.bin, exc))
        if proc.returncode != 0:
            raise Unavailable(
                "%s --version exited %d: %s"
                % (self.bin, proc.returncode, proc.stdout.decode("utf-8", "replace").strip())
            )
        self.version_out = proc.stdout.decode("utf-8", "replace").strip()

        # Private fixtures live in a unique, *short* 0700 temp dir outside the
        # evidence tree: cert/key, password, PID files and UDS sockets. It alone
        # is removed after capture so only logs + manifest persist.
        self.private_dir = self._mkdtemp_0700("hdt-", tempfile.gettempdir())
        self.removable_dirs.append(self.private_dir)

        self.password = "hdb-tls-pw-%d-%s" % (os.getpid(), uuid.uuid4().hex[:12])
        self.wrong_password = self.password + "-wrong"
        # Longest first so the wrong-password prefix does not survive redaction.
        self.redact_values.extend([self.wrong_password, self.password])
        self.pw_file = os.path.join(self.private_dir, "scram-password.txt")
        with open(self.pw_file, "w", encoding="utf-8") as fh:
            fh.write(self.password)
        os.chmod(self.pw_file, 0o600)

        self.cert = os.path.join(self.private_dir, CERT_NAME)
        self.key = os.path.join(self.private_dir, KEY_NAME)
        self._generate_cert()

        self._write(
            "version.txt",
            "binary: %s\nsha256: %s\nversion: %s\n" % (self.bin, self.bin_sha, self.version_out),
        )

    def _generate_cert(self) -> None:
        if shutil.which("openssl") is None:
            raise Unavailable("openssl is required to generate the self-signed test certificate")
        cmd = [
            "openssl",
            "req",
            "-x509",
            "-newkey",
            "rsa:2048",
            "-nodes",
            "-keyout",
            self.key,
            "-out",
            self.cert,
            "-subj",
            "/CN=localhost",
            "-days",
            "1",
        ]
        try:
            proc = subprocess.run(cmd, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, timeout=LAUNCH_TIMEOUT)
        except (OSError, subprocess.TimeoutExpired) as exc:
            raise Unavailable("openssl cert generation failed to run: %s" % exc)
        if proc.returncode != 0:
            raise Unavailable(
                "openssl cert generation failed: %s" % proc.stdout.decode("utf-8", "replace").strip()
            )
        os.chmod(self.key, 0o600)

    def _write(self, name: str, text: str) -> str:
        path = os.path.join(self.output_dir or self.evidence_dir, name)
        with open(path, "w", encoding="utf-8") as fh:
            fh.write(text)
        return path

    def redact(self, text: str) -> str:
        for value in self.redact_values:
            if value:
                text = text.replace(value, "<redacted>")
        return text

    # ── process identity / cleanup ──────────────────────────────────────
    def capture_identity(self, case: Case) -> bool:
        try:
            case.starttime = proc_starttime(case.pid)
            case.exe = proc_exe(case.pid)
            case.raw_cmdline = proc_cmdline_raw(case.pid)
            case.argv = [part.decode("utf-8", "replace") for part in case.raw_cmdline.split(b"\0") if part]
        except OSError:
            return False
        return True

    @staticmethod
    def _argv_pair(argv: List[str], flag: str, value: str) -> bool:
        return any(argv[i] == flag and i + 1 < len(argv) and argv[i + 1] == value for i in range(len(argv)))

    def _identity_ok(self, pid: int, node_id: str, starttime: str, exe: str) -> bool:
        """All three proofs must hold: starttime, executable, node-id pair."""
        if pid <= 0 or not is_alive(pid):
            return False
        try:
            if proc_starttime(pid) != starttime:
                return False
            if proc_exe(pid) != exe:
                return False
            argv = proc_cmdline(pid)
        except OSError:
            return False
        return self._argv_pair(argv, "--node-id", node_id)

    def still_ours(self, case: Case) -> bool:
        return self._identity_ok(case.pid, case.node_id, case.starttime, case.exe)

    def _wait_gone(self, pid: int, seconds: float) -> None:
        deadline = time.monotonic() + seconds
        while time.monotonic() < deadline and is_alive(pid):
            time.sleep(POLL_INTERVAL)

    def _signal_and_wait(self, pid: int, node_id: str, starttime: str, exe: str, label: str) -> None:
        """TERM then KILL, re-validating all three identity proofs before each.

        A PID that no longer matches is never signalled. A cleanly vanished
        process is not a failure; a still-owned process we cannot signal, one
        that survives the waits, or a live PID whose identity changed is
        recorded as unresolved cleanup -- never silently dropped.
        """
        if not self._identity_ok(pid, node_id, starttime, exe):
            if is_alive(pid):
                self.cleanup_failures.append(
                    "%s: pid %d is alive but no longer matches identity; left unsignalled" % (label, pid)
                )
            return
        try:
            os.kill(pid, signal.SIGTERM)
        except ProcessLookupError:
            return
        except OSError as exc:
            self.cleanup_failures.append("%s: SIGTERM to pid %d failed: %s" % (label, pid, exc))
            return
        self._wait_gone(pid, TERM_WAIT)
        if not is_alive(pid):
            return
        if not self._identity_ok(pid, node_id, starttime, exe):
            self.cleanup_failures.append("%s: pid %d changed identity after SIGTERM; not signalling again" % (label, pid))
            return
        try:
            os.kill(pid, signal.SIGKILL)
        except ProcessLookupError:
            return
        except OSError as exc:
            self.cleanup_failures.append("%s: SIGKILL to pid %d failed: %s" % (label, pid, exc))
            return
        self._wait_gone(pid, KILL_WAIT)
        if is_alive(pid) and self._identity_ok(pid, node_id, starttime, exe):
            self.cleanup_failures.append("%s: pid %d still alive after TERM/KILL waits" % (label, pid))

    def _reap_popen(self, proc: Optional[subprocess.Popen], label: str) -> None:
        """TERM then KILL an owned Popen, recording every wait/TERM/KILL failure.

        Nothing here is swallowed: an unreaped child is a hard error in the
        manifest and fails the run.
        """
        if proc is None:
            return
        try:
            if proc.poll() is not None:
                return
        except Exception as exc:
            self.cleanup_failures.append("%s: poll failed: %s" % (label, exc))
            return
        try:
            proc.terminate()
        except ProcessLookupError:
            pass
        except OSError as exc:
            self.cleanup_failures.append("%s: terminate failed: %s" % (label, exc))
        try:
            proc.wait(timeout=TERM_WAIT)
            return
        except subprocess.TimeoutExpired:
            pass
        except OSError as exc:
            self.cleanup_failures.append("%s: wait after TERM failed: %s" % (label, exc))
        try:
            proc.kill()
        except ProcessLookupError:
            pass
        except OSError as exc:
            self.cleanup_failures.append("%s: kill failed: %s" % (label, exc))
        try:
            proc.wait(timeout=KILL_WAIT)
        except subprocess.TimeoutExpired:
            self.cleanup_failures.append("%s: still alive after TERM/KILL waits" % label)
        except OSError as exc:
            self.cleanup_failures.append("%s: wait after KILL failed: %s" % (label, exc))

    def _terminate_worker(self, proc, label: str) -> None:
        """Reap an owned multiprocessing child; record every failure (never swallow)."""
        if proc is None:
            return
        try:
            if proc.is_alive():
                proc.terminate()
        except Exception as exc:
            self.cleanup_failures.append("%s: worker terminate failed: %s" % (label, exc))
        try:
            proc.join(TERM_WAIT)
        except Exception as exc:
            self.cleanup_failures.append("%s: worker join after TERM failed: %s" % (label, exc))
        if proc.is_alive():
            try:
                proc.kill()
            except Exception as exc:
                self.cleanup_failures.append("%s: worker kill failed: %s" % (label, exc))
            try:
                proc.join(KILL_WAIT)
            except Exception as exc:
                self.cleanup_failures.append("%s: worker join after KILL failed: %s" % (label, exc))
        if proc.is_alive():
            self.cleanup_failures.append("%s: worker still alive after TERM/KILL" % label)
        elif proc.exitcode is None:
            self.cleanup_failures.append("%s: worker exit status was not reaped" % label)

    def stop_case(self, case: Case) -> None:
        if case.stopped:
            return
        case.stopped = True
        self.registry.pop(case.pid, None)
        if case.identity_proven:
            self._signal_and_wait(case.pid, case.node_id, case.starttime, case.exe, "case %s" % case.name)
        elif case.proc is not None and case.proc.pid == case.pid:
            # Foreground child we spawned directly: the live Popen handle is the
            # ownership proof even if /proc identity capture itself failed.
            self._reap_popen(case.proc, "case %s foreground" % case.name)
        else:
            # Daemon worker or adopted orphan: only an identity-validated signal
            # is ever sent.
            self._signal_and_wait(case.pid, case.node_id, case.starttime, case.exe, "case %s" % case.name)
        # Always reap the owned Popen (identity-proven foreground children are
        # signalled above but still need a wait() to avoid a lingering zombie).
        if case.proc is not None:
            self._reap_popen(case.proc, "case %s reap" % case.name)

    def cleanup(self) -> None:
        for case in list(self.cases):
            try:
                self.stop_case(case)
            except Exception as exc:  # noqa: BLE001 - cleanup bugs must be visible
                self.cleanup_failures.append("cleanup for %s raised %s: %s" % (case.name, type(exc).__name__, exc))
        for case in list(self.registry.values()):
            try:
                self.stop_case(case)
            except Exception as exc:  # noqa: BLE001
                self.cleanup_failures.append("registry cleanup for %s raised %s: %s" % (case.name, type(exc).__name__, exc))
        # Only directories this harness created are ever removed. Caller content
        # and the retained evidence run dir are never touched.
        for path in self.removable_dirs:
            if path and os.path.isdir(path):
                try:
                    shutil.rmtree(path)
                except OSError as exc:
                    self.cleanup_failures.append("temp dir removal failed for %s: %s" % (path, exc))

    def finalize_binary_binding(self) -> None:
        """Re-hash the exact binary after the suite and assert it is unchanged."""
        if not self.bin:
            return
        self.bin_sha_after = sha256_file(self.bin)
        self.binary_hash_ok = self.bin_sha_after == self.bin_sha

    # ── launch ──────────────────────────────────────────────────────────
    def _new_case(self, name: str) -> Case:
        case = Case(name)
        case.pg_port = ephemeral_port()
        case.node_id = str(uuid.uuid4())
        case.logfile = os.path.join(self.output_dir or self.evidence_dir, name + ".launch.log")
        self.cases.append(case)
        return case

    def _clean_env(self) -> Dict[str, str]:
        env = {k: v for k, v in os.environ.items() if k not in SECRET_ENV_KEYS}
        # Controlled tracing filter: never let an ambient RUST_LOG suppress the
        # refusal diagnostic the foreground cases assert on.
        env["RUST_LOG"] = CHILD_RUST_LOG
        return env

    def _base_argv(self, case: Case) -> List[str]:
        # The SCRAM password reaches the server only through the 0600 password
        # file (a temporary file source), never as a literal on the harness
        # command line.
        return [
            self.bin,
            "start",
            "--memory",
            "--listen",
            "127.0.0.1",
            "--port",
            str(case.pg_port),
            "--http-port",
            "0",
            "--node-id",
            case.node_id,
            "--auth",
            "scram-sha-256",
            "--password-file",
            self.pw_file,
        ]

    def _write_launch_log(self, case: Case) -> None:
        try:
            with open(case.logfile, "w", encoding="utf-8") as fh:
                fh.write(self.redact(case.launch_out.decode("utf-8", "replace")))
        except OSError as exc:
            self.cleanup_failures.append("could not write launch log %s: %s" % (case.logfile, exc))

    def _check_identity(self, case: Case) -> None:
        exe_ok = case.exe == self.bin_real
        marker_ok = self._argv_pair(case.argv, "--node-id", case.node_id)
        case.check(exe_ok, "%s: /proc/%d/exe is the target binary" % (case.name, case.pid))
        case.check(marker_ok, "%s: argv carries the exact --node-id %s" % (case.name, case.node_id))
        if exe_ok and marker_ok:
            self.registry[case.pid] = case
            case.identity_proven = True
        else:
            case.check(False, "%s: identity validation failed; PID not registered for signalling" % case.name)

    def _adopt_worker(self, case: Case, pid: int, starttime: str, exe: str, source: str) -> bool:
        """Adopt a candidate worker only after exe/starttime/--node-id proof.

        The /proc fields are captured before any signal is ever sent, and an
        adopted worker is registered for identity-validated cleanup.
        """
        case.pid = pid
        case.starttime = starttime
        case.exe = exe
        try:
            case.raw_cmdline = proc_cmdline_raw(pid)
        except OSError as exc:
            case.check(False, "%s: could not read /proc/%d/cmdline for %s worker: %s" % (case.name, pid, source, exc))
            return False
        case.argv = [part.decode("utf-8", "replace") for part in case.raw_cmdline.split(b"\0") if part]
        self._check_identity(case)
        if case.identity_proven:
            case.check(True, "%s: adopted %s worker %d with exe/starttime/--node-id proof before signalling" % (case.name, source, pid))
        return case.identity_proven

    def _find_orphan_worker(
        self, node_id: str, exclude: Optional[set] = None
    ) -> Optional[Tuple[int, str, str]]:
        """A live process whose argv carries the exact ``--node-id`` pair and
        whose exe is the target binary. Returns ``(pid, starttime, exe)`` only
        after exe + starttime are read, so callers can validate before signalling.

        `exclude` drops PIDs that must never be mistaken for the worker (the
        harness itself and the daemon launcher, which also carries --node-id).
        """
        if not node_id:
            return None
        excluded = set(exclude or ())
        excluded.add(os.getpid())
        for pid in proc_pids():
            if pid in excluded:
                continue
            try:
                if not is_alive(pid):
                    continue
                if proc_exe(pid) != self.bin_real:
                    continue
                argv = proc_cmdline(pid)
            except OSError:
                continue
            if not self._argv_pair(argv, "--node-id", node_id):
                continue
            try:
                starttime = proc_starttime(pid)
            except OSError:
                continue
            return (pid, starttime, self.bin_real)
        return None

    def launch_foreground(self, name: str, extra_args: List[str]) -> Case:
        case = self._new_case(name)
        return self._launch_foreground_case(case, extra_args)

    def _launch_foreground_case(self, case: Case, extra_args: List[str]) -> Case:
        argv = self._base_argv(case) + list(extra_args)
        logfh = open(case.logfile, "wb")
        try:
            case.proc = subprocess.Popen(
                argv,
                env=self._clean_env(),
                stdin=subprocess.DEVNULL,
                stdout=logfh,
                stderr=subprocess.STDOUT,
            )
        finally:
            logfh.close()
        # Register the live Popen handle on the case *before* touching /proc, so
        # even an unexpected identity-capture exception can still reap the child.
        case.pid = case.proc.pid
        captured = False
        try:
            captured = self.capture_identity(case)
        except Exception as exc:  # noqa: BLE001 - keep the Popen handle registered
            case.check(False, "%s: identity capture raised %s: %s" % (case.name, type(exc).__name__, exc))
        if captured:
            self._check_identity(case)
        else:
            case.check(False, "%s: could not read /proc/%d identity after launch" % (case.name, case.pid))
        return case

    def launch_daemon(self, name: str, extra_args: List[str]) -> Case:
        case = self._new_case(name)
        case.is_daemon = True
        case.pidfile = os.path.join(self.private_dir, name + ".pid")
        try:
            os.remove(case.pidfile)
        except OSError:
            pass
        argv = self._base_argv(case) + ["--daemon", "--pid-file", case.pidfile] + list(extra_args)

        try:
            launcher = subprocess.Popen(
                argv,
                env=self._clean_env(),
                stdin=subprocess.DEVNULL,
                stdout=subprocess.PIPE,
                stderr=subprocess.STDOUT,
            )
        except OSError as exc:
            case.check(False, "%s: could not launch daemon process: %s" % (name, exc))
            return case
        case.launcher_pid = launcher.pid
        out = b""
        try:
            out, _ = launcher.communicate(timeout=LAUNCH_TIMEOUT)
        except subprocess.TimeoutExpired:
            case.timed_out = True
            try:
                launcher.kill()
            except OSError as exc:
                self.cleanup_failures.append("%s: daemon launcher kill failed: %s" % (name, exc))
            try:
                out, _ = launcher.communicate(timeout=TERM_WAIT)
            except subprocess.TimeoutExpired:
                self.cleanup_failures.append("%s: daemon launcher did not exit after kill" % name)
                out = b""
        if launcher.returncode is None:
            self._reap_popen(launcher, "%s launcher" % name)
        case.launch_rc = launcher.returncode
        case.launch_out = out or b""
        self._write_launch_log(case)

        case.pidfile_seen = os.path.exists(case.pidfile)
        pidfile_pid = 0
        if case.pidfile_seen:
            try:
                with open(case.pidfile, "r", encoding="utf-8") as fh:
                    parsed = int(fh.read().strip())
                pidfile_pid = parsed if parsed > 0 else 0
            except (OSError, ValueError):
                pidfile_pid = 0

        adopted = False
        pidfile_wrong_process = False
        if pidfile_pid > 0:
            try:
                starttime = proc_starttime(pidfile_pid)
                exe = proc_exe(pidfile_pid)
            except OSError:
                starttime, exe = "", ""
            if starttime and self._identity_ok(pidfile_pid, case.node_id, starttime, exe):
                adopted = self._adopt_worker(case, pidfile_pid, starttime, exe, "PID-file")
                if adopted:
                    case.check(
                        pidfile_pid != case.launcher_pid,
                        "%s: PID-file worker %d differs from the launcher PID" % (name, pidfile_pid),
                    )
            else:
                # A parseable PID file that names a positive PID which is not the
                # exact --node-id worker is a hard identity failure. Orphan
                # recovery below may still adopt the real worker for cleanup, but
                # it must never clear this or let the daemon case pass.
                pidfile_wrong_process = True

        if not adopted:
            found = self._find_orphan_worker(case.node_id, exclude={case.launcher_pid})
            if found is not None:
                pid, starttime, exe = found
                adopted = self._adopt_worker(case, pid, starttime, exe, "orphan")
                if adopted:
                    case.check(
                        True,
                        "%s: recovered orphan worker %d by exact --node-id after PID-file failure" % (name, pid),
                    )
            elif pidfile_pid > 0:
                case.check(
                    False,
                    "%s: PID-file worker %d unidentifiable and no --node-id orphan found" % (name, pidfile_pid),
                )

        if pidfile_wrong_process:
            case.check(
                False,
                "%s: parseable PID file named process %d, which is not the exact "
                "--node-id worker; adopting the orphan does not clear the PID-file identity failure"
                % (name, pidfile_pid),
            )

        if case.timed_out:
            case.check(False, "%s: daemon launcher timed out" % name)
            return case
        if case.launch_rc != 0:
            case.check(False, "%s: daemon launcher exited %s" % (name, case.launch_rc))
            return case
        if not case.pidfile_seen or pidfile_pid <= 0:
            case.check(False, "%s: daemon launcher left no parseable PID file" % name)
            return case
        if not case.identity_proven:
            case.check(False, "%s: could not prove the PID-file worker identity" % name)
        return case

    # ── listener ownership (Linux) ──────────────────────────────────────
    @staticmethod
    def listening_inodes(port: int) -> set:
        """Inodes of LISTEN sockets on `port`, from /proc/net/tcp{,6}.

        The inode is the parser's exact field 9 (0-indexed after ``line.split()``
        on the documented column layout ``sl local_address rem_address st
        tx_queue:rx_queue tr:tm->when retrnsmt uid timeout inode ...``). We read
        that one field; we deliberately do NOT scan trailing bookkeeping fields
        for a value that "looks like" a socket inode.
        """
        inodes = set()
        for path in ("/proc/net/tcp", "/proc/net/tcp6"):
            try:
                with open(path, "r", encoding="ascii", errors="replace") as fh:
                    next(fh, None)
                    for line in fh:
                        parts = line.split()
                        if len(parts) < 10:
                            continue
                        local, state, inode = parts[1], parts[3], parts[9]
                        if state != "0A":  # TCP_LISTEN
                            continue
                        _, _, p = local.rpartition(":")
                        try:
                            if int(p, 16) == port:
                                inodes.add(inode)
                        except ValueError:
                            continue
            except OSError:
                continue
        return inodes

    def child_owns_port(self, pid: int, port: int) -> bool:
        """True iff `pid` holds a /proc/<pid>/fd socket whose inode is the
        LISTEN socket for `port`."""
        wanted = self.listening_inodes(port)
        if not wanted:
            return False
        fd_dir = "/proc/%d/fd" % pid
        try:
            fds = os.listdir(fd_dir)
        except OSError:
            return False
        for fd in fds:
            try:
                target = os.readlink(os.path.join(fd_dir, fd))
            except OSError:
                continue
            if target.startswith("socket:[") and target.endswith("]"):
                if target[len("socket:["):-1] in wanted:
                    return True
        return False

    def child_owns_unix_socket(self, pid: int, path: str) -> bool:
        """True iff `pid` holds the exact fd for the Unix socket at `path`.

        The kernel inode is resolved from /proc/<pid>/net/unix by an exact path
        match (never from the socket file's filesystem ``st_ino``), then
        required to appear as a ``socket:[<inode>]`` link in the child's fd
        table -- an exact kernel-inode match, not merely the existence of a
        directory.
        """
        inode = unix_socket_inode(pid, path)
        if inode is None:
            return False
        target = "socket:[%d]" % inode
        fd_dir = "/proc/%d/fd" % pid
        try:
            fds = os.listdir(fd_dir)
        except OSError:
            return False
        for fd in fds:
            try:
                if os.readlink(os.path.join(fd_dir, fd)) == target:
                    return True
            except OSError:
                continue
        return False

    def assert_owns_listener(self, case: Case, port: int, label: str) -> bool:
        if self.child_owns_port(case.pid, port):
            case.check(True, "%s: child owns the listening socket on port %d" % (label, port))
            return True
        case.check(
            False,
            "%s: child does NOT own a listening socket on port %d (probe-close race or wrong process)" % (label, port),
        )
        return False

    # ── readiness + log evidence ────────────────────────────────────────
    def wait_ready(self, case: Case) -> bool:
        deadline = time.monotonic() + READY_TIMEOUT
        while time.monotonic() < deadline:
            if not is_alive(case.pid):
                return False
            if self.child_owns_port(case.pid, case.pg_port):
                try:
                    sock = socket.create_connection(("127.0.0.1", case.pg_port), timeout=1.0)
                    sock.close()
                    return True
                except OSError:
                    pass
            time.sleep(POLL_INTERVAL)
        return False

    def wait_path(self, path: str, timeout: float = READY_TIMEOUT) -> bool:
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            if os.path.exists(path):
                return True
            time.sleep(POLL_INTERVAL)
        return False

    def wait_log_contains(self, case: Case, needle: str, timeout: float = LOG_TIMEOUT) -> bool:
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            try:
                with open(case.logfile, "r", encoding="utf-8", errors="replace") as fh:
                    if needle in fh.read():
                        return True
            except OSError:
                pass
            time.sleep(POLL_INTERVAL)
        return False

    # ── SQL proofs (owned subprocess, monotonic deadline) ───────────────
    def _make_pipe(self):
        """Injectable pipe factory (tests replace this with a fake)."""
        ctx = multiprocessing.get_context("fork")
        return ctx.Pipe(duplex=False)

    def _spawn_worker(self, target, args):
        """Injectable worker factory (tests replace this with a fake)."""
        ctx = multiprocessing.get_context("fork")
        return ctx.Process(target=target, args=args, daemon=True)

    def _run_bounded_sql(self, kwargs: Dict[str, object], label: str):
        """Run connect+execute+fetch in an owned child under a monotonic deadline.

        Returns ``(status, payload)`` where status is ``"ok"``, ``"error"`` or
        ``"timeout"``. There is no unbounded blocking path: the parent only ever
        waits on ``poll`` up to the deadline, then TERM/KILLs the child.
        """
        parent_conn, child_conn = self._make_pipe()
        proc = self._spawn_worker(_child_sql, (kwargs, child_conn))
        status = "error"
        payload: Dict[str, object] = {
            "type": "HarnessError",
            "msg": "SQL worker never produced a result",
            "pgcode": None,
        }
        timed_out = False
        started = False
        try:
            try:
                proc.start()
                started = True
            except Exception as exc:  # noqa: BLE001 - a failed spawn is a red result
                status = "error"
                payload = {"type": type(exc).__name__, "msg": "worker failed to start: %s" % exc, "pgcode": None}
                return (status, payload)
            try:
                child_conn.close()
            except Exception as exc:  # noqa: BLE001 - never swallow pipe-close failures
                self.cleanup_failures.append("%s: closing SQL child pipe failed: %s" % (label, exc))
            deadline = time.monotonic() + SQL_DEADLINE
            remaining = deadline - time.monotonic()
            if parent_conn.poll(max(0.0, remaining)):
                try:
                    status, payload = parent_conn.recv()
                except EOFError:
                    status = "error"
                    payload = {
                        "type": "EOFError",
                        "msg": "SQL worker closed the pipe without a result",
                        "pgcode": None,
                    }
            else:
                timed_out = True
        finally:
            try:
                parent_conn.close()
            except Exception as exc:  # noqa: BLE001
                self.cleanup_failures.append("%s: closing SQL pipe failed: %s" % (label, exc))
            if started:
                self._terminate_worker(proc, label)
            else:
                try:
                    child_conn.close()
                except Exception as exc:  # noqa: BLE001 - never swallow pipe-close failures
                    self.cleanup_failures.append("%s: closing SQL child pipe after failed spawn failed: %s" % (label, exc))
            if timed_out:
                self.cleanup_failures.append(
                    "%s: SQL client deadline exceeded after %.1fs (connect/execute/fetch bounded)" % (label, SQL_DEADLINE)
                )
                return (
                    "timeout",
                    {
                        "type": "DeadlineExceeded",
                        "msg": "no result within %.1fs" % SQL_DEADLINE,
                        "pgcode": None,
                    },
                )
        return (status, payload)

    def _sql_kwargs(self, host: str, port: int, password: str, sslmode: Optional[str]) -> Dict[str, object]:
        # connect_timeout bounds only the TCP connect; the owned-subprocess
        # deadline bounds connect+execute+fetch. statement_timeout is *not* a
        # client deadline and is deliberately not used here.
        kwargs: Dict[str, object] = {
            "host": host,
            "port": port,
            "dbname": SQL_DB,
            "user": SQL_USER,
            "password": password,
            "connect_timeout": SQL_CONNECT_TIMEOUT,
        }
        if sslmode is not None:
            kwargs["sslmode"] = sslmode
        return kwargs

    def _select1(
        self,
        case: Case,
        host: str,
        password: str,
        sslmode: Optional[str],
        label: str,
        expect_ssl: bool,
    ) -> None:
        status, payload = self._run_bounded_sql(self._sql_kwargs(host, case.pg_port, password, sslmode), label)
        if status == "ok":
            rows = [tuple(r) for r in payload.get("rows", [])]  # type: ignore[arg-type]
            case.check(rows == [(1,)], "%s: SELECT 1 returned exactly [(1,)] (got %r)" % (label, rows))
            got_ssl = bool(payload.get("ssl_in_use"))
            case.check(
                got_ssl == expect_ssl,
                "%s: conn.info.ssl_in_use == %s (expected %s, got %s)"
                % (label, expect_ssl, expect_ssl, got_ssl),
            )
        else:
            case.check(
                False,
                "%s: SELECT 1 failed [%s]: %s: %s"
                % (label, status, payload.get("type"), payload.get("msg")),
            )

    def select1_tls(self, case: Case, label: str) -> None:
        self._select1(case, "127.0.0.1", self.password, "require", label + " [tls]", expect_ssl=True)

    def select1_plaintext(self, case: Case, label: str) -> None:
        self._select1(case, "127.0.0.1", self.password, "disable", label + " [plaintext]", expect_ssl=False)

    def select1_uds(self, case: Case, label: str) -> None:
        self._select1(case, case.uds_dir, self.password, None, label + " [uds]", expect_ssl=False)

    def expect_auth_rejected(
        self,
        case: Case,
        password: str,
        label: str,
        host: str = "127.0.0.1",
        sslmode: Optional[str] = "require",
        transport: str = "tls",
        uds_path: Optional[str] = None,
    ) -> None:
        """A wrong password must be rejected by *authentication* (SQLSTATE 28),
        not by a transport error, crash, or timeout; the child/listener must
        survive the attempt."""
        status, payload = self._run_bounded_sql(self._sql_kwargs(host, case.pg_port, password, sslmode), label)
        auth_rejected = False
        if status == "error":
            code = payload.get("pgcode")
            message = str(payload.get("msg", "")).lower()
            auth_rejected = (
                (code is not None and str(code).startswith("28"))
                or "authentication failed" in message
                or "password authentication failed" in message
                or "invalid_password" in message
            )
            detail = "pgcode=%s type=%s: %s" % (code, payload.get("type"), payload.get("msg"))
        elif status == "ok":
            detail = "connection accepted and SELECT 1 returned %r" % (payload.get("rows"),)
        else:
            detail = "client deadline exceeded: %s" % payload.get("msg")
        case.check(
            auth_rejected,
            "%s: wrong credentials rejected by authentication via %s [%s]" % (label, transport, detail),
        )
        case.check(self.still_ours(case), "%s: child still alive after the wrong-password attempt" % label)
        if uds_path is not None:
            owns = self.child_owns_unix_socket(case.pid, uds_path)
            case.check(
                owns,
                "%s: child still owns the exact UDS inode for %s after the wrong-password attempt (got %s)"
                % (label, uds_path, owns),
            )
        else:
            owns = self.child_owns_port(case.pid, case.pg_port)
            case.check(
                owns,
                "%s: listener survived the wrong-password attempt (child owns port %d: %s)"
                % (label, case.pg_port, owns),
            )

    def expect_plaintext_refused(self, case: Case, label: str, expect_log: bool = True) -> None:
        """Under --require-tls a plaintext TCP client (sslmode=disable, so no
        SSLRequest is sent) must be refused with the exact server diagnostic,
        and the SAME PID must remain bound and healthy."""
        status, payload = self._run_bounded_sql(
            self._sql_kwargs("127.0.0.1", case.pg_port, self.password, "disable"), label
        )
        refused = status == "error"
        if status == "ok":
            detail = "plaintext SELECT 1 returned %r" % (payload.get("rows"),)
        elif status == "timeout":
            detail = "plaintext connection hung until the client deadline"
        else:
            detail = "%s: %s" % (payload.get("type"), payload.get("msg"))
        case.check(refused, "%s: plaintext PostgreSQL connection was refused [%s]" % (label, detail))
        if expect_log:
            if self.wait_log_contains(case, REQUIRE_TLS_REFUSAL):
                case.check(True, "%s: server log records %r" % (label, REQUIRE_TLS_REFUSAL))
            else:
                case.check(False, "%s: server log does not contain %r" % (label, REQUIRE_TLS_REFUSAL))
        case.check(self.still_ours(case), "%s: SAME PID is alive after the plaintext refusal" % label)
        owns = self.child_owns_port(case.pid, case.pg_port)
        case.check(
            owns,
            "%s: SAME PID still owns the listener after the plaintext refusal (got %s)" % (label, owns),
        )

    # ── expected startup rejection before any mutation ──────────────────
    def expect_refused_before_mutation(
        self,
        name: str,
        extra_args: List[str],
        data_dir: str,
        needles: List[str],
        daemon: bool = False,
        pid_file: str = "",
        node_id: str = "",
    ) -> Case:
        """A TLS config error must exit non-zero, say why, and leave no data
        directory, no listener, no PID file and no --node-id orphan behind."""
        case = self._new_case(name)
        if node_id:
            case.node_id = node_id
        if daemon:
            case.is_daemon = True
            case.pidfile = pid_file
        argv = [
            self.bin,
            "start",
            "--listen",
            "127.0.0.1",
            "--port",
            str(case.pg_port),
            "--http-port",
            "0",
            "--data-dir",
            data_dir,
        ]
        if daemon:
            argv += ["--daemon", "--pid-file", pid_file]
        if node_id:
            argv += ["--node-id", node_id]
        argv += list(extra_args)
        try:
            proc = subprocess.run(
                argv,
                env=self._clean_env(),
                stdout=subprocess.PIPE,
                stderr=subprocess.STDOUT,
                timeout=LAUNCH_TIMEOUT,
            )
            case.launch_rc = proc.returncode
            case.launch_out = proc.stdout or b""
        except subprocess.TimeoutExpired as exc:
            case.timed_out = True
            case.launch_out = exc.stdout or b""
        self._write_launch_log(case)
        out = case.launch_out.decode("utf-8", "replace")
        case.check(not case.timed_out, "%s: validation refusal did not hang" % name)
        case.check(case.launch_rc not in (None, 0), "%s: exited non-zero (got %r)" % (name, case.launch_rc))
        case.check(any(needle in out for needle in needles), "%s: rejection message matched (%s)" % (name, " / ".join(needles)))
        case.check(not os.path.exists(data_dir), "%s: data directory was not created/mutated" % name)
        case.check(not self.listening_inodes(case.pg_port), "%s: no listener was bound" % name)
        if daemon:
            case.pidfile_seen = os.path.exists(pid_file)
            case.check(not case.pidfile_seen, "%s: daemon refusal wrote no PID file" % name)
            orphan = self._find_orphan_worker(node_id)
            case.check(orphan is None, "%s: daemon refusal left no --node-id orphan worker" % name)
            if orphan is not None:
                pid, starttime, exe = orphan
                self._signal_and_wait(pid, node_id, starttime, exe, "case %s orphan" % name)
        return case


# ── scenario bodies ─────────────────────────────────────────────────────
def _started(h: TlsHarness, case: Case, label: str) -> bool:
    """Common preflight for a launched case: identity + owned listener."""
    case.check(case.identity_proven, "%s: process identity proven (exe/starttime/--node-id)" % label)
    if not case.identity_proven:
        h.stop_case(case)
        return False
    if not h.wait_ready(case):
        h.assert_owns_listener(case, case.pg_port, label)
        case.check(False, "%s: never accepted connections" % label)
        h.stop_case(case)
        return False
    h.assert_owns_listener(case, case.pg_port, label)
    return True


def case_foreground_require_tls(h: TlsHarness) -> None:
    name = "01-foreground-require-tls"
    case = h.launch_foreground(name, ["--require-tls", "--tls-cert", h.cert, "--tls-key", h.key])
    if not _started(h, case, name):
        return
    # Plaintext is refused with the exact diagnostic; same PID survives.
    h.expect_plaintext_refused(case, name + " tcp-plaintext", expect_log=True)
    # The SAME PID then completes TLS with the correct SCRAM credential.
    h.select1_tls(case, name + " correct")
    h.expect_auth_rejected(case, h.wrong_password, name + " wrong", sslmode="require", transport="tls")
    case.check(h.still_ours(case), "%s: SAME PID still alive after TLS auth" % name)
    case.check(h.child_owns_port(case.pid, case.pg_port), "%s: SAME PID still owns the listener" % name)
    h.stop_case(case)


def case_foreground_prefer(h: TlsHarness) -> None:
    name = "02-foreground-prefer"
    case = h.launch_foreground(name, ["--tls-cert", h.cert, "--tls-key", h.key])
    if not _started(h, case, name):
        return
    # SslMode::Prefer offers TLS and still accepts a plaintext client.
    h.select1_tls(case, name + " correct")
    h.select1_plaintext(case, name + " plaintext accepted")
    h.expect_auth_rejected(case, h.wrong_password, name + " wrong", sslmode="require", transport="tls")
    case.check(h.still_ours(case), "%s: SAME PID served TLS and plaintext" % name)
    h.stop_case(case)


def case_foreground_uds_exception(h: TlsHarness) -> None:
    name = "03-foreground-uds-exception"
    case = h._new_case(name)
    # A unique, short, 0700 dir under the private temp tree keeps the socket
    # path well under the Linux sun_path limit.
    uds_dir = os.path.join(h.private_dir, "u" + case.node_id[:8])
    os.makedirs(uds_dir, exist_ok=True)
    os.chmod(uds_dir, 0o700)
    sock_path = os.path.join(uds_dir, ".s.PGSQL.%d" % case.pg_port)
    if len(os.fsencode(sock_path)) >= MAX_UDS_PATH:
        raise Unavailable(
            "Unix socket path exceeds %d bytes (got %d): %s" % (MAX_UDS_PATH, len(os.fsencode(sock_path)), sock_path)
        )
    case.uds_dir = uds_dir
    case.uds_path = sock_path
    h._launch_foreground_case(
        case,
        ["--require-tls", "--tls-cert", h.cert, "--tls-key", h.key, "--pg-socket-dir", uds_dir],
    )
    if not _started(h, case, name):
        return
    case.check(h.wait_path(sock_path), "%s: PostgreSQL UDS socket was created at %s" % (name, sock_path))
    # Exact inode proof: the child must own the fd for THIS socket path.
    case.check(
        h.child_owns_unix_socket(case.pid, sock_path),
        "%s: child owns the exact UDS inode for %s" % (name, sock_path),
    )
    # TCP stays TLS-only even though the local socket is plaintext.
    h.expect_plaintext_refused(case, name + " tcp-plaintext", expect_log=True)
    h.select1_tls(case, name + " tcp-tls")
    # The authenticated local exception: plaintext UDS still demands SCRAM.
    h.select1_uds(case, name + " uds")
    h.expect_auth_rejected(
        case,
        h.wrong_password,
        name + " uds-wrong",
        host=uds_dir,
        sslmode=None,
        transport="uds",
        uds_path=sock_path,
    )
    case.check(h.still_ours(case), "%s: SAME PID served TCP TLS and plaintext UDS" % name)
    h.stop_case(case)


def case_daemon_require_tls(h: TlsHarness) -> None:
    name = "04-daemon-require-tls"
    case = h.launch_daemon(name, ["--require-tls", "--tls-cert", h.cert, "--tls-key", h.key])
    if case.timed_out or case.launch_rc != 0 or not case.identity_proven:
        # launch_daemon already recorded why; never probe or signal an unproven PID.
        return
    if not h.wait_ready(case):
        h.assert_owns_listener(case, case.pg_port, name)
        case.check(False, "%s: PID-file worker never accepted connections" % name)
        h.stop_case(case)
        return
    h.assert_owns_listener(case, case.pg_port, name)
    # The worker's stdout/stderr are /dev/null by design, so the exact refusal
    # text is asserted in the foreground cases; here the refusal is the closed
    # connection itself, and the SAME PID must still serve TLS.
    h.expect_plaintext_refused(case, name + " tcp-plaintext", expect_log=False)
    h.select1_tls(case, name + " correct")
    h.expect_auth_rejected(case, h.wrong_password, name + " wrong", sslmode="require", transport="tls")
    case.check(h.still_ours(case), "%s: PID-file worker still alive after TLS auth" % name)
    h.stop_case(case)


def case_missing_cert_or_key(h: TlsHarness) -> None:
    variants = [
        (
            "05-require-tls-no-cert-key",
            ["--require-tls"],
            ["--require-tls requires both --tls-cert and --tls-key."],
        ),
        (
            "06-require-tls-only-cert",
            ["--require-tls", "--tls-cert", os.path.join(h.private_dir, "does-not-exist.crt")],
            ["Both --tls-cert and --tls-key must be specified together"],
        ),
        (
            "07-require-tls-only-key",
            ["--require-tls", "--tls-key", os.path.join(h.private_dir, "does-not-exist.key")],
            ["Both --tls-cert and --tls-key must be specified together"],
        ),
    ]
    for name, extra, needles in variants:
        # A data directory that must never be created by a rejected startup.
        data_dir = os.path.join(h.private_dir, name + ".data")
        if os.path.exists(data_dir):
            try:
                shutil.rmtree(data_dir)
            except OSError as exc:
                h.cleanup_failures.append("%s: stale data directory removal failed: %s" % (name, exc))
                continue
        h.expect_refused_before_mutation(name, extra, data_dir, needles)

        # The same refusal in daemon mode must additionally write no PID file
        # and spawn no --node-id orphan worker.
        node_id = str(uuid.uuid4())
        daemon_data_dir = os.path.join(h.private_dir, name + ".daemon.data")
        if os.path.exists(daemon_data_dir):
            try:
                shutil.rmtree(daemon_data_dir)
            except OSError as exc:
                h.cleanup_failures.append("%s: stale daemon data directory removal failed: %s" % (name, exc))
                continue
        pid_file = os.path.join(h.private_dir, name + ".daemon.pid")
        try:
            os.remove(pid_file)
        except OSError:
            pass
        h.expect_refused_before_mutation(
            name + "-daemon",
            extra,
            daemon_data_dir,
            needles,
            daemon=True,
            pid_file=pid_file,
            node_id=node_id,
        )


def build_cases(h: TlsHarness) -> None:
    case_foreground_require_tls(h)
    case_foreground_prefer(h)
    case_foreground_uds_exception(h)
    case_daemon_require_tls(h)
    case_missing_cert_or_key(h)


def write_manifest(h: TlsHarness, unavailable: Optional[str]) -> None:
    cases = []
    for case in h.cases:
        cases.append(
            {
                "name": case.name,
                "pid": case.pid,
                "pg_port": case.pg_port,
                "node_id": case.node_id,
                "is_daemon": case.is_daemon,
                "launcher_pid": case.launcher_pid,
                "launch_rc": case.launch_rc,
                "timed_out": case.timed_out,
                "pidfile": h.redact(case.pidfile),
                "pidfile_seen": case.pidfile_seen,
                "identity_proven": case.identity_proven,
                "starttime": case.starttime,
                "exe": case.exe,
                "log": os.path.basename(case.logfile),
                "argv": [h.redact(a) for a in case.argv],
                "checks": [{"ok": ok, "label": h.redact(label)} for ok, label in case.checks],
                "failed": case.failed(),
            }
        )
    manifest = {
        "at": now(),
        "feature": "postgresql-tcp-require-tls",
        "require_tls_refusal_text": REQUIRE_TLS_REFUSAL,
        "binary": h.bin,
        "binary_realpath": h.bin_real,
        "binary_sha256": h.bin_sha,
        "binary_sha256_after": h.bin_sha_after,
        "binary_hash_ok": h.binary_hash_ok,
        "version": h.version_out,
        "evidence_run_dir": h.run_dir,
        "raw_logs": [os.path.basename(c.logfile) for c in h.cases],
        "source_binding": "executable hash only; source manifest produced by root outside this harness",
        "gate": {"HELIOSDB_TLS_REGRESSION": os.environ.get("HELIOSDB_TLS_REGRESSION")},
        "child_rust_log": CHILD_RUST_LOG,
        "sql_deadline_seconds": SQL_DEADLINE,
        "bounded_path_violations": h.bounded_path_violations,
        "unavailable": unavailable,
        "cleanup_failures": h.cleanup_failures,
        "cases": cases,
    }
    path = os.path.join(h.output_dir or h.evidence_dir, "require-tls-manifest.json")
    with open(path, "w", encoding="utf-8") as fh:
        json.dump(manifest, fh, indent=2)


def audit_harness_source() -> List[str]:
    try:
        with open(__file__, "r", encoding="utf-8") as fh:
            return audit_no_blocking_paths(fh.read())
    except Exception as exc:  # noqa: BLE001
        return ["harness source audit failed: %s: %s" % (type(exc).__name__, exc)]


def main() -> int:
    if os.environ.get("HELIOSDB_TLS_REGRESSION") != "1":
        eprint("skip: gated real-binary required-TLS harness; set HELIOSDB_TLS_REGRESSION=1 to run")
        return EXIT_OK

    h = TlsHarness()
    h.bounded_path_violations = audit_harness_source()
    unavailable: Optional[str] = None
    try:
        h.prepare()
        build_cases(h)
        h.finalize_binary_binding()
    except Unavailable as exc:
        unavailable = str(exc)
        eprint("unavailable: %s" % exc)
    except Exception as exc:  # noqa: BLE001 - convert harness bugs into a red result
        unavailable = "harness error: %s: %s" % (type(exc).__name__, exc)
        eprint("unavailable: %s" % unavailable)
    finally:
        h.cleanup()
        if h.output_dir or h.evidence_dir:
            write_manifest(h, unavailable)

    if unavailable is not None:
        return EXIT_UNAVAILABLE

    for case in h.cases:
        for ok, label in case.checks:
            eprint("%s %s" % ("ok  " if ok else "FAIL", label))
    eprint(
        "%s binary sha256 unchanged before/after suite (%s)"
        % ("ok  " if h.binary_hash_ok else "FAIL", h.bin_sha[:16])
    )
    for violation in h.bounded_path_violations:
        eprint("FAIL bounded-path audit: %s" % violation)
    if h.cleanup_failures:
        for item in h.cleanup_failures:
            eprint("FAIL cleanup: %s" % item)

    failures = [c for c in h.cases if c.failed()]
    if failures or h.cleanup_failures or not h.binary_hash_ok or h.bounded_path_violations:
        eprint("RESULT: FAIL (%d case(s) with failures)" % len(failures))
        return EXIT_FAIL
    eprint(
        "RESULT: PASS - required-TLS refused plaintext, the same PID served TLS, "
        "Prefer and the authenticated UDS exception held, and the daemon PID file matched its worker"
    )
    return EXIT_OK


if __name__ == "__main__":
    sys.exit(main())
