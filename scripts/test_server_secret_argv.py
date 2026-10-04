#!/usr/bin/env python3
"""Real-process P0-2 secret-handling regression for the daemon.

This is the acceptance-evidence harness behind
``scripts/test_server_secret_argv.sh``. It forks real ``heliosdb-nano start
--daemon`` children, inspects each child's ``/proc/<pid>`` directly, proves the
PostgreSQL credentials actually authenticate (``SELECT 1`` with the right
password, refusal with the wrong one), and writes an evidence bundle into a
caller-selected directory.

It is deliberately NOT part of ``cargo test``/CI: it binds real ephemeral TCP
ports and forks real daemons. See ``main`` for the gate.

Exit codes
----------
0   ran, every assertion passed (or the gate was not requested -> visible skip)
1   ran, at least one assertion failed
3   requested (gated) but could not run: missing/invalid binary, missing
    psycopg2, missing evidence directory, non-Linux, or an MCP HTTP check that
    the binary cannot satisfy. Fail closed: never a silent green.
"""

import hashlib
import json
import os
import re
import select
import shutil
import signal
import socket
import subprocess
import sys
import time
import urllib.error
import urllib.request
import uuid
from typing import Dict, List, Optional, Tuple

try:
    import psycopg2
except Exception:  # pragma: no cover - import failure is an "unavailable" result
    psycopg2 = None

EXIT_OK = 0
EXIT_FAIL = 1
EXIT_UNAVAILABLE = 3

LAUNCH_TIMEOUT = 30.0
READY_TIMEOUT = 15.0
SQL_CONNECT_TIMEOUT = 5
SQL_STATEMENT_TIMEOUT = 5.0
HTTP_TIMEOUT = 10.0
TERM_WAIT = 5.0
KILL_WAIT = 5.0
POLL_INTERVAL = 0.1

SECRET_ENV_KEYS = ("HELIOSDB_PASSWORD", "HELIOSDB_PASSWORD_FILE", "HELIOSDB_MCP_TOKEN")

SQL_USER = "postgres"
SQL_DB = "postgres"


class Unavailable(Exception):
    """A prerequisite is missing; the gated run must fail closed (exit 3)."""


class SqlDeadlineExceeded(Exception):
    """The monotonic client deadline expired mid-connect or mid-statement.

    Deliberately NOT a ``psycopg2.Error`` subclass: callers must classify a
    client-side timeout as a transport/harness failure, never as an
    authentication rejection (SQLSTATE 28) or a clean rejection.
    """


class Orphan:
    """A child recovered without a usable pidfile, bound to its unique marker.

    The marker (`--node-id <uuid>`) plus the captured starttime and executable
    are required again on *every* signal, so a later process running the same
    binary with a different marker is never adopted or killed.
    """

    def __init__(self, pid: int, node_id: str, starttime: str, exe: str) -> None:
        self.pid = pid
        self.node_id = node_id
        self.starttime = starttime
        self.exe = exe


class Case:
    """One started (or attempted) daemon and the evidence gathered about it."""

    def __init__(self, name: str) -> None:
        self.name = name
        self.pg_port = 0
        self.http_port = 0
        self.node_id = ""
        self.pid = 0
        self.pidfile = ""
        self.logfile = ""
        self.launch_rc: Optional[int] = None
        self.launch_out = b""
        self.timed_out = False
        self.starttime = ""
        self.exe = ""
        self.argv: List[str] = []
        self.raw_cmdline = b""
        self.environ: Dict[str, str] = {}
        self.checks: List[Tuple[bool, str]] = []
        self.orphans: List[Orphan] = []
        self.identity_proven = False
        self.pidfile_seen = False
        self.stray = False

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
    """A free loopback port via bind(127.0.0.1, 0). Never a PID-derived range.

    The probe socket is closed before the child is launched, so there is a
    small reuse window; the child's own bind failure is treated as a distinct
    (unrelated) failure by the config-error checker rather than a silent pass.
    """
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
    # Field 3 (state) is rest[0], so field 22 is rest[19].
    return rest[19]


def proc_cmdline_raw(pid: int) -> bytes:
    with open("/proc/%d/cmdline" % pid, "rb") as fh:
        return fh.read()


def proc_cmdline(pid: int) -> List[str]:
    raw = proc_cmdline_raw(pid)
    return [part.decode("utf-8", "replace") for part in raw.split(b"\0") if part]


def proc_environ(pid: int) -> Dict[str, str]:
    try:
        with open("/proc/%d/environ" % pid, "rb") as fh:
            raw = fh.read()
    except OSError:
        return {}
    out: Dict[str, str] = {}
    for item in raw.split(b"\0"):
        if b"=" in item:
            key, _, value = item.partition(b"=")
            out[key.decode("utf-8", "replace")] = value.decode("utf-8", "replace")
    return out


