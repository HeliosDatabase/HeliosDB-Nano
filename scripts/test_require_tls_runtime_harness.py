#!/usr/bin/env python3
"""Focused, fully mocked tests for the required-TLS runtime harness.

These tests exercise the *actual* helper logic in
``scripts/test_require_tls_runtime.py`` -- the deadline-bounded SQL worker, the
identity-validated signal/reap path, orphan recovery, the exact UDS-inode proof
and the child environment -- without launching the real binary, opening a
socket, or touching production source.

Run with::

    python3 -m unittest scripts/test_require_tls_runtime_harness.py

or::

    pytest -q scripts/test_require_tls_runtime_harness.py
"""

import importlib.util
import io
import os
import signal
import sys
import tempfile
import unittest
from unittest import mock

# The harness sets this on import, but set it first so loading it here cannot
# leave a scripts/__pycache__ behind either.
sys.dont_write_bytecode = True

HARNESS_PATH = os.path.join(os.path.dirname(os.path.abspath(__file__)), "test_require_tls_runtime.py")


def _load_harness():
    spec = importlib.util.spec_from_file_location("require_tls_harness", HARNESS_PATH)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


harness = _load_harness()


class FakeConn:
    """Minimal stand-in for a multiprocessing Connection endpoint."""

    def __init__(self, poll_result=True, recv_value=None, recv_raises=None):
        self._poll_result = poll_result
        self._recv_value = recv_value
        self._recv_raises = recv_raises
        self.closed = False
        self.poll_timeout = None
        self.sent = None

    def poll(self, timeout=None):
        self.poll_timeout = timeout
        return self._poll_result

    def recv(self):
        if self._recv_raises is not None:
            raise self._recv_raises
        return self._recv_value

    def close(self):
        self.closed = True

    def send(self, value):
        self.sent = value


class FakeProc:
    """Controllable Process double for deadline/reap tests."""

    def __init__(
        self,
        alive_values=None,
        exitcode=0,
        start_raises=None,
        terminate_raises=None,
        kill_raises=None,
        join_raises=None,
    ):
        self.pid = 4242
        self.exitcode = exitcode
        self.started = False
        self.terminate_calls = 0
        self.kill_calls = 0
        self.join_calls = 0
        self._alive = list(alive_values) if alive_values else [False]
        self._start_raises = start_raises
        self._terminate_raises = terminate_raises
        self._kill_raises = kill_raises
        self._join_raises = join_raises

    def start(self):
        if self._start_raises is not None:
            raise self._start_raises
        self.started = True

    def is_alive(self):
        if len(self._alive) > 1:
            return self._alive.pop(0)
        return self._alive[0]

    def terminate(self):
        self.terminate_calls += 1
        if self._terminate_raises is not None:
            raise self._terminate_raises

    def kill(self):
        self.kill_calls += 1
        if self._kill_raises is not None:
            raise self._kill_raises

    def join(self, timeout=None):
        self.join_calls += 1
        if self._join_raises is not None:
            raise self._join_raises


class HarnessTestBase(unittest.TestCase):
    def setUp(self):
        self.h = harness.TlsHarness()
        self.h.bin_real = "/opt/helios/heliosdb-nano"
        self.tmp = tempfile.mkdtemp(prefix="hdb-tls-test-")
        self.h.output_dir = self.tmp
        self.h.evidence_dir = self.tmp

    def tearDown(self):
        import shutil

        shutil.rmtree(self.tmp, ignore_errors=True)


