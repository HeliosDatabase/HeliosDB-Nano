#!/usr/bin/env python3
"""Focused real-process evidence for the Linux foreground argv re-exec.

Companion to ``scripts/test_server_secret_argv.py``. It reuses that harness's
``/proc`` identity binding, listener-ownership proof, SQL authentication
primitives, and identity-safe cleanup, and adds what the re-exec itself needs:

* a **foreground** server (not daemon) is launched and inspected directly, so
  the process that must drop the literal secret is the one we observe;
* the final ``/proc/<pid>/cmdline`` is proven secret-free for both the
  ``--password`` and ``--mcp-token`` options, in split and ``=value`` form;
* ``/proc/<pid>/exe`` and starttime are bound to the launched PID, and the
  same PID authenticates SQL (``SELECT 1``) and rejects a wrong password;
* the daemon path is shown to hand the worker a pidfile-recorded PID whose
  argv stays secret-free and whose authenticated SQL still works;
* an invalid credential is shown to exit without leaving a listener behind on
  either the PostgreSQL or the HTTP port (asserted after exit only, not as a
  claim that no listener was ever briefly bound).

GATED: with ``HELIOSDB_PROC_REGRESSION`` unset this exits 0 after a visible
skip and launches nothing. Exit codes mirror the sibling harness: 0 pass/skip,
1 assertion failure, 3 requested but unavailable (fail closed).
"""

import importlib.util
import json
import os
import subprocess
import sys
import time
import uuid

# Importing the sibling harness by path must not leave a scripts/__pycache__.
sys.dont_write_bytecode = True

HERE = os.path.dirname(os.path.abspath(__file__))
_HARNESS_PATH = os.path.join(HERE, "test_server_secret_argv.py")


def _load_sibling():
    spec = importlib.util.spec_from_file_location("secret_argv_harness", _HARNESS_PATH)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


m = _load_sibling()


