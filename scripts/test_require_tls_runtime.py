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
  the exact ``--node-id`` UUID, and it is not the launcher PID.
* ``--require-tls`` without ``--tls-cert``/``--tls-key`` is refused *before* the
  data directory is created or any listener is bound.

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
as ``require-tls-manifest.json`` under the evidence directory. Private fixtures
(the self-signed cert/key, the SCRAM password file, PID files and the UDS
directory) live in a ``private/`` subdirectory and are removed after capture;
only the logs and manifest persist.

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
foreground cases; the daemon case proves refusal by the connection failing and
proves the same PID still serves TLS.
"""

import hashlib
import json
import os
import shutil
import signal
import socket
import subprocess
import sys
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
SQL_STATEMENT_TIMEOUT_MS = 5000
TERM_WAIT = 5.0
KILL_WAIT = 5.0
POLL_INTERVAL = 0.1

# Credential channels that must not survive into a launched child.
SECRET_ENV_KEYS = ("HELIOSDB_PASSWORD", "HELIOSDB_PASSWORD_FILE", "HELIOSDB_MCP_TOKEN")

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


class TlsHarness:
    def __init__(self) -> None:
        self.bin = ""
        self.bin_real = ""
        self.bin_sha = ""
        self.bin_sha_after = ""
        self.binary_hash_ok = True
        self.version_out = ""
        self.evidence_dir = ""
        self.private_dir = ""
        self.cert = ""
        self.key = ""
        self.pw_file = ""
        self.password = ""
        self.wrong_password = ""
        self.registry: Dict[int, Case] = {}
        self.redact_values: List[str] = []
        self.cleanup_failures: List[str] = []
        self.cases: List[Case] = []

    # ── setup ────────────────────────────────────────────────────────────
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
        self.evidence_dir = evidence

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

        # Private fixtures: cert/key, password, PID files and UDS sockets. 0700
        # and removed after capture so only logs + manifest persist.
        self.private_dir = os.path.join(self.evidence_dir, "private")
        os.makedirs(self.private_dir, exist_ok=True)
        os.chmod(self.private_dir, 0o700)

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
        deadline = time.time() + seconds
        while time.time() < deadline and is_alive(pid):
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

    def stop_case(self, case: Case) -> None:
        self.registry.pop(case.pid, None)
        if case.identity_proven:
            self._signal_and_wait(case.pid, case.node_id, case.starttime, case.exe, "case %s" % case.name)
        elif case.proc is not None and case.proc.pid == case.pid:
            # Foreground child we spawned directly: the live Popen handle is the
            # ownership proof even if /proc identity capture itself failed.
            try:
                case.proc.terminate()
                try:
                    case.proc.wait(timeout=TERM_WAIT)
                except subprocess.TimeoutExpired:
                    case.proc.kill()
            except Exception:
                pass
        else:
            # Daemon worker: only an identity-validated signal is ever sent.
            self._signal_and_wait(case.pid, case.node_id, case.starttime, case.exe, "case %s" % case.name)
        if case.proc is not None:
            try:
                case.proc.wait(timeout=KILL_WAIT)
            except Exception:
                pass

    def cleanup(self) -> None:
        for case in list(self.registry.values()):
            self.stop_case(case)
        # Private fixtures (cert/key, password, PID files, UDS sockets) are
        # removed; retained logs and the manifest are never touched.
        if self.private_dir:
            shutil.rmtree(self.private_dir, ignore_errors=True)

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
        case.logfile = os.path.join(self.evidence_dir, name + ".launch.log")
        return case

    def _clean_env(self) -> Dict[str, str]:
        return {k: v for k, v in os.environ.items() if k not in SECRET_ENV_KEYS}

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
        except OSError:
            pass

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

    def launch_foreground(self, name: str, extra_args: List[str]) -> Case:
        case = self._new_case(name)
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
        case.pid = case.proc.pid
        if self.capture_identity(case):
            self._check_identity(case)
        else:
            case.check(False, "%s: could not read /proc/%d identity after launch" % (name, case.pid))
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

        launcher = subprocess.Popen(
            argv,
            env=self._clean_env(),
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
        )
        case.launcher_pid = launcher.pid
        try:
            out, _ = launcher.communicate(timeout=LAUNCH_TIMEOUT)
        except subprocess.TimeoutExpired:
            launcher.kill()
            out, _ = launcher.communicate()
            case.timed_out = True
        case.launch_rc = launcher.returncode
        case.launch_out = out or b""
        self._write_launch_log(case)

        case.pidfile_seen = os.path.exists(case.pidfile)
        if case.pidfile_seen:
            try:
                with open(case.pidfile, "r", encoding="utf-8") as fh:
                    parsed = int(fh.read().strip())
                case.pid = parsed if parsed > 0 else 0
            except (OSError, ValueError):
                case.pid = 0

        # Capture the worker identity even if the launcher misbehaved, so a
        # worker that is still running can be cleaned up with proof.
        if case.pid > 0 and self.capture_identity(case):
            case.check(
                case.pid != case.launcher_pid,
                "%s: PID-file worker %d differs from the launcher PID" % (name, case.pid),
            )
            self._check_identity(case)

        if case.timed_out:
            case.check(False, "%s: daemon launcher timed out" % name)
            return case
        if case.launch_rc != 0:
            case.check(False, "%s: daemon launcher exited %s" % (name, case.launch_rc))
            return case
        if not case.pidfile_seen or case.pid <= 0:
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

    def wait_path(self, path: str, timeout: float = READY_TIMEOUT) -> bool:
        deadline = time.time() + timeout
        while time.time() < deadline:
            if os.path.exists(path):
                return True
            time.sleep(POLL_INTERVAL)
        return False

    def wait_log_contains(self, case: Case, needle: str, timeout: float = LOG_TIMEOUT) -> bool:
        deadline = time.time() + timeout
        while time.time() < deadline:
            try:
                with open(case.logfile, "r", encoding="utf-8", errors="replace") as fh:
                    if needle in fh.read():
                        return True
            except OSError:
                pass
            time.sleep(POLL_INTERVAL)
        return False

    # ── SQL proofs (bounded) ────────────────────────────────────────────
    def _connect(
        self,
        host: str,
        port: int,
        password: str,
        sslmode: Optional[str],
    ):
        kwargs = {
            "host": host,
            "port": port,
            "dbname": SQL_DB,
            "user": SQL_USER,
            "password": password,
            "connect_timeout": SQL_CONNECT_TIMEOUT,
            "options": "-c statement_timeout=%d" % SQL_STATEMENT_TIMEOUT_MS,
        }
        if sslmode is not None:
            kwargs["sslmode"] = sslmode
        return psycopg2.connect(**kwargs)

    def _select1(self, case: Case, host: str, password: str, sslmode: Optional[str], label: str) -> None:
        conn = None
        try:
            conn = self._connect(host, case.pg_port, password, sslmode)
            cur = conn.cursor()
            cur.execute("SELECT 1")
            rows = cur.fetchall()
            cur.close()
            case.check(rows == [(1,)], "%s: SELECT 1 returned exactly [(1,)] (got %r)" % (label, rows))
        except Exception as exc:  # noqa: BLE001 - any failure means the query did not work
            case.check(False, "%s: SELECT 1 failed: %s: %s" % (label, type(exc).__name__, exc))
        finally:
            if conn is not None:
                try:
                    conn.close()
                except Exception:
                    pass

    def select1_tls(self, case: Case, label: str) -> None:
        self._select1(case, "127.0.0.1", self.password, "require", label + " [tls]")

    def select1_plaintext(self, case: Case, label: str) -> None:
        self._select1(case, "127.0.0.1", self.password, "disable", label + " [plaintext]")

    def select1_uds(self, case: Case, label: str) -> None:
        self._select1(case, case.uds_dir, self.password, None, label + " [uds]")

    def expect_auth_rejected(
        self,
        case: Case,
        password: str,
        label: str,
        host: str = "127.0.0.1",
        sslmode: Optional[str] = "require",
        transport: str = "tls",
    ) -> None:
        """A wrong password must be rejected by *authentication* (SQLSTATE 28),
        not by a transport error, crash, or timeout; the child/listener must
        survive the attempt."""
        conn = None
        auth_rejected = False
        detail = "no error"
        try:
            conn = self._connect(host, case.pg_port, password, sslmode)
            cur = conn.cursor()
            cur.execute("SELECT 1")
            rows = cur.fetchall()
            cur.close()
            detail = "connection accepted and SELECT 1 returned %r" % (rows,)
        except psycopg2.Error as exc:
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
        finally:
            if conn is not None:
                try:
                    conn.close()
                except Exception:
                    pass
        case.check(auth_rejected, "%s: wrong credentials rejected by authentication via %s [%s]" % (label, transport, detail))
        case.check(self.still_ours(case), "%s: child still alive after the wrong-password attempt" % label)
        case.check(
            self.child_owns_port(case.pid, case.pg_port) or bool(case.uds_dir),
            "%s: listener survived the wrong-password attempt" % label,
        )

    def expect_plaintext_refused(self, case: Case, label: str, expect_log: bool = True) -> None:
        """Under --require-tls a plaintext TCP client (sslmode=disable, so no
        SSLRequest is sent) must be refused with the exact server diagnostic,
        and the SAME PID must remain bound and healthy."""
        refused = False
        detail = ""
        try:
            conn = self._connect("127.0.0.1", case.pg_port, self.password, "disable")
            try:
                cur = conn.cursor()
                cur.execute("SELECT 1")
                detail = "plaintext SELECT 1 returned %r" % (cur.fetchall(),)
            finally:
                conn.close()
        except Exception as exc:  # noqa: BLE001 - refusal is the expected outcome
            refused = True
            detail = "%s: %s" % (type(exc).__name__, exc)
        case.check(refused, "%s: plaintext PostgreSQL connection was refused [%s]" % (label, detail))
        if expect_log:
            if self.wait_log_contains(case, REQUIRE_TLS_REFUSAL):
                case.check(True, "%s: server log records %r" % (label, REQUIRE_TLS_REFUSAL))
            else:
                case.check(False, "%s: server log does not contain %r" % (label, REQUIRE_TLS_REFUSAL))
        case.check(self.still_ours(case), "%s: SAME PID is alive after the plaintext refusal" % label)
        case.check(
            self.child_owns_port(case.pid, case.pg_port),
            "%s: SAME PID still owns the listener after the plaintext refusal" % label,
        )

    # ── expected startup rejection before any mutation ──────────────────
    def expect_refused_before_mutation(
        self,
        name: str,
        extra_args: List[str],
        data_dir: str,
        needles: List[str],
    ) -> Case:
        """A TLS config error must exit non-zero, say why, and leave no data
        directory and no listener behind."""
        case = self._new_case(name)
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
        ] + list(extra_args)
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
    h.cases.append(case)
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
    h.cases.append(case)
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
    uds_dir = os.path.join(h.private_dir, name + ".sock")
    os.makedirs(uds_dir, exist_ok=True)
    case = h.launch_foreground(
        name,
        ["--require-tls", "--tls-cert", h.cert, "--tls-key", h.key, "--pg-socket-dir", uds_dir],
    )
    case.uds_dir = uds_dir
    h.cases.append(case)
    if not _started(h, case, name):
        return
    sock_path = os.path.join(uds_dir, ".s.PGSQL.%d" % case.pg_port)
    case.check(h.wait_path(sock_path), "%s: PostgreSQL UDS socket was created" % name)
    # TCP stays TLS-only even though the local socket is plaintext.
    h.expect_plaintext_refused(case, name + " tcp-plaintext", expect_log=True)
    h.select1_tls(case, name + " tcp-tls")
    # The authenticated local exception: plaintext UDS still demands SCRAM.
    h.select1_uds(case, name + " uds")
    h.expect_auth_rejected(case, h.wrong_password, name + " uds-wrong", host=uds_dir, sslmode=None, transport="uds")
    case.check(h.still_ours(case), "%s: SAME PID served TCP TLS and plaintext UDS" % name)
    h.stop_case(case)


def case_daemon_require_tls(h: TlsHarness) -> None:
    name = "04-daemon-require-tls"
    case = h.launch_daemon(name, ["--require-tls", "--tls-cert", h.cert, "--tls-key", h.key])
    h.cases.append(case)
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
            shutil.rmtree(data_dir, ignore_errors=True)
        h.cases.append(h.expect_refused_before_mutation(name, extra, data_dir, needles))


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
        "source_binding": "executable hash only; source manifest produced by root outside this harness",
        "gate": {"HELIOSDB_TLS_REGRESSION": os.environ.get("HELIOSDB_TLS_REGRESSION")},
        "unavailable": unavailable,
        "cleanup_failures": h.cleanup_failures,
        "cases": cases,
    }
    with open(os.path.join(h.evidence_dir, "require-tls-manifest.json"), "w", encoding="utf-8") as fh:
        json.dump(manifest, fh, indent=2)


def main() -> int:
    if os.environ.get("HELIOSDB_TLS_REGRESSION") != "1":
        eprint("skip: gated real-binary required-TLS harness; set HELIOSDB_TLS_REGRESSION=1 to run")
        return EXIT_OK

    h = TlsHarness()
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

    for case in h.cases:
        for ok, label in case.checks:
            eprint("%s %s" % ("ok  " if ok else "FAIL", label))
    eprint(
        "%s binary sha256 unchanged before/after suite (%s)"
        % ("ok  " if h.binary_hash_ok else "FAIL", h.bin_sha[:16])
    )
    if h.cleanup_failures:
        for item in h.cleanup_failures:
            eprint("FAIL cleanup: %s" % item)

    failures = [c for c in h.cases if c.failed()]
    if failures or h.cleanup_failures or not h.binary_hash_ok:
        eprint("RESULT: FAIL (%d case(s) with failures)" % len(failures))
        return EXIT_FAIL
    eprint(
        "RESULT: PASS - required-TLS refused plaintext, the same PID served TLS, "
        "Prefer and the authenticated UDS exception held, and the daemon PID file matched its worker"
    )
    return EXIT_OK


if __name__ == "__main__":
    sys.exit(main())