class DeadlineTests(HarnessTestBase):
    def test_timeout_kills_worker_and_records_failure(self):
        parent = FakeConn(poll_result=False)
        self.h._make_pipe = lambda: (parent, FakeConn())
        proc = FakeProc(alive_values=[False], exitcode=0)
        self.h._spawn_worker = lambda target, args: proc

        status, payload = self.h._run_bounded_sql({"host": "127.0.0.1"}, "L")

        self.assertEqual(status, "timeout")
        self.assertEqual(payload["type"], "DeadlineExceeded")
        self.assertTrue(any("deadline exceeded" in f.lower() for f in self.h.cleanup_failures))
        self.assertTrue(proc.started)

    def test_success_returns_rows_and_ssl_flag(self):
        payload = ("ok", {"rows": [(1,)], "ssl_in_use": True})
        self.h._make_pipe = lambda: (FakeConn(poll_result=True, recv_value=payload), FakeConn())
        proc = FakeProc(alive_values=[False], exitcode=0)
        self.h._spawn_worker = lambda target, args: proc

        status, got = self.h._run_bounded_sql({"host": "127.0.0.1"}, "L")

        self.assertEqual(status, "ok")
        self.assertEqual(got["rows"], [(1,)])
        self.assertTrue(got["ssl_in_use"])
        self.assertEqual(self.h.cleanup_failures, [])

    def test_eof_without_result_is_error_not_hang(self):
        self.h._make_pipe = lambda: (FakeConn(poll_result=True, recv_raises=EOFError()), FakeConn())
        self.h._spawn_worker = lambda target, args: FakeProc(alive_values=[False])

        status, payload = self.h._run_bounded_sql({"host": "127.0.0.1"}, "L")

        self.assertEqual(status, "error")
        self.assertEqual(payload["type"], "EOFError")

    def test_failed_spawn_does_not_touch_unstarted_worker(self):
        self.h._make_pipe = lambda: (FakeConn(poll_result=True), FakeConn())
        proc = FakeProc(start_raises=OSError("no fork"))
        self.h._spawn_worker = lambda target, args: proc

        status, payload = self.h._run_bounded_sql({"host": "127.0.0.1"}, "L")

        self.assertEqual(status, "error")
        self.assertIn("failed to start", payload["msg"])
        self.assertEqual(proc.join_calls, 0)

    def test_terminate_worker_records_every_failure(self):
        proc = FakeProc(
            alive_values=[True],
            exitcode=None,
            terminate_raises=OSError("term"),
            kill_raises=OSError("kill"),
            join_raises=OSError("join"),
        )
        self.h._terminate_worker(proc, "L")
        joined = "\n".join(self.h.cleanup_failures)
        self.assertIn("terminate failed", joined)
        self.assertIn("kill failed", joined)
        self.assertIn("still alive after TERM/KILL", joined)


class SignalTests(HarnessTestBase):
    def test_unvalidated_live_pid_is_never_signalled(self):
        with mock.patch.object(harness, "is_alive", return_value=True), mock.patch.object(
            self.h, "_identity_ok", return_value=False
        ), mock.patch.object(harness.os, "kill") as kill:
            self.h._signal_and_wait(123, "nid", "42", self.h.bin_real, "L")
        kill.assert_not_called()
        self.assertTrue(any("no longer matches identity" in f for f in self.h.cleanup_failures))

    def test_term_reaps_cleanly(self):
        with mock.patch.object(harness, "is_alive", return_value=False), mock.patch.object(
            self.h, "_identity_ok", return_value=True
        ), mock.patch.object(self.h, "_wait_gone"), mock.patch.object(harness.os, "kill") as kill:
            self.h._signal_and_wait(123, "nid", "42", self.h.bin_real, "L")
        kill.assert_called_once_with(123, signal.SIGTERM)
        self.assertEqual(self.h.cleanup_failures, [])

    def test_kill_failure_is_recorded(self):
        with mock.patch.object(harness, "is_alive", return_value=True), mock.patch.object(
            self.h, "_identity_ok", return_value=True
        ), mock.patch.object(self.h, "_wait_gone"), mock.patch.object(
            harness.os, "kill", side_effect=[None, OSError("nope")]
        ) as kill:
            self.h._signal_and_wait(123, "nid", "42", self.h.bin_real, "L")
        self.assertEqual(kill.call_count, 2)
        self.assertTrue(any("SIGKILL" in f for f in self.h.cleanup_failures))


