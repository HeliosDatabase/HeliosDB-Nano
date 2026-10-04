#!/usr/bin/env python3
"""Lightweight, process-free tests for the argv-reexec *harness* lifecycle.

These tests drive the real Python `ReexecHarness` subclass in
``scripts/test_argv_reexec.py`` with every process/primitives seam mocked. They
assert externally meaningful lifecycle outcomes:

* a failed launcher with a surviving worker is rejected and the worker is
  reclaimed as an identity-bound orphan;
* a launcher that reports success without a pidfile still recovers a marked
  orphan;
* an exception during identity capture, after a foreground spawn, leaves the
  child registered for cleanup and reaps it;
* an unverified pidfile PID is never signalled;
* a failed wait/reap is recorded, never swallowed.

They deliberately do **not** re-implement or model the Rust `plan_start_reexec`
planner: the production logic lives in Rust and its unit tests are run by
`cargo test --bin heliosdb-nano`, not here. No process is launched and no signal
is sent.

Run: python3 scripts/test_argv_reexec_mocks.py
"""

import importlib.util
import os
import subprocess
import sys
import tempfile
import unittest
from unittest import mock

# Loading the harness modules by path must not leave a __pycache__ behind.
sys.dont_write_bytecode = True

HERE = os.path.dirname(os.path.abspath(__file__))