class ReexecHarness(m.Harness):
    """Foreground/daemon re-exec evidence on top of the shared harness."""

    def __init__(self):
        super().__init__()
        # PID -> Popen for processes we spawned directly. This ownership is the
        # handle we hold, independent of the identity-verified `registry`, which
        # may also hold pidfile-derived daemon workers. Cleanup treats the two
        # distinctly: an owned handle is reaped through Popen, a pidfile PID is
        # only ever signalled after a full identity match.
        self.owned = {}

    # ── launch helpers ──────────────────────────────────────────────────
    def _base_argv(self, node_id, pg_port, http_port, auth):
        return [
            self.bin,
            "start",
            "--memory",
            "--listen",
            "127.0.0.1",
            "--port",
            str(pg_port),
            "--http-port",
            str(http_port),
            "--node-id",
            node_id,
            "--auth",
            auth,
        ]

    def _clean_env(self, secret_env):
        env = {k: v for k, v in os.environ.items() if k not in m.SECRET_ENV_KEYS}
        env.update(secret_env)
        return env

    def _new_case(self, name):
        case = m.Case(name)
        case.pg_port = m.ephemeral_port()
        case.http_port = m.ephemeral_port()
        while case.http_port == case.pg_port:
            case.http_port = m.ephemeral_port()
        case.node_id = str(uuid.uuid4())
        case.pidfile = ""
        case.logfile = os.path.join(self.evidence_dir, name + ".launch.log")
        return case

    def _wait_reexec(self, case, secrets):
        """Poll until the running image is secret-free and identity-stable.

        The process is the binary from the start, so the observable change is
        the argv: the literal option and its value disappear only after the
        re-exec. If the process exits first (a fail-closed preflight), return
        False immediately rather than burning the whole timeout.
        """
        deadline = time.time() + m.READY_TIMEOUT
        while time.time() < deadline:
            proc = getattr(case, "proc", None)
            if proc is not None and proc.poll() is not None:
                return False
            if not self.capture_identity(case):
                time.sleep(m.POLL_INTERVAL)
                continue
            if case.exe != self.bin_real:
                time.sleep(m.POLL_INTERVAL)
                continue
            if any(secret and secret.encode("utf-8") in case.raw_cmdline for secret in secrets):
                time.sleep(m.POLL_INTERVAL)
                continue
            if not self._argv_pair(case.argv, "--node-id", case.node_id):
                time.sleep(m.POLL_INTERVAL)
                continue
            return True
        return False

    def launch_foreground(self, name, extra_args, secret_env, secrets, auth="scram-sha-256"):
        case = self._new_case(name)
        argv = self._base_argv(case.node_id, case.pg_port, case.http_port, auth) + list(extra_args)
        try:
            logfh = open(case.logfile, "wb")
        except OSError as exc:
            case.check(False, "%s: could not open launch log: %s" % (name, exc))
            return case
        try:
            with logfh:
                case.proc = subprocess.Popen(
                    argv,
                    env=self._clean_env(secret_env),
                    stdin=subprocess.DEVNULL,
                    stdout=logfh,
                    stderr=subprocess.STDOUT,
                )
        except OSError as exc:
            case.check(False, "%s: could not spawn the server: %s" % (name, exc))
            return case
        # Register for final cleanup the instant the child exists, before any
        # identity work: if capture raises below, the child is still reaped.
        case.pid = case.proc.pid
        self.owned[case.pid] = case.proc
        try:
            case.identity_proven = self._wait_reexec(case, secrets)
        except Exception as exc:  # noqa: BLE001 - a capture bug must not leak the child
            case.check(False, "%s: identity capture raised %s: %s" % (name, type(exc).__name__, exc))
            case.identity_proven = False
        if case.identity_proven:
            self.registry[case.pid] = case
        return case

    def launch_daemon(self, name, extra_args, secret_env, auth="scram-sha-256"):
        # Reuse the reviewed sibling lifecycle (daemon argv, identity proof,
        # orphan recovery, nonzero/timeout handling) rather than a weakened
        # copy. Then insist that a surviving worker does not excuse a launcher
        # that exited nonzero or timed out.
        case = super().launch(name, auth, list(extra_args), secret_env)
        if case.timed_out:
            case.check(False, "%s: daemon launcher timed out" % name)
        elif case.launch_rc != 0:
            case.check(False, "%s: daemon launcher exited %s" % (name, case.launch_rc))
        return case

    def expect_rejected_before_listener(self, name, extra_args, secret_env, needles):
        """A rejected startup must exit without leaving a listener behind.

        The process is run to completion, so this observes the *post-exit*
        state only (PG and HTTP ports). It does not and cannot prove that no
        listener was ever briefly bound during startup; the wording below says
        exactly what is checked.
        """
        case = self._new_case(name)
        argv = self._base_argv(case.node_id, case.pg_port, case.http_port, "scram-sha-256") + list(extra_args)
        try:
            proc = subprocess.run(
                argv,
                env=self._clean_env(secret_env),
                stdout=subprocess.PIPE,
                stderr=subprocess.STDOUT,
                timeout=m.LAUNCH_TIMEOUT,
            )
            case.launch_rc = proc.returncode
            case.launch_out = proc.stdout or b""
        except subprocess.TimeoutExpired as exc:
            case.timed_out = True
            case.launch_out = exc.stdout or b""
        self._write(os.path.basename(case.logfile), self.redact(case.launch_out.decode("utf-8", "replace")))
        out = case.launch_out.decode("utf-8", "replace")
        case.check(not case.timed_out, "%s: rejected startup did not hang" % name)
        case.check(case.launch_rc not in (None, 0), "%s: invalid credential exited non-zero" % name)
        case.check(
            not self.listening_inodes(case.pg_port),
            "%s: no listener remains on the requested PG port after exit" % name,
        )
        case.check(
            not self.listening_inodes(case.http_port),
            "%s: no listener remains on the requested HTTP port after exit" % name,
        )
        case.check(any(needle in out for needle in needles), "%s: rejection message matched" % name)
        return case

    # ── cleanup ─────────────────────────────────────────────────────────
    def _reap_owned(self, proc, label):
        """Bounded TERM/KILL/reap for a process we spawned directly.

        Ownership is the Popen handle, so no /proc identity recheck is needed.
        Every failure to reap is recorded, never swallowed.
        """
        if proc.poll() is not None:
            try:
                proc.wait(timeout=m.KILL_WAIT)
            except Exception as exc:  # noqa: BLE001 - a failed reap is evidence
                self.cleanup_failures.append("%s: reap after exit failed: %s" % (label, exc))
            return
        try:
            proc.terminate()
        except OSError as exc:
            self.cleanup_failures.append("%s: terminate() failed: %s" % (label, exc))
        else:
            try:
                proc.wait(timeout=m.TERM_WAIT)
                return
            except subprocess.TimeoutExpired:
                pass
            except Exception as exc:  # noqa: BLE001
                self.cleanup_failures.append("%s: wait after TERM failed: %s" % (label, exc))
        if proc.poll() is not None:
            return
        try:
            proc.kill()
        except OSError as exc:
            self.cleanup_failures.append("%s: kill() failed: %s" % (label, exc))
        try:
            proc.wait(timeout=m.KILL_WAIT)
        except subprocess.TimeoutExpired:
            self.cleanup_failures.append("%s: still alive after TERM/KILL waits" % label)
        except Exception as exc:  # noqa: BLE001
            self.cleanup_failures.append("%s: wait after KILL failed: %s" % (label, exc))

    def stop_case(self, case):
        self.registry.pop(case.pid, None)
        proc = getattr(case, "proc", None)
        if proc is not None:
            # Directly owned Popen child: reap through the handle we created.
            self.owned.pop(case.pid, None)
            self._reap_owned(proc, "case %s" % case.name)
            return
        # pidfile-derived (daemon worker): only signal a fully verified identity.
        self._signal_and_wait(case.pid, case.node_id, case.starttime, case.exe, "case %s" % case.name)

    def cleanup(self):
        # The reviewed base cleanup handles registry cases (through our
        # stop_case) and recovered orphans; then reap any directly owned child
        # that never joined a proven case (e.g. a capture exception).
        super().cleanup()
        for pid, proc in list(self.owned.items()):
            self.owned.pop(pid, None)
            self._reap_owned(proc, "owned pid %d" % pid)