class OrphanRecoveryTests(HarnessTestBase):
    def test_finds_worker_by_exact_node_id_and_exe(self):
        node = "abc-node"
        with mock.patch.object(harness, "proc_pids", return_value=[111, 222]), mock.patch.object(
            harness, "is_alive", return_value=True
        ), mock.patch.object(
            harness, "proc_exe", side_effect=lambda pid: self.h.bin_real if pid == 222 else "/other/bin"
        ), mock.patch.object(
            harness, "proc_cmdline", side_effect=lambda pid: ["--node-id", node] if pid == 222 else ["x"]
        ), mock.patch.object(harness, "proc_starttime", return_value="77"):
            self.assertEqual(self.h._find_orphan_worker(node), (222, "77", self.h.bin_real))
            self.assertIsNone(self.h._find_orphan_worker(node, exclude={222}))

    def test_adopt_registers_only_after_identity_proof(self):
        node = "abc-node"
        case = harness.Case("c")
        case.node_id = node
        with mock.patch.object(harness, "proc_cmdline_raw", return_value=b"helios\0--node-id\0abc-node\0"):
            adopted = self.h._adopt_worker(case, 1234, "42", self.h.bin_real, "orphan")
        self.assertTrue(adopted)
        self.assertTrue(case.identity_proven)
        self.assertIs(self.h.registry[1234], case)

    def test_adopt_rejects_unreadable_cmdline(self):
        case = harness.Case("c")
        case.node_id = "n"
        with mock.patch.object(harness, "proc_cmdline_raw", side_effect=OSError("gone")):
            adopted = self.h._adopt_worker(case, 1234, "42", self.h.bin_real, "orphan")
        self.assertFalse(adopted)
        self.assertFalse(case.identity_proven)
        self.assertTrue(any(not ok for ok, _ in case.checks))

    def test_foreground_popen_registered_before_identity_capture(self):
        case = harness.Case("f")
        self.h._new_case = lambda name: case
        log = os.path.join(self.tmp, "f.log")
        case.logfile = log

        class PopenStub:
            pid = 5555

        with mock.patch.object(harness.subprocess, "Popen", return_value=PopenStub()), mock.patch.object(
            self.h, "capture_identity", side_effect=RuntimeError("proc race")
        ):
            returned = self.h.launch_foreground("f", [])
        self.assertIsInstance(returned.proc, PopenStub)
        self.assertEqual(returned.pid, 5555)
        self.assertTrue(any("identity capture raised" in lbl for ok, lbl in returned.checks if not ok))


class DaemonPidfileTests(HarnessTestBase):
    def _launch_with_wrong_live_pidfile_and_orphan(self):
        name = "04-daemon-wrong-pid"
        wrong_pid = 31337
        orphan_pid = 31338
        launcher_pid = 31339
        self.h.private_dir = self.tmp
        pidfile = os.path.join(self.tmp, name + ".pid")

        class LauncherStub:
            pid = launcher_pid
            returncode = 0

            def communicate(self, timeout=None):
                return (b"", None)

        def fake_popen(argv, **_kwargs):
            with open(pidfile, "w", encoding="utf-8") as fh:
                fh.write("%d\n" % wrong_pid)
            return LauncherStub()

        holder = {}
        real_new_case = self.h._new_case

        def spy_new_case(case_name):
            case = real_new_case(case_name)
            holder["case"] = case
            return case

        def fake_cmdline_raw(pid):
            if pid == orphan_pid:
                return b"helios\0--node-id\0" + holder["case"].node_id.encode() + b"\0"
            raise AssertionError("unexpected proc_cmdline_raw(%d)" % pid)

        with mock.patch.object(harness.subprocess, "Popen", side_effect=fake_popen), mock.patch.object(
            self.h, "_new_case", side_effect=spy_new_case
        ), mock.patch.object(
            harness, "proc_starttime", return_value="42"
        ), mock.patch.object(
            harness, "proc_exe", return_value=self.h.bin_real
        ), mock.patch.object(
            self.h, "_identity_ok", return_value=False
        ), mock.patch.object(
            self.h, "_find_orphan_worker", return_value=(orphan_pid, "99", self.h.bin_real)
        ) as find, mock.patch.object(
            harness, "proc_cmdline_raw", side_effect=fake_cmdline_raw
        ):
            case = self.h.launch_daemon(name, ["--require-tls"])

        return case, wrong_pid, orphan_pid, find

    def test_wrong_positive_pidfile_stays_failed_despite_orphan_recovery(self):
        case, wrong_pid, orphan_pid, find = self._launch_with_wrong_live_pidfile_and_orphan()

        # The real worker is still adopted (and registered for cleanup)...
        find.assert_called_once()
        self.assertTrue(case.identity_proven)
        self.assertEqual(case.pid, orphan_pid)
        self.assertIs(self.h.registry.get(orphan_pid), case)
        self.assertTrue(any(ok and "recovered orphan worker" in label for ok, label in case.checks))
        # ...but the wrong positive PID file is a hard failure that is not erased.
        self.assertTrue(
            any(
                (not ok) and ("parseable PID file named process %d" % wrong_pid) in label
                for ok, label in case.checks
            ),
            case.checks,
        )
        self.assertTrue(case.failed())