def is_alive(pid: int) -> bool:
    """True only for a live, non-zombie process we can observe.

    A zombie has exited; treating it as dead avoids reporting a normally
    finished child as a cleanup failure and keeps TERM/KILL waits bounded.
    """
    if pid <= 0:
        return False
    try:
        with open("/proc/%d/stat" % pid, "r") as fh:
            data = fh.read()
    except OSError:
        # /proc unreadable (gone, or another owner): fall back to signal 0.
        try:
            os.kill(pid, 0)
        except ProcessLookupError:
            return False
        except PermissionError:
            return True
        return True
    rest = data[data.rfind(")") + 1 :].split()
    return bool(rest) and rest[0] != "Z"


def poll_async_connection(conn, deadline: float, select_fn=select.select) -> None:
    """Drive a psycopg2 ``async_=True`` connection until POLL_OK or the deadline.

    This one helper is shared by connection establishment and by every ``SELECT
    1`` execution, so neither phase can block past its monotonic budget:

    * ``POLL_OK`` returns, but only when the monotonic deadline has not already
      expired: the clock is checked *before* each ``poll()`` and again *before*
      accepting ``POLL_OK``, so a select that times out followed by a late
      ``POLL_OK`` is still rejected.
    * ``POLL_READ``/``POLL_WRITE`` wait on ``conn.fileno()`` via ``select`` for
      only the remaining budget.
    * any other poll state raises ``OperationalError`` (never silently OK).
    * a ``psycopg2.Error`` raised by ``poll()`` (SQLSTATE 28 auth refusal, etc.)
      propagates untouched so callers keep their exact classification.
    * exhausting the deadline raises :class:`SqlDeadlineExceeded`, which is not a
      ``psycopg2.Error`` and therefore can never be mistaken for auth rejection.

    ``deadline`` is an absolute ``time.monotonic()`` instant. ``select_fn`` is
    injectable so the real helper is unit-testable without a socket or server.
    """
    while True:
        if time.monotonic() >= deadline:
            raise SqlDeadlineExceeded(
                "client deadline expired before polling the connection"
            )
        state = conn.poll()
        if state == psycopg2.extensions.POLL_OK:
            if time.monotonic() >= deadline:
                raise SqlDeadlineExceeded(
                    "client deadline expired before accepting POLL_OK"
                )
            return
        if state not in (psycopg2.extensions.POLL_READ, psycopg2.extensions.POLL_WRITE):
            raise psycopg2.OperationalError("unexpected async poll state: %r" % (state,))
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            raise SqlDeadlineExceeded(
                "client deadline expired while waiting to %s"
                % ("read" if state == psycopg2.extensions.POLL_READ else "write")
            )
        fileno = conn.fileno()
        if state == psycopg2.extensions.POLL_READ:
            select_fn([fileno], [], [], remaining)
        else:
            select_fn([], [fileno], [], remaining)