def _load(name, path):
    spec = importlib.util.spec_from_file_location(name, path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


# Loading the reexec module also loads the reviewed sibling harness (as r.m).
r = _load("argv_reexec_harness", os.path.join(HERE, "test_argv_reexec.py"))
m = r.m

BIN = "/opt/nano/heliosdb-nano"


def _harness(evidence):
    h = r.ReexecHarness()
    h.bin = BIN
    h.bin_real = BIN
    h.evidence_dir = evidence
    h.work_dir = os.path.join(evidence, "work")
    os.makedirs(h.work_dir, exist_ok=True)
    return h


class FakeProc:
    """Minimal Popen stand-in: poll/terminate/kill/wait with injectable faults."""

    def __init__(self, pid, alive=True, wait_timeout=False, wait_error=None):
        self.pid = pid
        self._alive = alive
        self._wait_timeout = wait_timeout
        self._wait_error = wait_error
        self.terminated = False
        self.killed = False
        self.wait_calls = 0

    def poll(self):
        return None if self._alive else 0

    def terminate(self):
        self.terminated = True
        if not self._wait_timeout:
            self._alive = False

    def kill(self):
        self.killed = True
        self._alive = False

    def wait(self, timeout=None):
        self.wait_calls += 1
        if self._wait_timeout:
            raise subprocess.TimeoutExpired(cmd="fake", timeout=timeout or 0)
        if self._wait_error is not None:
            raise self._wait_error
        return 0


def _fake_run(pidfile_path, pid_text=None, rc=0, out=b""):
    """A subprocess.run stand-in: optionally writes a pidfile, then returns."""

    def run(argv, **kwargs):
        if pid_text is not None:
            with open(pidfile_path, "w", encoding="utf-8") as fh:
                fh.write(pid_text)
        result = mock.Mock()
        result.returncode = rc
        result.stdout = out
        return result

    return run


class DaemonLifecycleTests(unittest.TestCase):
    def test_failed_launcher_with_live_worker_is_rejected_and_reclaimed(self):
        with tempfile.TemporaryDirectory() as d:
            h = _harness(d)
            name = "daemon-fail"
            pidfile = os.path.join(h.work_dir, name + ".pid")
            orphan = m.Orphan(4321, "uuid", "17", BIN)
            with mock.patch.object(m, "ephemeral_port", side_effect=[1111, 1112]), mock.patch.object(
                subprocess, "run", side_effect=_fake_run(pidfile, "4321", rc=1, out=b"launcher failed")
            ), mock.patch.object(m.Harness, "scan_orphans", return_value=[orphan]):
                case = h.launch_daemon(name, ["--password", "s"], {})

            self.assertFalse(case.timed_out)
            self.assertEqual(case.launch_rc, 1)
            self.assertFalse(case.identity_proven, "a nonzero launcher must not be treated as started")
            self.assertTrue(case.failed(), "the nonzero launcher must be a failure")
            self.assertTrue(any("launcher exited 1" in label for ok, label in case.checks if not ok))
            self.assertTrue(case.stray)
            self.assertEqual([o.pid for o in case.orphans], [4321])

            # The live worker is reclaimed through identity-safe orphan cleanup.
            h.cases.append(case)
            with mock.patch.object(m.Harness, "stop_orphan") as stop:
                h.cleanup()
            stop.assert_called_once()

    def test_missing_pidfile_with_live_orphan_is_recovered(self):
        with tempfile.TemporaryDirectory() as d:
            h = _harness(d)
            name = "daemon-nopid"
            pidfile = os.path.join(h.work_dir, name + ".pid")
            orphan = m.Orphan(9001, "uuid", "19", BIN)
            with mock.patch.object(m, "ephemeral_port", side_effect=[1111, 1112]), mock.patch.object(
                subprocess, "run", side_effect=_fake_run(pidfile, None, rc=0, out=b"")
            ), mock.patch.object(m.Harness, "scan_orphans", return_value=[orphan]):
                case = h.launch_daemon(name, ["--password", "s"], {})

            self.assertFalse(case.pidfile_seen, "no pidfile was written")
            self.assertEqual(case.pid, 0, "no PID may be adopted without a pidfile")
            self.assertTrue(case.stray)
            self.assertEqual([o.pid for o in case.orphans], [9001])
            self.assertTrue(any("wrote no pidfile" in label for ok, label in case.checks if not ok))

            h.cases.append(case)
            with mock.patch.object(m.Harness, "stop_orphan") as stop:
                h.cleanup()
            stop.assert_called_once()

    def test_unverified_pidfile_pid_is_never_signalled(self):
        with tempfile.TemporaryDirectory() as d:
            h = _harness(d)
            name = "daemon-mismatch"
            pidfile = os.path.join(h.work_dir, name + ".pid")

            def fake_capture(case):
                # Right pidfile, wrong executable/marker/starttime.
                case.starttime = ""
                case.exe = "/some/other/binary"
                case.raw_cmdline = b"x\0"
                case.argv = ["x"]
                case.environ = {}
                return True

            with mock.patch.object(m, "ephemeral_port", side_effect=[1111, 1112]), mock.patch.object(
                subprocess, "run", side_effect=_fake_run(pidfile, "5123", rc=0, out=b"")
            ), mock.patch.object(m.Harness, "capture_identity", side_effect=fake_capture), mock.patch.object(
                m.Harness, "scan_orphans", return_value=[]
            ), mock.patch.object(m.os, "kill", side_effect=AssertionError("must not signal an unverified pid")):
                case = h.launch_daemon(name, ["--password", "s"], {})

            self.assertFalse(case.identity_proven)
            self.assertNotIn(5123, h.registry)
            self.assertTrue(
                any("identity validation failed" in label for ok, label in case.checks if not ok),
                "the mismatch must be recorded",
            )

            h.cases.append(case)
            h.cleanup()  # must not attempt to signal the unverified pid


class ForegroundLifecycleTests(unittest.TestCase):
    def test_exception_after_spawn_still_registers_and_reaps(self):
        with tempfile.TemporaryDirectory() as d:
            h = _harness(d)
            proc = FakeProc(7001, alive=True)
            with mock.patch.object(m, "ephemeral_port", side_effect=[1111, 1112]), mock.patch.object(
                subprocess, "Popen", return_value=proc
            ), mock.patch.object(r.ReexecHarness, "_wait_reexec", side_effect=RuntimeError("capture boom")):
                case = h.launch_foreground("fg-exc", ["--password", "s"], {}, ["s"])

            self.assertFalse(case.identity_proven)
            self.assertIn(7001, h.owned, "the child must be registered before identity capture")
            self.assertTrue(case.failed(), "the capture exception must be recorded")

            h.cases.append(case)
            h.cleanup()
            self.assertTrue(proc.terminated or proc.killed, "the owned child must be reaped")
            self.assertGreaterEqual(proc.wait_calls, 1)
            self.assertEqual(h.cleanup_failures, [])

    def test_wait_and_reap_failures_are_reported(self):
        with tempfile.TemporaryDirectory() as d:
            h = _harness(d)
            proc = FakeProc(8001, alive=True, wait_timeout=True)
            with mock.patch.object(m, "ephemeral_port", side_effect=[1111, 1112]), mock.patch.object(
                subprocess, "Popen", return_value=proc
            ), mock.patch.object(r.ReexecHarness, "_wait_reexec", return_value=True):
                case = h.launch_foreground("fg-reap", ["--password", "s"], {}, ["s"])

            h.stop_case(case)
            self.assertTrue(
                any("still alive after TERM/KILL waits" in f for f in h.cleanup_failures),
                "an un-reapable child must be reported",
            )

            # A completed child whose wait() fails is reported, not swallowed.
            proc2 = FakeProc(8002, alive=True, wait_error=RuntimeError("reap boom"))
            h.owned[8002] = proc2
            h._reap_owned(proc2, "owned pid 8002")
            self.assertTrue(any("wait after TERM failed" in f for f in h.cleanup_failures))

            proc3 = FakeProc(8003, alive=False, wait_error=RuntimeError("reap boom"))
            h._reap_owned(proc3, "owned pid 8003")
            self.assertTrue(any("reap after exit failed" in f for f in h.cleanup_failures))


if __name__ == "__main__":
    unittest.main(verbosity=2)