class UdsProofTests(HarnessTestBase):
    @staticmethod
    def _net_unix_text(rows):
        lines = ["Num       RefCount Protocol Flags    Type St Inode Path"]
        for path, inode in rows:
            lines.append("0000000000000000: 00000002 00000000 00010000 0001 01 %d %s" % (inode, path))
        return "\n".join(lines) + "\n"

    def test_unix_socket_inode_reads_kernel_inode_not_stat_inode(self):
        stat_inode = 6923596109
        kernel_inode = 4027028528

        class StatStub:
            st_mode = 0o140000  # S_IFSOCK
            st_ino = stat_inode

        text = self._net_unix_text([("/tmp/x/.s.PGSQL.5432", kernel_inode)])
        with mock.patch.object(harness.os, "stat", return_value=StatStub()), mock.patch.object(
            harness, "open", side_effect=lambda path, *a, **k: io.StringIO(text), create=True
        ):
            self.assertEqual(harness.unix_socket_inode(123, "/tmp/x/.s.PGSQL.5432"), kernel_inode)
            self.assertNotEqual(kernel_inode, stat_inode)
            # A socket path absent from /proc/<pid>/net/unix resolves to None.
            self.assertIsNone(harness.unix_socket_inode(123, "/tmp/missing/.s.PGSQL.5432"))

    def test_child_owns_exact_socket_kernel_inode(self):
        stat_inode = 98765  # deliberately different from the kernel inode
        kernel_inode = 4027028528

        class StatStub:
            st_mode = 0o140000  # S_IFSOCK
            st_ino = stat_inode

        text = self._net_unix_text([("/tmp/x/.s.PGSQL.5432", kernel_inode)])
        with mock.patch.object(harness.os, "stat", return_value=StatStub()), mock.patch.object(
            harness, "open", side_effect=lambda path, *a, **k: io.StringIO(text), create=True
        ), mock.patch.object(
            harness.os, "listdir", return_value=["3", "4"]
        ), mock.patch.object(
            harness.os,
            "readlink",
            side_effect=lambda p: "socket:[%d]" % kernel_inode if p.endswith("/4") else "socket:[1]",
        ):
            self.assertTrue(self.h.child_owns_unix_socket(123, "/tmp/x/.s.PGSQL.5432"))
            # stat says "socket" but the path is not in /proc/<pid>/net/unix.
            self.assertFalse(self.h.child_owns_unix_socket(123, "/tmp/other/.s.PGSQL.5432"))

    def test_source_has_no_uds_dir_shortcut(self):
        with open(HARNESS_PATH, "r", encoding="utf-8") as fh:
            src = fh.read()
        self.assertNotIn("or bool(case.uds_dir)", src)
        self.assertIn("child_owns_unix_socket", src)

    def test_child_sql_reports_ssl_in_use_without_real_connection(self):
        class FakeCursor:
            def execute(self, _q):
                return None

            def fetchall(self):
                return [(1,)]

            def close(self):
                return None

        class FakeInfo:
            ssl_in_use = True

        class FakeConnection:
            info = FakeInfo()

            def cursor(self):
                return FakeCursor()

            def close(self):
                return None

        class FakePg:
            @staticmethod
            def connect(**_kwargs):
                return FakeConnection()

        send = FakeConn()
        with mock.patch.object(harness, "psycopg2", FakePg):
            harness._child_sql({"host": "127.0.0.1"}, send)
        self.assertEqual(send.sent, ("ok", {"rows": [(1,)], "ssl_in_use": True}))

    def test_child_sql_reports_error_payload(self):
        class FakePg:
            @staticmethod
            def connect(**_kwargs):
                raise RuntimeError("boom")

        send = FakeConn()
        with mock.patch.object(harness, "psycopg2", FakePg):
            harness._child_sql({"host": "127.0.0.1"}, send)
        status, payload = send.sent
        self.assertEqual(status, "error")
        self.assertEqual(payload["type"], "RuntimeError")

    def test_query_time_sqlstate_28_is_not_authentication_rejection(self):
        case = harness.Case("query-auth-error")
        case.pid = 123
        case.node_id = "node"
        with mock.patch.object(
            self.h,
            "_run_bounded_sql",
            return_value=(
                "error",
                {
                    "type": "QueryError",
                    "msg": "query rejected",
                    "pgcode": "28000",
                    "phase": "query",
                },
            ),
        ), mock.patch.object(self.h, "still_ours", return_value=True), mock.patch.object(
            self.h, "child_owns_port", return_value=True
        ):
            self.h.expect_auth_rejected(case, "wrong", "query-phase")
        self.assertTrue(case.failed())
        self.assertTrue(any("wrong credentials rejected" in label for ok, label in case.checks if not ok))