def _proof(h, case, secret, wrong, label):
    """Listener ownership + exact SQL auth + wrong-password rejection."""
    if not h.wait_ready(case):
        h.assert_owns_listener(case, case.pg_port, label)
        case.check(False, "%s: never accepted connections" % label)
        return
    h.assert_owns_listener(case, case.pg_port, label)
    h.sql_expect_ok(case, secret, label)
    h.sql_expect_rejected(case, wrong, label)


def build_cases(h):
    secret = "hdb-reexec-pw-%d" % os.getpid()
    wrong = secret + "-wrong"
    token = "hdb-reexec-token-%d" % os.getpid()
    h.redact_values.extend([secret, wrong, token])

    def foreground(name, extra, secret_env, secrets, want_mcp=None):
        case = h.launch_foreground(name, extra, secret_env, secrets)
        h.cases.append(case)
        case.check(case.identity_proven, "%s: process identity proven (exe/PID/starttime)" % name)
        if not case.identity_proven:
            h.stop_case(case)
            return
        case.check(case.pid == case.proc.pid, "%s: PID preserved across the re-exec" % name)
        case.check(case.starttime != "", "%s: /proc starttime captured after re-exec" % name)
        for value in secrets:
            h.argv_hides(case, value, name)
        h.argv_lacks_flag(case, "--password", name)
        h.argv_lacks_flag(case, "--mcp-token", name)
        _proof(h, case, secret, wrong, name)
        if want_mcp is not None and os.environ.get("HELIOSDB_PROC_MCP_HTTP") == "1":
            h.mcp_http_auth(case, want_mcp, name)
        h.stop_case(case)

    # 1/2. split and equals literal password: off the final argv, SQL auth works.
    foreground("01-fg-password-split", ["--password", secret], {}, [secret])
    foreground("02-fg-password-equals", ["--password=" + secret], {}, [secret])

    # 3. stale inherited password file must not outrank the literal password.
    stale = os.path.join(h.work_dir, "stale-password.txt")
    with open(stale, "w", encoding="utf-8") as fh:
        fh.write("hdb-stale-value")
    foreground(
        "03-fg-password-beats-file",
        ["--password", secret],
        {"HELIOSDB_PASSWORD_FILE": stale},
        [secret],
    )

    # 4/5. split and equals literal MCP token: off argv; password comes from env.
    foreground("04-fg-mcp-split", ["--mcp-token", token], {"HELIOSDB_PASSWORD": secret}, [token], want_mcp=token)
    foreground("05-fg-mcp-equals", ["--mcp-token=" + token], {"HELIOSDB_PASSWORD": secret}, [token], want_mcp=token)

    # 6. daemon: the pidfile records the worker PID; its argv is secret-free and
    #    SQL auth still works. The reviewed sibling launch proved the identity
    #    and reclaimed any orphan on failure.
    case = h.launch_daemon("06-daemon-password", ["--password", secret], {})
    h.cases.append(case)
    if case.identity_proven:
        h.argv_hides(case, secret, case.name)
        h.argv_lacks_flag(case, "--password", case.name)
        _proof(h, case, secret, wrong, case.name)
        h.stop_case(case)
    else:
        case.check(False, "%s: daemon launched but no proven worker was captured" % case.name)

    # 7. invalid credential must exit without leaving a listener behind. (A real
    #    exec failure is covered by the Rust adapter unit test, not here; the
    #    listener assertion is post-exit only.)
    case = h.expect_rejected_before_listener(
        "07-empty-password-no-residual-listener",
        ["--password", ""],
        {},
        ["--password value is empty"],
    )
    h.cases.append(case)


