#!/usr/bin/env python3
"""Lightweight mocked-process tests for the secret-argv harness.

No signals and no process launches: every primitive that touches /proc,
subprocess, os.kill, or process discovery is monkeypatched. The assertions are
about externally meaningful outcomes — a negative config case fails when a
stray daemon or a leftover pidfile exists, and an un-cleanable or
identity-changed process is recorded rather than silently accepted.

Run: python3 scripts/test_secret_argv_mocks.py
"""

import importlib.util
import os
import sys
import tempfile
import unittest
from unittest import mock

# Loading the harness by path must not leave a scripts/__pycache__ behind.
sys.dont_write_bytecode = True

HERE = os.path.dirname(os.path.abspath(__file__))
HARNESS_PATH = os.path.join(HERE, "test_server_secret_argv.py")


def _load_harness():
    spec = importlib.util.spec_from_file_location("secret_argv_harness", HARNESS_PATH)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


m = _load_harness()

BIN = "/opt/nano/heliosdb-nano"


def _harness(evidence):
    h = m.Harness()
    h.bin = BIN
    h.bin_real = BIN
    h.evidence_dir = evidence
    h.work_dir = os.path.join(evidence, "work")
    os.makedirs(h.work_dir, exist_ok=True)
    return h


class ExpectConfigErrorTests(unittest.TestCase):
    """A rejection must be clean: no live child and no leftover pidfile."""

    def test_nonzero_with_pidfile_and_unparsed_pid_fails(self):
        with tempfile.TemporaryDirectory() as d:
            h = _harness(d)
            case = m.Case("cfg")
            case.launch_rc = 1
            case.launch_out = b"error: Cannot read --password-file /x: No such file"
            case.pidfile_seen = True
            case.pid = 0  # never read because rc != 0
            h.expect_config_error(case, ["Cannot read --password-file"], case.name)
            self.assertTrue(case.failed(), "leftover pidfile must fail a rejection case")

    def test_nonzero_with_stray_child_fails_and_is_tracked(self):
        with tempfile.TemporaryDirectory() as d:
            h = _harness(d)
            case = m.Case("cfg")
            case.launch_rc = 1
            case.launch_out = b"boom"
            case.stray = True
            case.orphans = [m.Orphan(4242, "uuid", "17", BIN)]
            h.expect_config_error(case, ["boom"], case.name)
            self.assertTrue(case.failed(), "a surviving child must fail a rejection case")
            self.assertEqual([o.pid for o in case.orphans], [4242])

    def test_clean_nonzero_matching_message_passes(self):
        with tempfile.TemporaryDirectory() as d:
            h = _harness(d)
            case = m.Case("cfg")
            case.launch_rc = 1
            case.launch_out = b"error: --password-file /x is empty."
            h.expect_config_error(case, ["is empty"], case.name)
            self.assertFalse(case.failed(), "a clean rejection should pass")

    def test_timeout_fails(self):
        with tempfile.TemporaryDirectory() as d:
            h = _harness(d)
            case = m.Case("cfg")
            case.timed_out = True
            case.launch_out = b"is empty"
            h.expect_config_error(case, ["is empty"], case.name)
            self.assertTrue(case.failed())

    def test_bind_failure_fails_as_unrelated(self):
        with tempfile.TemporaryDirectory() as d:
            h = _harness(d)
            case = m.Case("cfg")
            case.launch_rc = 1
            case.launch_out = b"failed to bind 127.0.0.1: Address already in use"
            h.expect_config_error(case, ["Address already in use"], case.name)
            self.assertTrue(case.failed(), "a bind failure is not a config rejection")