class Harness:
    def __init__(self) -> None:
        self.bin = ""
        self.bin_real = ""
        self.bin_sha = ""
        self.bin_sha_after = ""
        self.binary_hash_ok = True
        self.version_out = ""
        self.evidence_dir = ""
        self.work_dir = ""
        self.registry: Dict[int, Case] = {}
        self.redact_values: List[str] = []
        self.cleanup_failures: List[str] = []
        self.cases: List[Case] = []

    def finalize_binary_binding(self) -> None:
        """Re-hash the exact binary after the suite and assert it is unchanged.

        This binds the evidence to one executable hash only; it is not a source
        binding. The source manifest is produced by root outside this harness,
        so no stronger provenance claim is made here.
        """
        if not self.bin:
            return
        self.bin_sha_after = sha256_file(self.bin)
        self.binary_hash_ok = self.bin_sha_after == self.bin_sha

    # ── setup ────────────────────────────────────────────────────────────
    def prepare(self) -> None:
        if os.uname().sysname != "Linux" or not os.access("/proc/self/cmdline", os.R_OK):
            raise Unavailable("requires Linux with /proc mounted")
        if psycopg2 is None:
            raise Unavailable("python3 psycopg2 is not importable (needed for the SELECT 1 proof)")

        raw_bin = os.environ.get("HELIOSDB_NANO_BIN", "")
        if not raw_bin:
            raise Unavailable("HELIOSDB_NANO_BIN is required (explicit absolute path to a built heliosdb-nano)")
        if not os.path.isabs(raw_bin):
            raise Unavailable("HELIOSDB_NANO_BIN must be an absolute path, got %r" % raw_bin)
        if not (os.path.isfile(raw_bin) and os.access(raw_bin, os.X_OK)):
            raise Unavailable("HELIOSDB_NANO_BIN is not an executable regular file: %s" % raw_bin)
        self.bin = raw_bin
        self.bin_real = os.path.realpath(raw_bin)
        self.bin_sha = sha256_file(raw_bin)

        evidence = os.environ.get("HELIOSDB_PROC_EVIDENCE_DIR", "")
        if not evidence:
            raise Unavailable("HELIOSDB_PROC_EVIDENCE_DIR is required (caller-selected evidence directory)")
        if not os.path.isabs(evidence):
            raise Unavailable("HELIOSDB_PROC_EVIDENCE_DIR must be an absolute path, got %r" % evidence)
        os.makedirs(evidence, exist_ok=True)
        self.evidence_dir = evidence

        # Prove the binary runs and record what it claims to be.
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
            raise Unavailable("%s --version exited %d: %s" % (self.bin, proc.returncode, proc.stdout.decode("utf-8", "replace").strip()))
        self.version_out = proc.stdout.decode("utf-8", "replace").strip()
        self._write("version.txt", "binary: %s\nsha256: %s\nversion: %s\n" % (self.bin, self.bin_sha, self.version_out))

        self.work_dir = os.path.join(self.evidence_dir, "work")
        os.makedirs(self.work_dir, exist_ok=True)
        os.chmod(self.work_dir, 0o700)

    def _write(self, name: str, text: str) -> str:
        path = os.path.join(self.evidence_dir, name)
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
            case.argv = [p.decode("utf-8", "replace") for p in case.raw_cmdline.split(b"\0") if p]
            case.environ = proc_environ(case.pid)
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

    def scan_orphans(self, node_id: str) -> List[Orphan]:
        """Find live children carrying our unique node-id marker.

        Each hit is frozen with its starttime/executable so every later signal
        can be re-validated; a same-binary process with a different marker is
        ignored and never adopted.
        """
        hits: List[Orphan] = []
        for entry in os.listdir("/proc"):
            if not entry.isdigit():
                continue
            pid = int(entry)
            try:
                argv = proc_cmdline(pid)
                if not self._argv_pair(argv, "--node-id", node_id):
                    continue
                if "start" not in argv:
                    continue
                exe = proc_exe(pid)
                if exe != self.bin_real:
                    continue
                starttime = proc_starttime(pid)
            except OSError:
                continue
            hits.append(Orphan(pid, node_id, starttime, exe))
        return hits

    def _wait_gone(self, pid: int, seconds: float) -> None:
        deadline = time.time() + seconds
        while time.time() < deadline and is_alive(pid):
            time.sleep(POLL_INTERVAL)

    def _signal_and_wait(self, pid: int, node_id: str, starttime: str, exe: str, label: str) -> None:
        """TERM then KILL, re-validating all three identity proofs before each.

        A PID that no longer matches is never signalled. A cleanly vanished
        process is not a failure; a still-owned process we cannot signal, or one
        that survives the waits, or a live PID whose identity changed, is
        recorded as unresolved cleanup — never silently dropped.
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
            return  # exited between validation and signal: normally gone
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

    def stop_case(self, case: Case) -> None:
        self.registry.pop(case.pid, None)
        self._signal_and_wait(case.pid, case.node_id, case.starttime, case.exe, "case %s" % case.name)

    def stop_orphan(self, orphan: Orphan) -> None:
        self._signal_and_wait(
            orphan.pid, orphan.node_id, orphan.starttime, orphan.exe, "orphan node-id %s" % orphan.node_id
        )

    def cleanup(self) -> None:
        for case in list(self.registry.values()):
            self.stop_case(case)
        for case in self.cases:
            for orphan in case.orphans:
                self.stop_orphan(orphan)
        # The scratch dir holds pidfiles and readable secret fixtures. Logs and
        # the manifest live in the evidence root and are never removed.
        if self.work_dir:
            shutil.rmtree(self.work_dir, ignore_errors=True)

    # ── launch ──────────────────────────────────────────────────────────
    def launch(self, name: str, auth: str, extra_args: List[str], secret_env: Dict[str, str]) -> Case:
        case = Case(name)
        case.pg_port = ephemeral_port()
        case.http_port = ephemeral_port()
        while case.http_port == case.pg_port:
            case.http_port = ephemeral_port()
        case.node_id = str(uuid.uuid4())
        case.pidfile = os.path.join(self.work_dir, name + ".pid")
        case.logfile = os.path.join(self.evidence_dir, name + ".launch.log")

        env = {k: v for k, v in os.environ.items() if k not in SECRET_ENV_KEYS}
        env.update(secret_env)

        argv = [
            self.bin,
            "start",
            "--daemon",
            "--memory",
            "--listen",
            "127.0.0.1",
            "--port",
            str(case.pg_port),
            "--http-port",
            str(case.http_port),
            "--pid-file",
            case.pidfile,
            "--node-id",
            case.node_id,
            "--auth",
            auth,
        ] + list(extra_args)

        try:
            os.remove(case.pidfile)
        except OSError:
            pass

        try:
            proc = subprocess.run(
                argv,
                env=env,
                stdout=subprocess.PIPE,
                stderr=subprocess.STDOUT,
                timeout=LAUNCH_TIMEOUT,
            )
            case.launch_rc = proc.returncode
            case.launch_out = proc.stdout or b""
        except subprocess.TimeoutExpired as exc:
            case.timed_out = True
            case.launch_out = exc.stdout or b""
        self._write(os.path.basename(case.logfile), self.redact(case.launch_out.decode("utf-8", "replace")))

        case.pidfile_seen = os.path.exists(case.pidfile)

        if case.timed_out:
            case.orphans = self.scan_orphans(case.node_id)
            case.stray = bool(case.orphans)
            if case.orphans:
                case.check(False, "launcher timed out; recovered orphan(s) for cleanup: %s" % [o.pid for o in case.orphans])
            else:
                self.cleanup_failures.append("launcher for %s timed out and no orphan matched node-id %s" % (name, case.node_id))
                case.check(False, "launcher timed out and no orphan could be identified")
            return case

        # Read the pidfile regardless of the launcher's exit code: a failing
        # launcher can still have spawned (or be racing) a child, and a leftover
        # pidfile is itself a failure for an expected-rejection case.
        if case.pidfile_seen:
            try:
                with open(case.pidfile, "r", encoding="utf-8") as fh:
                    parsed = int(fh.read().strip())
                case.pid = parsed if parsed > 0 else 0
            except (OSError, ValueError):
                case.pid = 0

        if case.launch_rc == 0 and case.pidfile_seen and case.pid > 0 and self.capture_identity(case):
            # Do NOT register/signal until the executable, the unique node-id
            # pair, AND the starttime all match; a failed assertion grants no
            # kill authority.
            exe_ok = case.exe == self.bin_real
            marker_ok = self._argv_pair(case.argv, "--node-id", case.node_id)
            start_ok = case.starttime != ""
            case.check(exe_ok, "child /proc/%d/exe is the target binary" % case.pid)
            case.check(marker_ok, "child argv carries the unique --node-id marker")
            case.check(start_ok, "child /proc/%d/stat starttime captured" % case.pid)
            if exe_ok and marker_ok and start_ok:
                case.check("start" in case.argv, "child argv contains 'start'")
                self.registry[case.pid] = case
                case.identity_proven = True
                return case
            case.check(False, "identity validation failed; pid %d is not signalled" % case.pid)

        # Launcher failed, or "succeeded" without a proven child. Recover any
        # live child by its unique marker for identity-safe cleanup; never adopt
        # an unverified PID.
        case.orphans = self.scan_orphans(case.node_id)
        case.stray = bool(case.orphans)
        if case.launch_rc == 0:
            if not case.pidfile_seen:
                case.check(False, "launcher reported success but wrote no pidfile")
            else:
                case.check(False, "launcher reported success but no proven child was captured")
            if not case.orphans and case.pid <= 0:
                self.cleanup_failures.append("%s: launcher succeeded but no pidfile and no orphan matched" % name)
        return case

    # ── listener ownership (Linux) ──────────────────────────────────────
    @staticmethod
    def listening_inodes(port: int) -> set:
        """Inodes of LISTEN sockets on `port`, from /proc/net/tcp{,6}."""
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
        LISTEN socket for `port`. Proves the child owns the listener rather
        than merely that some child is alive near an ephemeral port."""
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
                if target[len("socket:[") : -1] in wanted:
                    return True
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

    # ── readiness + proofs ──────────────────────────────────────────────
    def wait_ready(self, case: Case) -> bool:
        deadline = time.time() + READY_TIMEOUT
        while time.time() < deadline:
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

    def _sql_connect(self, case: Case, password: str):
        """Open an ``async_=True`` connection, driven to POLL_OK or the deadline.

        Returns only once the server has accepted the connection, so a wrong
        password surfaces here as a ``psycopg2.Error`` (SQLSTATE 28 / "password
        authentication failed"), while a slow, black-holed, or hung server
        surfaces as :class:`SqlDeadlineExceeded` and can never masquerade as an
        authentication rejection. ``connect_timeout`` is retained as a libpq
        backstop, but the bounded select/poll loop is what guarantees progress.
        """
        deadline = time.monotonic() + SQL_CONNECT_TIMEOUT
        conn = psycopg2.connect(
            host="127.0.0.1",
            port=case.pg_port,
            dbname=SQL_DB,
            user=SQL_USER,
            password=password,
            connect_timeout=SQL_CONNECT_TIMEOUT,
            sslmode="disable",
            async_=True,
        )
        try:
            poll_async_connection(conn, deadline)
        except Exception:
            try:
                conn.close()
            except Exception:
                pass
            raise
        return conn

    @staticmethod
    def _sql_select1_rows(conn) -> list:
        """Run ``SELECT 1`` on an async connection under its own client deadline.

        The statement gets a fresh monotonic budget (measured from just before
        ``execute``), independent of the server's ``statement_timeout``. Returns
        the exact fetched rows; a server error propagates as ``psycopg2.Error``
        and a stalled statement raises :class:`SqlDeadlineExceeded`.
        """
        deadline = time.monotonic() + SQL_STATEMENT_TIMEOUT
        cur = conn.cursor()
        try:
            cur.execute("SELECT 1")
            poll_async_connection(conn, deadline)
            return cur.fetchall()
        finally:
            try:
                cur.close()
            except Exception:
                pass

    def sql_expect_ok(self, case: Case, password: str, label: str) -> None:
        conn = None
        try:
            conn = self._sql_connect(case, password)
            rows = self._sql_select1_rows(conn)
            case.check(rows == [(1,)], "%s: SELECT 1 returned exactly [(1,)] (got %r)" % (label, rows))
        except Exception as exc:  # noqa: BLE001 - any failure means correct creds did not work
            case.check(False, "%s: correct credentials were not accepted: %s" % (label, exc))
        finally:
            if conn is not None:
                try:
                    conn.close()
                except Exception:
                    pass

    def sql_expect_rejected(self, case: Case, password: str, label: str) -> None:
        """A wrong password must be rejected by *authentication* (SQLSTATE 28),
        not by a transport error, crash, or timeout; the child/listener must
        survive the attempt.

        Authentication is classified **only** while establishing the connection.
        If ``_sql_connect`` returns, the server accepted the connection, so the
        wrong-password negative must fail regardless of any later query-phase
        error (including one that happens to carry an auth-flavoured message).
        """
        conn = None
        auth_rejected = False
        detail = "no error"
        try:
            conn = self._sql_connect(case, password)
        except psycopg2.Error as exc:  # server replied with an auth or other error
            code = getattr(exc, "pgcode", None)
            if code is None:
                diag = getattr(exc, "diag", None)
                code = getattr(diag, "sqlstate", None)
            message = str(exc).lower()
            auth_rejected = (
                (code is not None and str(code).startswith("28"))
                or "authentication failed" in message
                or "password authentication failed" in message
                or "invalid_password" in message
            )
            detail = "pgcode=%s exc=%s: %s" % (code, type(exc).__name__, exc)
        except Exception as exc:  # transport error / timeout / crash
            detail = "non-auth failure %s: %s" % (type(exc).__name__, exc)
        else:
            # The connect completed: authentication succeeded. Everything from
            # here on is a query-phase outcome and must NEVER be reclassified as
            # an authentication rejection.
            detail = "connection accepted (authentication succeeded) with the wrong password"
            try:
                rows = self._sql_select1_rows(conn)
                detail += "; SELECT 1 returned %r" % (rows,)
            except Exception as exc:  # noqa: BLE001 - query-phase, not auth classification
                detail += "; later query error %s: %s" % (type(exc).__name__, exc)
        finally:
            if conn is not None:
                try:
                    conn.close()
                except Exception:
                    pass
        if auth_rejected:
            case.check(True, "%s: wrong credentials rejected by authentication [%s]" % (label, detail))
        else:
            case.check(False, "%s: wrong credentials NOT rejected by authentication [%s]" % (label, detail))
        case.check(
            self._identity_ok(case.pid, case.node_id, case.starttime, case.exe),
            "%s: child still alive after the wrong-password attempt" % label,
        )
        case.check(
            self.child_owns_port(case.pid, case.pg_port),
            "%s: child still owns the SQL listener after the wrong-password attempt" % label,
        )

    def argv_hides(self, case: Case, secret: str, label: str) -> None:
        case.check(secret.encode("utf-8") not in case.raw_cmdline, "%s: secret absent from NUL-delimited child argv" % label)
        if secret:
            self.redact_values.append(secret)

    def argv_lacks_flag(self, case: Case, flag: str, label: str) -> None:
        case.check(flag not in case.argv, "%s: child argv has no %s flag" % (label, flag))

    def argv_has_pair(self, case: Case, flag: str, value: str, label: str) -> None:
        present = any(case.argv[i] == flag and i + 1 < len(case.argv) and case.argv[i + 1] == value for i in range(len(case.argv)))
        case.check(present, "%s: child argv forwards %s %s" % (label, flag, value))

    def env_retains(self, case: Case, key: str, value: str, label: str) -> None:
        case.check(case.environ.get(key) == value, "%s: child resolved channel %s retains the credential" % (label, key))

    def mcp_http_auth(self, case: Case, token: str, label: str) -> None:
        if not self.child_owns_port(case.pid, case.http_port):
            case.check(
                False,
                "%s: child does NOT own the HTTP listening socket on port %d; skipping MCP requests" % (label, case.http_port),
            )
            return
        case.check(True, "%s: child owns the HTTP listening socket on port %d" % (label, case.http_port))
        body = json.dumps({"jsonrpc": "2.0", "id": 1, "method": "tools/list", "params": {}}).encode("utf-8")
        status_no = self._mcp_request(case.http_port, None, body)
        if status_no == 404:
            raise Unavailable(
                "HELIOSDB_PROC_MCP_HTTP=1 was requested but this binary does not mount /mcp "
                "(built without the mcp-endpoint feature); token auth cannot be proven"
            )
        status_wrong = self._mcp_request(case.http_port, "wrong-" + token, body)
        status_ok = self._mcp_request(case.http_port, token, body)
        case.check(status_no == 401, "%s: /mcp without a token -> 401 (got %s)" % (label, status_no))
        case.check(status_wrong == 401, "%s: /mcp with a wrong token -> 401 (got %s)" % (label, status_wrong))
        case.check(status_ok == 200, "%s: /mcp with the valid token -> 200 (got %s)" % (label, status_ok))

    @staticmethod
    def _mcp_request(port: int, token: Optional[str], body: bytes) -> int:
        req = urllib.request.Request("http://127.0.0.1:%d/mcp" % port, data=body, method="POST")
        req.add_header("Content-Type", "application/json")
        if token is not None:
            req.add_header("Authorization", "Bearer " + token)
        try:
            with urllib.request.urlopen(req, timeout=HTTP_TIMEOUT) as resp:
                return int(resp.status)
        except urllib.error.HTTPError as exc:
            return int(exc.code)
        except OSError:
            return 0

    # ── expected-failure classification (R7) ────────────────────────────
    def expect_config_error(self, case: Case, needles: List[str], label: str) -> None:
        out = case.launch_out.decode("utf-8", "replace")
        if case.timed_out:
            case.check(False, "%s: launcher timed out instead of reporting a config error" % label)
            return
        if re.search(r"(?i)address already in use|failed to bind|EADDRINUSE", out):
            case.check(False, "%s: failed on an unrelated port bind instead of the config error" % label)
            return
        if case.launch_rc == 0:
            case.check(False, "%s: startup succeeded where a config error was expected" % label)
            return
        # A rejection must not leave a daemon or a pidfile behind, whether or
        # not the pid could be parsed (checked unconditionally, not only when a
        # pid was read).
        if case.pidfile_seen:
            case.check(False, "%s: startup failed but a pidfile was left behind" % label)
        if case.stray or case.orphans:
            case.check(False, "%s: startup failed but a child/daemon was left running" % label)
        matched = any(needle in out for needle in needles)
        case.check(matched, "%s: config error message matched (%s)" % (label, " / ".join(needles)))
        if not matched:
            eprint("---- %s output ----\n%s" % (label, out))