class AuditAndEnvTests(HarnessTestBase):
    def test_real_source_passes_bounded_path_audit(self):
        with open(HARNESS_PATH, "r", encoding="utf-8") as fh:
            violations = harness.audit_no_blocking_paths(fh.read())
        self.assertEqual(violations, [])

    def test_audit_flags_bare_connect_outside_worker(self):
        bad = "import psycopg2\ndef helper():\n    return psycopg2.connect()\n"
        violations = harness.audit_no_blocking_paths(bad)
        self.assertTrue(any("psycopg2.connect" in v for v in violations))

    def test_clean_env_strips_secrets_and_sets_controlled_rust_log(self):
        with mock.patch.dict(
            os.environ,
            {"HELIOSDB_PASSWORD": "sekret", "HELIOSDB_MCP_TOKEN": "tok", "RUST_LOG": "off"},
            clear=False,
        ):
            env = self.h._clean_env()
        self.assertNotIn("HELIOSDB_PASSWORD", env)
        self.assertNotIn("HELIOSDB_MCP_TOKEN", env)
        self.assertEqual(env["RUST_LOG"], harness.CHILD_RUST_LOG)


class RefusalTests(HarnessTestBase):
    def test_daemon_refusal_uses_marker_and_checks_no_orphan(self):
        name = "99-refuse"
        data_dir = os.path.join(self.tmp, "data")
        pid_file = os.path.join(self.tmp, "d.pid")
        node_id = "nid-1"
        seen = {}

        class Result:
            returncode = 2
            stdout = b"refused by validation"

        def fake_run(argv, **_kwargs):
            seen["argv"] = argv
            return Result()

        with mock.patch.object(harness.subprocess, "run", side_effect=fake_run), mock.patch.object(
            self.h, "_find_orphan_worker", return_value=None
        ) as find:
            case = self.h.expect_refused_before_mutation(
                name,
                ["--require-tls"],
                data_dir,
                ["refused"],
                daemon=True,
                pid_file=pid_file,
                node_id=node_id,
            )

        argv = seen["argv"]
        self.assertIn("--daemon", argv)
        self.assertIn("--pid-file", argv)
        self.assertIn("--node-id", argv)
        self.assertIn(node_id, argv)
        find.assert_called_once_with(node_id)
        self.assertTrue(any("no PID file" in lbl for _ok, lbl in case.checks))
        self.assertTrue(any("no --node-id orphan" in lbl for _ok, lbl in case.checks))

    def test_foreground_refusal_omits_daemon_flags(self):
        seen = {}

        class Result:
            returncode = 2
            stdout = b"refused"

        def fake_run(argv, **_kwargs):
            seen["argv"] = argv
            return Result()

        with mock.patch.object(harness.subprocess, "run", side_effect=fake_run):
            self.h.expect_refused_before_mutation(
                "98-refuse-fg", ["--require-tls"], os.path.join(self.tmp, "d2"), ["refused"]
            )
        self.assertNotIn("--daemon", seen["argv"])
        self.assertNotIn("--pid-file", seen["argv"])


if __name__ == "__main__":
    unittest.main()