class SignalCleanupTests(unittest.TestCase):
    """_signal_and_wait must never silently accept an un-cleanable process."""

    def test_permission_error_is_recorded(self):
        with tempfile.TemporaryDirectory() as d:
            h = _harness(d)
            with mock.patch.object(m, "is_alive", return_value=True), mock.patch.object(
                m.Harness, "_identity_ok", return_value=True
            ), mock.patch.object(m.os, "kill", side_effect=PermissionError("EPERM")):
                h._signal_and_wait(9001, "uuid", "3", BIN, "case x")
            self.assertTrue(h.cleanup_failures, "EPERM must be recorded as a cleanup failure")
            self.assertIn("SIGTERM", h.cleanup_failures[0])

    def test_vanished_process_is_not_flagged(self):
        with tempfile.TemporaryDirectory() as d:
            h = _harness(d)
            with mock.patch.object(m, "is_alive", return_value=True), mock.patch.object(
                m.Harness, "_identity_ok", return_value=True
            ), mock.patch.object(m.os, "kill", side_effect=ProcessLookupError()):
                h._signal_and_wait(9001, "uuid", "3", BIN, "case x")
            self.assertEqual(h.cleanup_failures, [], "a normally vanished process is not a failure")

    def test_live_identity_mismatch_is_recorded_and_not_signalled(self):
        with tempfile.TemporaryDirectory() as d:
            h = _harness(d)
            with mock.patch.object(m, "is_alive", return_value=True), mock.patch.object(
                m.Harness, "_identity_ok", return_value=False
            ), mock.patch.object(m.os, "kill", side_effect=AssertionError("must not signal a mismatched pid")):
                h._signal_and_wait(9001, "uuid", "3", BIN, "case x")
            self.assertTrue(h.cleanup_failures, "a live but mismatched pid must be reported")

    def test_dead_identity_mismatch_is_not_flagged(self):
        with tempfile.TemporaryDirectory() as d:
            h = _harness(d)
            with mock.patch.object(m, "is_alive", return_value=False), mock.patch.object(
                m.Harness, "_identity_ok", return_value=False
            ), mock.patch.object(m.os, "kill", side_effect=AssertionError("must not signal")):
                h._signal_and_wait(9001, "uuid", "3", BIN, "case x")
            self.assertEqual(h.cleanup_failures, [])

    def test_identity_change_after_term_is_recorded(self):
        with tempfile.TemporaryDirectory() as d:
            h = _harness(d)
            with mock.patch.object(m, "is_alive", return_value=True), mock.patch.object(
                m.Harness, "_wait_gone", return_value=None
            ), mock.patch.object(m.Harness, "_identity_ok", side_effect=[True, False]), mock.patch.object(
                m.os, "kill", return_value=None
            ):
                h._signal_and_wait(9001, "uuid", "3", BIN, "case x")
            self.assertTrue(any("changed identity" in f for f in h.cleanup_failures))

    def test_still_alive_after_kill_waits_is_recorded(self):
        with tempfile.TemporaryDirectory() as d:
            h = _harness(d)
            with mock.patch.object(m, "is_alive", return_value=True), mock.patch.object(
                m.Harness, "_wait_gone", return_value=None
            ), mock.patch.object(m.Harness, "_identity_ok", return_value=True), mock.patch.object(
                m.os, "kill", return_value=None
            ):
                h._signal_and_wait(9001, "uuid", "3", BIN, "case x")
            self.assertTrue(any("still alive" in f for f in h.cleanup_failures))


class _FakeAsyncConn:
    """Minimal psycopg2-async stand-in: a scripted poll() sequence + fileno()."""

    def __init__(self, states, fileno=41):
        self._states = list(states)
        self._fileno = fileno

    def poll(self):
        return self._states.pop(0)

    def fileno(self):
        return self._fileno


