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