def build_cases(h: Harness) -> None:
    """Run every case. Assertions land on each Case; failures are summarised."""
    secret = "hdb-regress-pw-%d" % os.getpid()
    wrong = "hdb-regress-wrong-%d" % os.getpid()
    token = "hdb-regress-token-%d" % os.getpid()
    h.redact_values.extend([secret, wrong, token])

    pw_file = os.path.join(h.work_dir, "password.txt")
    with open(pw_file, "w", encoding="utf-8") as fh:
        fh.write(secret)
    os.chmod(pw_file, 0o600)

    env_pw_file = os.path.join(h.work_dir, "env-password.txt")
    with open(env_pw_file, "w", encoding="utf-8") as fh:
        fh.write(secret)
    os.chmod(env_pw_file, 0o600)

    def expect_started(case: Case) -> bool:
        if case.timed_out:
            case.check(False, "%s: launcher timed out" % case.name)
            return False
        if case.launch_rc != 0:
            case.check(False, "%s: launcher exited %s" % (case.name, case.launch_rc))
            return False
        if not case.pid:
            case.check(False, "%s: launcher succeeded but no live child pid was captured" % case.name)
            return False
        if not case.identity_proven:
            # launch() already recorded why; never probe or signal an unproven PID.
            return False
        return True

    def proof(case: Case, with_sql: bool) -> None:
        if not h.wait_ready(case):
            h.assert_owns_listener(case, case.pg_port, case.name)
            case.check(False, "%s: daemon never accepted connections" % case.name)
            h.stop_case(case)
            return
        h.assert_owns_listener(case, case.pg_port, case.name)
        if with_sql:
            h.sql_expect_ok(case, secret, case.name)
            h.sql_expect_rejected(case, wrong, case.name)
        h.stop_case(case)

    # 1. explicit --password-file: child forwards the path, SQL auth works.
    case = h.launch("01-password-file", "scram-sha-256", ["--password-file", pw_file], {})
    h.cases.append(case)
    if expect_started(case):
        h.argv_has_pair(case, "--password-file", pw_file, case.name)
        h.argv_hides(case, secret, case.name)
        h.argv_lacks_flag(case, "--password", case.name)
        proof(case, with_sql=True)

    # 2. env HELIOSDB_PASSWORD: no literal on child argv, SQL auth works.
    case = h.launch("02-env-password", "scram-sha-256", [], {"HELIOSDB_PASSWORD": secret})
    h.cases.append(case)
    if expect_started(case):
        h.argv_hides(case, secret, case.name)
        h.argv_lacks_flag(case, "--password", case.name)
        proof(case, with_sql=True)

    # 3. legacy literal --password: off the child argv, but still authenticates.
    case = h.launch("03-legacy-literal", "scram-sha-256", ["--password", secret], {})
    h.cases.append(case)
    if expect_started(case):
        h.argv_hides(case, secret, case.name)
        h.argv_lacks_flag(case, "--password", case.name)
        proof(case, with_sql=True)

    # 4. env HELIOSDB_PASSWORD_FILE: path forwarded, child reads, SQL auth works.
    case = h.launch("04-env-password-file", "scram-sha-256", [], {"HELIOSDB_PASSWORD_FILE": env_pw_file})
    h.cases.append(case)
    if expect_started(case):
        h.argv_has_pair(case, "--password-file", env_pw_file, case.name)
        h.argv_hides(case, secret, case.name)
        h.argv_lacks_flag(case, "--password", case.name)
        proof(case, with_sql=True)

    # 5. trust + explicit password: SQL trust is NOT authentication, so we only
    #    prove the configured credential survives in the child's resolved
    #    channel (environment) while staying off argv.
    case = h.launch("05-trust-plus-password", "trust", ["--password", secret], {})
    h.cases.append(case)
    if expect_started(case):
        h.argv_hides(case, secret, case.name)
        h.argv_lacks_flag(case, "--password", case.name)
        h.env_retains(case, "HELIOSDB_PASSWORD", secret, case.name)
        case.check("HELIOSDB_PASSWORD_FILE" not in case.environ, "%s: no stale file source in the child env" % case.name)
        h.stop_case(case)

    # 6. MCP bearer token: off argv; env channel retained. SQL/argv alone cannot
    #    prove token auth, so the HTTP 401/200 gate is opt-in (feature-gated).
    case = h.launch("06-mcp-token", "trust", ["--mcp-token", token], {})
    h.cases.append(case)
    if expect_started(case):
        h.argv_hides(case, token, case.name)
        h.argv_lacks_flag(case, "--mcp-token", case.name)
        h.env_retains(case, "HELIOSDB_MCP_TOKEN", token, case.name)
        if os.environ.get("HELIOSDB_PROC_MCP_HTTP") == "1":
            h.mcp_http_auth(case, token, case.name)
        h.stop_case(case)

    # 7. configuration errors must fail closed, distinctly from a timeout/crash
    #    or an unrelated bind failure.
    missing = os.path.join(h.work_dir, "does-not-exist.txt")
    case = h.launch("07-missing-password-file", "scram-sha-256", ["--password-file", missing], {})
    h.expect_config_error(case, ["Cannot read --password-file", "No such file"], case.name)
    h.cases.append(case)

    empty = os.path.join(h.work_dir, "empty.txt")
    open(empty, "w", encoding="utf-8").close()
    case = h.launch("08-empty-password-file", "scram-sha-256", ["--password-file", empty], {})
    h.expect_config_error(case, ["is empty"], case.name)
    h.cases.append(case)

    # Honest naming: the file is removed *before* the parent resolves it, so
    # this is the parent-resolution missing-file path, not the parent-resolve
    # vs child-re-read race. That race needs a fixture that can interleave the
    # child's startup read; it is NOT exercised here.
    case = h.launch("09-deleted-before-parent-read", "scram-sha-256", ["--password-file", missing], {})
    h.expect_config_error(case, ["Cannot read --password-file", "No such file"], case.name)
    h.cases.append(case)

    case = h.launch("10-empty-literal-password", "scram-sha-256", ["--password", ""], {})
    h.expect_config_error(case, ["--password value is empty"], case.name)
    h.cases.append(case)

    case = h.launch("11-empty-mcp-token", "trust", ["--mcp-token", ""], {})
    h.expect_config_error(case, ["--mcp-token value is empty"], case.name)
    h.cases.append(case)

    case = h.launch("12-empty-password-file-arg", "scram-sha-256", ["--password-file", ""], {})
    h.expect_config_error(case, ["empty path"], case.name)
    h.cases.append(case)