class PollAsyncConnectionTests(unittest.TestCase):
    """Exercise the REAL shared polling helper (no socket, no server).

    It is used for both connection establishment and each SELECT 1 execution, so
    its bounded progression and its timeout-vs-auth distinction are the contract
    the forthcoming argv re-exec harness inherits.
    """

    def test_readable_then_writable_then_ok_progresses(self):
        calls = []

        def select_fn(rlist, wlist, xlist, timeout):
            calls.append((list(rlist), list(wlist), list(xlist), timeout))
            return ([], [], [])

        conn = _FakeAsyncConn(
            [
                m.psycopg2.extensions.POLL_READ,
                m.psycopg2.extensions.POLL_WRITE,
                m.psycopg2.extensions.POLL_OK,
            ],
            fileno=41,
        )
        m.poll_async_connection(conn, m.time.monotonic() + 5.0, select_fn=select_fn)

        self.assertEqual(len(calls), 2, "one wait per non-OK poll, then POLL_OK returns")
        self.assertEqual(calls[0][:3], ([41], [], []), "POLL_READ waits on the fd for read")
        self.assertEqual(calls[1][:3], ([], [41], []), "POLL_WRITE waits on the fd for write")
        self.assertTrue(all(c[3] > 0 for c in calls), "each wait gets a positive remaining budget")

    def test_immediate_ok_does_not_wait(self):
        def select_fn(*args):
            raise AssertionError("POLL_OK must return without selecting")

        conn = _FakeAsyncConn([m.psycopg2.extensions.POLL_OK])
        m.poll_async_connection(conn, m.time.monotonic() + 5.0, select_fn=select_fn)

    def test_expired_deadline_is_a_timeout_not_a_wait(self):
        def select_fn(*args):
            raise AssertionError("must not select past an expired deadline")

        conn = _FakeAsyncConn([m.psycopg2.extensions.POLL_READ])
        with self.assertRaises(m.SqlDeadlineExceeded):
            m.poll_async_connection(conn, m.time.monotonic() - 1.0, select_fn=select_fn)

    def test_unexpected_poll_state_raises(self):
        conn = _FakeAsyncConn([999])
        with self.assertRaises(m.psycopg2.OperationalError):
            m.poll_async_connection(conn, m.time.monotonic() + 5.0, select_fn=lambda *a: ([], [], []))

    def test_poll_error_propagates_untouched(self):
        class AuthFailConn:
            def poll(self):
                raise m.psycopg2.OperationalError("password authentication failed for user \"postgres\"")

            def fileno(self):
                return 3

        with self.assertRaises(m.psycopg2.Error):
            m.poll_async_connection(AuthFailConn(), m.time.monotonic() + 5.0, select_fn=lambda *a: ([], [], []))

    def test_deadline_exceeded_is_not_classified_as_auth(self):
        self.assertFalse(
            issubclass(m.SqlDeadlineExceeded, m.psycopg2.Error),
            "a client timeout must never be catchable as an authentication error",
        )

    def test_select_timeout_then_late_ok_is_rejected(self):
        """A read wait that times out, then a late POLL_OK, must not be accepted.

        The clock advances across the ``poll()`` after the timed-out select, so
        this exercises the second deadline check (before accepting POLL_OK), not
        just the pre-poll one.
        """
        base = m.time.monotonic()
        deadline = base + 1.0
        waited = []

        def select_fn(rlist, wlist, xlist, timeout):
            waited.append((list(rlist), list(wlist), timeout))
            return ([], [], [])  # poll() never became readable: select timed out

        conn = _FakeAsyncConn(
            [m.psycopg2.extensions.POLL_READ, m.psycopg2.extensions.POLL_OK], fileno=41
        )
        # pre-poll, remaining-budget, pre-poll again, then post-POLL_OK (late).
        ticks = [base, base, base, base + 2.0]
        with mock.patch.object(m.time, "monotonic", side_effect=ticks):
            with self.assertRaises(m.SqlDeadlineExceeded):
                m.poll_async_connection(conn, deadline, select_fn=select_fn)
        self.assertEqual(len(waited), 1, "the read wait happened exactly once")
        self.assertEqual(waited[0][:2], ([41], []), "the wait was a read wait on the fd")