def write_manifest(h, unavailable):
    cases = []
    for case in h.cases:
        cases.append(
            {
                "name": case.name,
                "pid": case.pid,
                "pg_port": case.pg_port,
                "http_port": case.http_port,
                "node_id": case.node_id,
                "pidfile": case.pidfile,
                "pidfile_seen": case.pidfile_seen,
                "identity_proven": case.identity_proven,
                "starttime": case.starttime,
                "exe": case.exe,
                "argv": [h.redact(a) for a in case.argv],
                "checks": [{"ok": ok, "label": h.redact(label)} for ok, label in case.checks],
                "failed": case.failed(),
            }
        )
    manifest = {
        "at": m.now(),
        "binary": h.bin,
        "binary_realpath": h.bin_real,
        "binary_sha256": h.bin_sha,
        "binary_sha256_after": h.bin_sha_after,
        "binary_hash_ok": h.binary_hash_ok,
        "version": h.version_out,
        "source_binding": "executable hash only; source manifest produced by root outside this harness",
        "gate": {
            "HELIOSDB_PROC_REGRESSION": os.environ.get("HELIOSDB_PROC_REGRESSION"),
            "HELIOSDB_PROC_MCP_HTTP": os.environ.get("HELIOSDB_PROC_MCP_HTTP"),
        },
        "unavailable": unavailable,
        "cleanup_failures": h.cleanup_failures,
        "cases": cases,
    }
    with open(os.path.join(h.evidence_dir, "reexec-manifest.json"), "w", encoding="utf-8") as fh:
        json.dump(manifest, fh, indent=2)


def main():
    if os.environ.get("HELIOSDB_PROC_REGRESSION") != "1":
        m.eprint("skip: gated real-process argv-reexec harness; set HELIOSDB_PROC_REGRESSION=1 to run")
        return m.EXIT_OK

    h = ReexecHarness()
    unavailable = None
    try:
        h.prepare()
        build_cases(h)
        h.finalize_binary_binding()
    except m.Unavailable as exc:
        unavailable = str(exc)
        m.eprint("unavailable: %s" % exc)
    except Exception as exc:  # noqa: BLE001 - convert harness bugs into a red result
        unavailable = "harness error: %s: %s" % (type(exc).__name__, exc)
        m.eprint("unavailable: %s" % unavailable)
    finally:
        h.cleanup()
        if h.evidence_dir:
            write_manifest(h, unavailable)

    if unavailable is not None:
        return m.EXIT_UNAVAILABLE

    failures = [c for c in h.cases if c.failed()]
    for case in h.cases:
        for ok, label in case.checks:
            m.eprint("%s %s" % ("ok  " if ok else "FAIL", label))
    m.eprint(
        "%s binary sha256 unchanged before/after suite (%s)"
        % ("ok  " if h.binary_hash_ok else "FAIL", h.bin_sha[:16])
    )
    if h.cleanup_failures:
        for item in h.cleanup_failures:
            m.eprint("FAIL cleanup: %s" % item)
    if failures or h.cleanup_failures or not h.binary_hash_ok:
        m.eprint("RESULT: FAIL (%d case(s) with failures)" % len(failures))
        return m.EXIT_FAIL
    m.eprint("RESULT: PASS — literal secrets left the foreground/daemon argv and auth still works")
    return m.EXIT_OK


if __name__ == "__main__":
    sys.exit(main())