def write_manifest(h: Harness, unavailable: Optional[str]) -> None:
    cases = []
    for case in h.cases:
        cases.append(
            {
                "name": case.name,
                "pid": case.pid,
                "pg_port": case.pg_port,
                "http_port": case.http_port,
                "node_id": case.node_id,
                "launch_rc": case.launch_rc,
                "timed_out": case.timed_out,
                "pidfile": case.pidfile,
                "pidfile_seen": case.pidfile_seen,
                "stray": case.stray,
                "identity_proven": case.identity_proven,
                "log": os.path.basename(case.logfile),
                "starttime": case.starttime,
                "exe": case.exe,
                "argv": [h.redact(a) for a in case.argv],
                "env_flags": {
                    "HELIOSDB_PASSWORD_present": "HELIOSDB_PASSWORD" in case.environ,
                    "HELIOSDB_PASSWORD_FILE_present": "HELIOSDB_PASSWORD_FILE" in case.environ,
                    "HELIOSDB_MCP_TOKEN_present": "HELIOSDB_MCP_TOKEN" in case.environ,
                },
                "checks": [{"ok": ok, "label": h.redact(label)} for ok, label in case.checks],
                "failed": case.failed(),
            }
        )
    manifest = {
        "at": now(),
        "binary": h.bin,
        "binary_realpath": h.bin_real,
        "binary_sha256": h.bin_sha,
        "binary_sha256_after": h.bin_sha_after,
        "binary_hash_ok": h.binary_hash_ok,
        "version": h.version_out,
        "source_binding": "executable hash only; source manifest produced by root outside this harness",
        "gate": {"HELIOSDB_PROC_REGRESSION": os.environ.get("HELIOSDB_PROC_REGRESSION"), "HELIOSDB_PROC_MCP_HTTP": os.environ.get("HELIOSDB_PROC_MCP_HTTP")},
        "unavailable": unavailable,
        "cleanup_failures": h.cleanup_failures,
        "cases": cases,
    }
    with open(os.path.join(h.evidence_dir, "manifest.json"), "w", encoding="utf-8") as fh:
        json.dump(manifest, fh, indent=2)


def main() -> int:
    if os.environ.get("HELIOSDB_PROC_REGRESSION") != "1":
        eprint("skip: gated real-process harness; set HELIOSDB_PROC_REGRESSION=1 to run")
        return EXIT_OK

    h = Harness()
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
        if h.evidence_dir:
            write_manifest(h, unavailable)

    if unavailable is not None:
        return EXIT_UNAVAILABLE

    failures = [c for c in h.cases if c.failed()]
    for case in h.cases:
        for ok, label in case.checks:
            prefix = "ok  " if ok else "FAIL"
            eprint("%s %s" % (prefix, label))
    eprint(
        "%s binary sha256 unchanged before/after suite (%s)"
        % ("ok  " if h.binary_hash_ok else "FAIL", h.bin_sha[:16])
    )
    if h.cleanup_failures:
        for item in h.cleanup_failures:
            eprint("FAIL cleanup: %s" % item)
    if failures or h.cleanup_failures or not h.binary_hash_ok:
        eprint("RESULT: FAIL (%d case(s) with failures)" % len(failures))
        return EXIT_FAIL
    eprint("RESULT: PASS — secrets stayed off child argv and credentials authenticated")
    return EXIT_OK


if __name__ == "__main__":
    sys.exit(main())