class SqlCallerClassificationTests(unittest.TestCase):
    """Caller-level guarantees of the bounded SQL helpers.

    ``_sql_connect`` returns only after authentication succeeds, so
    ``sql_expect_rejected`` must fail whenever the connection was accepted, even
    if the later ``SELECT 1`` raises an auth-shaped error. A connect that hits
    its deadline must still close the half-open connection.
    """

    def _mock_identity(self):
        return (
            mock.patch.object(m.Harness, "_identity_ok", return_value=True),
            mock.patch.object(m.Harness, "child_owns_port", return_value=True),
        )

    def test_accepted_connection_with_query_auth_error_is_not_a_rejection(self):
        with tempfile.TemporaryDirectory() as d:
            h = _harness(d)
            case = m.Case("wrong-pw")
            case.pid = 1234
            case.node_id = "uuid"
            case.starttime = "7"
            case.exe = BIN
            accepted = mock.Mock()
            query_auth_error = m.psycopg2.OperationalError(
                'password authentication failed for user "postgres"'
            )
            ident, owns = self._mock_identity()
            with ident, owns, mock.patch.object(
                h, "_sql_connect", return_value=accepted
            ), mock.patch.object(h, "_sql_select1_rows", side_effect=query_auth_error):
                h.sql_expect_rejected(case, "wrong", "wrong-pw")
            self.assertTrue(
                case.failed(),
                "a connection that authenticated must never count as a wrong-password rejection",
            )
            accepted.close.assert_called_once()

    def test_connect_deadline_closes_connection(self):
        with tempfile.TemporaryDirectory() as d:
            h = _harness(d)
            case = m.Case("connect-deadline")
            case.pg_port = 5432
            half_open = mock.Mock()
            with mock.patch.object(m.psycopg2, "connect", return_value=half_open), mock.patch.object(
                m, "poll_async_connection", side_effect=m.SqlDeadlineExceeded("client deadline expired")
            ):
                with self.assertRaises(m.SqlDeadlineExceeded):
                    h._sql_connect(case, "pw")
            half_open.close.assert_called_once()


class LaunchStrayTests(unittest.TestCase):
    """A failing launcher that leaves a daemon must be tracked for cleanup."""

    def test_nonzero_launcher_with_pidfile_recovers_and_tracks_stray(self):
        with tempfile.TemporaryDirectory() as d:
            h = _harness(d)
            name = "cfg"
            pidfile = os.path.join(h.work_dir, name + ".pid")
            stray = m.Orphan(5555, "uuid", "23", BIN)

            def fake_run(argv, **kwargs):
                with open(pidfile, "w", encoding="utf-8") as fh:
                    fh.write("not-a-pid")
                result = mock.Mock()
                result.returncode = 1
                result.stdout = b"Cannot read --password-file /x: No such file"
                return result

            with mock.patch.object(m, "ephemeral_port", side_effect=[1111, 1112]), mock.patch.object(
                m.subprocess, "run", side_effect=fake_run
            ), mock.patch.object(m.Harness, "scan_orphans", return_value=[stray]):
                case = h.launch(name, "scram-sha-256", ["--password-file", "/x"], {})

            self.assertTrue(case.pidfile_seen, "pidfile existence must be observed even on failure")
            self.assertEqual(case.pid, 0, "an unparsable pid must not be adopted")
            self.assertTrue(case.stray)
            self.assertEqual([o.pid for o in case.orphans], [5555])

            h.cases.append(case)
            h.expect_config_error(case, ["Cannot read --password-file"], case.name)
            self.assertTrue(case.failed(), "stray daemon + pidfile must fail the rejection case")

            # The stray is registered for identity-safe cleanup.
            with mock.patch.object(m.Harness, "stop_orphan") as stop:
                h.cleanup()
            stop.assert_called_once()


if __name__ == "__main__":
    unittest.main(verbosity=2)
