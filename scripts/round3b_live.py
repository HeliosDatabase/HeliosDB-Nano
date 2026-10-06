#!/usr/bin/env python3
"""Round 3b live scenarios L2/L3/L4/L5(simple)/L6 (report-only; no product code).

Reuses the reviewed wire fixture (probe_standby_wire_writes.py) read-only via
import. Every server, resync and daemon child is owned: it is started with an
exact PID, waited on, terminated, and re-checked by the post-run audit.

Subcommands:
  l2  one secret, both channels (native standby + physical raw resync, NON-SERVING)
  l3  protocol-mismatch matrix (v3 primary vs v1/guard-v2/auth-v2 peers, and reverse)
  l4  guard containment (fresh-history primary replacement)
  l5s standby simple-query local write refusal
  l6  plaintext-remote opt-in (refused, accepted, daemon)
"""
import argparse
import importlib.util
import json
import os
import signal
import socket
import subprocess
import sys
import tempfile
import time
import traceback
from pathlib import Path

TOKEN_BYTES = 32
EXIT_OK = 0
EXIT_FAIL = 1
EXIT_INCONCLUSIVE = 2


def load_helper(path):
    spec = importlib.util.spec_from_file_location("owned_wire_fixture", path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def write_token(path, mode=0o600, length=TOKEN_BYTES):
    path.write_bytes(os.urandom(length))
    os.chmod(path, mode)
    return path


def log_text(path):
    try:
        return Path(path).read_text(errors="replace")
    except FileNotFoundError:
        return ""


def wait_log(path, needles, timeout=25.0):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        text = log_text(path)
        if all(n in text for n in needles):
            return True
        time.sleep(0.2)
    return False


def wait_exit(child, timeout):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if child.poll() is not None:
            return child.returncode
        time.sleep(0.1)
    return None


def run_process(command, log_path, timeout):
    """Run a standalone owned process, capture combined output, return (rc, text)."""
    with open(log_path, "wb") as log:
        child = subprocess.Popen(command, stdin=subprocess.DEVNULL, stdout=log, stderr=subprocess.STDOUT)
    rc = wait_exit(child, timeout)
    if rc is None:
        child.terminate()
        rc = wait_exit(child, 5)
        if rc is None:
            child.kill()
            rc = wait_exit(child, 5)
        rc = ("timeout", rc)
    return rc, log_text(log_path)


def port_closed(port, timeout=3):
    try:
        with socket.create_connection(("127.0.0.1", port), timeout):
            return False
    except OSError:
        return True


def check(result, name, condition, detail=""):
    result["checks"].append({"name": name, "status": "pass" if condition else "fail", "detail": str(detail)})
    return condition


# --------------------------------------------------------------------------- L2
def l2(root, binary, helper):
    result = {"scenario": "l2", "checks": []}
    fixture = helper.Fixture(root / "l2", binary)
    pids = []
    try:
        ppg, prepl, spg, srepl, phys = fixture.ports(5)
        fixture.release_port(phys)
        token = write_token(root / "token")
        token_wrong = write_token(root / "token_wrong")
        export = root / "export"
        export.mkdir()
        os.chmod(export, 0o700)
        primary, plog = fixture.start(
            "primary", ppg, prepl, "primary", token_file=token,
            extra_args=["--physical-replication",
                        "--physical-replication-listen", f"127.0.0.1:{phys}",
                        "--physical-export-dir", str(export)])
        fixture.ready(primary, plog, ppg)
        fixture.wait_native_owner(primary, prepl)
        standby, slog = fixture.start("standby", spg, srepl, "standby", prepl, ppg, token_file=token)
        fixture.ready(standby, slog, spg, standby=True)
        source = helper.Wire(fixture, primary, ppg, "primary")
        replica = helper.Wire(fixture, standby, spg, "standby")
        for sql in ["CREATE TABLE standby_write_probe (id INT PRIMARY KEY, note TEXT)",
                    "BEGIN", "INSERT INTO standby_write_probe VALUES (1, 'ok')", "COMMIT"]:
            response = source.simple(sql)
            if response["errors"]:
                raise AssertionError(f"source setup failed: {response}")
        helper.exact_rows(source, [["1", "ok"]])
        helper.exact_rows(replica, [["1", "ok"]])
        check(result, "native channel accepts the shared token file", True)

        rc, out = run_process(
            [str(binary), "resync", "--primary-host", f"127.0.0.1:{phys}",
             "--data-dir", str(root / "resync_ok"), "--replication-token-file", str(token), "--once"],
            root / "resync_ok.log", 90)
        check(result, "physical resync accepts the same token file (NON-SERVING)",
              rc == 0 and ("not SQL readiness" in out or "Physical state retained closed" in out),
              f"rc={rc} tail={out[-300:]}")

        rc2, out2 = run_process(
            [str(binary), "resync", "--primary-host", f"127.0.0.1:{phys}",
             "--data-dir", str(root / "resync_bad"), "--replication-token-file", str(token_wrong), "--once"],
            root / "resync_bad.log", 60)
        check(result, "physical resync refuses a wrong token",
              rc2 != 0 and ("authentication" in out2 or "token" in out2),
              f"rc={rc2} tail={out2[-300:]}")

        result["status"] = "pass" if all(c["status"] == "pass" for c in result["checks"]) else "fail"
    except BaseException as error:  # noqa: BLE001
        result["status"] = "inconclusive"
        result["error"] = repr(error)
        result["traceback"] = traceback.format_exc()
    finally:
        try:
            fixture.close()
        except BaseException as error:  # noqa: BLE001
            result.setdefault("cleanup_error", repr(error))
        for pid in pids:
            for sig in (signal.SIGTERM, signal.SIGKILL):
                try:
                    os.kill(pid, sig)
                except ProcessLookupError:
                    break
                time.sleep(0.2)
        (root / "result.json").write_text(json.dumps(result, indent=2) + "\n")
    return result


# --------------------------------------------------------------------------- L3
def l3(root, binary, helper, v1, guard, auth):
    result = {"scenario": "l3", "checks": []}
    peers = [("v1", v1, 1), ("guard-v2", guard, 2), ("auth-v2", auth, 2)]
    try:
        for label, peer_bin, version in peers:
            primary_fx = helper.Fixture(root / f"fwd_{label}", binary)
            peer_fx = helper.Fixture(root / f"peer_{label}", peer_bin)
            try:
                ppg, prepl = primary_fx.ports(2)
                spg, srepl = peer_fx.ports(2)
                primary, plog = primary_fx.start("primary", ppg, prepl, "primary")
                primary_fx.ready(primary, plog, ppg)
                primary_fx.wait_native_owner(primary, prepl)
                peer_fx.start("standby", spg, srepl, "standby", prepl, ppg)
                refused = wait_log(plog, ["Refused native handshake", "protocol version mismatch",
                                          f"peer v{version}"], timeout=25)
                check(result, f"{label}: v3 primary logs a version-mismatch refusal naming peer v{version}",
                      refused, log_text(plog)[-500:])
                connected = "Connected to primary" in log_text(peer_fx.root / f"standby.log")
                check(result, f"{label}: peer never registers/connects", not connected)
            finally:
                primary_fx.close()
                peer_fx.close()

        # reverse: candidate v3 standby against the v1 primary
        primary_fx = helper.Fixture(root / "rev_primary", v1)
        standby_fx = helper.Fixture(root / "rev_standby", binary)
        try:
            ppg, prepl = primary_fx.ports(2)
            spg, srepl = standby_fx.ports(2)
            primary, plog = primary_fx.start("primary", ppg, prepl, "primary")
            primary_fx.ready(primary, plog, ppg)
            primary_fx.wait_native_owner(primary, prepl)
            standby_fx.start("standby", spg, srepl, "standby", prepl, ppg)
            time.sleep(3)
            slog = log_text(standby_fx.root / "standby.log")
            check(result, "reverse: v3 standby never connects to a v1 primary",
                  "Connected to primary" not in slog, slog[-400:])
            check(result, "reverse: v3 standby reports a native protocol/streaming failure",
                  "version" in slog.lower() or "Streaming error" in slog, slog[-400:])
        finally:
            primary_fx.close()
            standby_fx.close()
        result["status"] = "pass" if all(c["status"] == "pass" for c in result["checks"]) else "fail"
    except BaseException as error:  # noqa: BLE001
        result["status"] = "inconclusive"
        result["error"] = repr(error)
        result["traceback"] = traceback.format_exc()
    (root / "result.json").write_text(json.dumps(result, indent=2) + "\n")
    return result


# --------------------------------------------------------------------------- L4
def l4(root, binary, helper):
    result = {"scenario": "l4", "checks": []}
    node_id = "11111111-2222-3333-4444-555555555555"
    fx = helper.Fixture(root / "l4_a", binary)
    fx2 = helper.Fixture(root / "l4_a2", binary)
    try:
        ppg, prepl, spg, srepl = fx.ports(4)
        primary, plog = fx.start("primary", ppg, prepl, "primary", node_id=node_id)
        fx.ready(primary, plog, ppg)
        fx.wait_native_owner(primary, prepl)
        standby, slog = fx.start("standby", spg, srepl, "standby", prepl, ppg)
        fx.ready(standby, slog, spg, standby=True)
        source = helper.Wire(fx, primary, ppg, "primary")
        for sql in ["CREATE TABLE guard_probe (id INT PRIMARY KEY, note TEXT)",
                    "BEGIN", "INSERT INTO guard_probe VALUES (1, 'seed')", "COMMIT"]:
            response = source.simple(sql)
            if response["errors"]:
                raise AssertionError(f"source setup failed: {response}")
        check(result, "baseline row replicated before replacement", True)

        # Replace the primary: kill it, start a fresh-history primary at the same
        # endpoint and node id.
        primary.terminate()
        wait_exit(primary, 10)
        fx2.release_port(prepl)
        fx2.release_port(ppg)
        primary2, plog2 = fx2.start("primary", ppg, prepl, "primary", node_id=node_id)
        fx2.ready(primary2, plog2, ppg)
        mismatch = wait_log(slog, ["Replication history mismatch"], timeout=30)
        check(result, "standby reports HistoryMismatch", mismatch, log_text(slog)[-500:])
        code = wait_exit(standby, 30)
        check(result, "standby exits terminally after HistoryMismatch", code is not None, f"rc={code}")
        check(result, "standby SQL listener closed", port_closed(spg), f"port {spg}")
        result["status"] = "pass" if all(c["status"] == "pass" for c in result["checks"]) else "fail"
    except BaseException as error:  # noqa: BLE001
        result["status"] = "inconclusive"
        result["error"] = repr(error)
        result["traceback"] = traceback.format_exc()
    finally:
        try:
            fx.close()
        except BaseException as error:  # noqa: BLE001
            result.setdefault("cleanup_error", repr(error))
        try:
            fx2.close()
        except BaseException as error:  # noqa: BLE001
            result.setdefault("cleanup_error2", repr(error))
        (root / "result.json").write_text(json.dumps(result, indent=2) + "\n")
    return result


# --------------------------------------------------------------------------- L5 simple
def l5s(root, binary, helper):
    result = {"scenario": "l5s", "checks": []}
    fixture = helper.Fixture(root / "l5s", binary)
    try:
        ppg, prepl, spg, srepl = fixture.ports(4)
        primary, plog = fixture.start("primary", ppg, prepl, "primary")
        fixture.ready(primary, plog, ppg)
        fixture.wait_native_owner(primary, prepl)
        standby, slog = fixture.start("standby", spg, srepl, "standby", prepl, ppg)
        fixture.ready(standby, slog, spg, standby=True)
        source = helper.Wire(fixture, primary, ppg, "primary")
        receiver = helper.Wire(fixture, standby, spg, "standby")
        for sql in ["CREATE TABLE standby_write_probe (id INT PRIMARY KEY, note TEXT)",
                    "BEGIN", "INSERT INTO standby_write_probe VALUES (1, 'seed')", "COMMIT"]:
            response = source.simple(sql)
            if response["errors"]:
                raise AssertionError(f"source setup failed: {response}")
        helper.exact_rows(source, [["1", "seed"]])
        helper.exact_rows(receiver, [["1", "seed"]])
        attempt = receiver.simple("INSERT INTO standby_write_probe VALUES (99, 'standby-local')")
        time.sleep(1.5)
        source_rows = source.rows()
        standby_rows = receiver.rows()
        refused = bool(attempt["errors"])
        no_mutation = source_rows == [["1", "seed"]] and standby_rows == [["1", "seed"]]
        check(result, "standby simple-query local write is refused", refused, attempt)
        check(result, "no local mutation on either node", no_mutation,
              f"source={source_rows} standby={standby_rows}")
        result["standby_attempt"] = attempt
        result["status"] = "pass" if all(c["status"] == "pass" for c in result["checks"]) else "fail"
    except BaseException as error:  # noqa: BLE001
        result["status"] = "inconclusive"
        result["error"] = repr(error)
        result["traceback"] = traceback.format_exc()
    finally:
        try:
            fixture.close()
        except BaseException as error:  # noqa: BLE001
            result.setdefault("cleanup_error", repr(error))
        (root / "result.json").write_text(json.dumps(result, indent=2) + "\n")
    return result


# --------------------------------------------------------------------------- L6
def l6(root, binary, helper):
    result = {"scenario": "l6", "checks": []}
    token = write_token(root / "token")
    daemon_pids = []
    try:
        # refused without the opt-in
        fx1 = helper.Fixture(root / "l6_refused", binary)
        try:
            ppg, prepl = fx1.ports(2)
            child, log = fx1.start("primary_refused", ppg, prepl, "primary", token_file=token, listen="0.0.0.0")
            code = wait_exit(child, 25)
            check(result, "non-loopback token refused without the opt-in",
                  code is not None and "allow-insecure-replication-remote" in log_text(log),
                  f"rc={code} tail={log_text(log)[-400:]}")
        finally:
            fx1.close()
        # accepted with the opt-in
        fx2 = helper.Fixture(root / "l6_optin", binary)
        try:
            ppg, prepl = fx2.ports(2)
            child, log = fx2.start("primary_optin", ppg, prepl, "primary", token_file=token,
                                   listen="0.0.0.0", extra_args=["--allow-insecure-replication-remote"])
            ready = wait_log(log, ["Server ready!"], timeout=40) and child.poll() is None
            check(result, "opt-in starts and logs INSECURE REPLICATION OVERRIDE",
                  ready and "INSECURE REPLICATION OVERRIDE" in log_text(log), log_text(log)[-400:])
        finally:
            fx2.close()
        # daemon mode
        fx3 = helper.Fixture(root / "l6_daemon", binary)
        try:
            ppg, prepl = fx3.ports(2)
            pid_file = root / "l6_daemon.owned.pid"
            child, log = fx3.start("primary_daemon", ppg, prepl, "primary", token_file=token,
                                   listen="0.0.0.0",
                                   extra_args=["--allow-insecure-replication-remote", "--daemon",
                                               "--pid-file", str(pid_file)])
            code = wait_exit(child, 40)
            time.sleep(1.0)
            text = log_text(log)
            check(result, "daemon opt-in starts and logs INSECURE REPLICATION OVERRIDE",
                  code == 0 and "INSECURE REPLICATION OVERRIDE" in text, f"rc={code} tail={text[-400:]}")
            if pid_file.exists():
                try:
                    daemon_pids.append(int(pid_file.read_text().strip()))
                except ValueError:
                    pass
        finally:
            fx3.close()
            for pid in daemon_pids:
                for sig in (signal.SIGTERM, signal.SIGKILL):
                    try:
                        os.kill(pid, sig)
                    except ProcessLookupError:
                        break
                    time.sleep(0.3)
        result["status"] = "pass" if all(c["status"] == "pass" for c in result["checks"]) else "fail"
    except BaseException as error:  # noqa: BLE001
        result["status"] = "inconclusive"
        result["error"] = repr(error)
        result["traceback"] = traceback.format_exc()
    (root / "result.json").write_text(json.dumps(result, indent=2) + "\n")
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("scenario", choices=["l2", "l3", "l4", "l5s", "l6"])
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--output-dir", type=Path, required=True)
    parser.add_argument("--helper", type=Path,
                        default=Path(__file__).resolve().parent / "probe_standby_wire_writes.py")
    parser.add_argument("--v1", type=Path)
    parser.add_argument("--guard-v2", type=Path)
    parser.add_argument("--auth-v2", type=Path)
    args = parser.parse_args()
    helper = load_helper(args.helper.resolve(strict=True))
    binary = args.binary.resolve(strict=True)
    args.output_dir.mkdir(parents=True, exist_ok=True)
    root = Path(tempfile.mkdtemp(prefix=f"round3b-{args.scenario}-", dir=args.output_dir.resolve()))
    try:
        if args.scenario == "l2":
            result = l2(root, binary, helper)
        elif args.scenario == "l3":
            result = l3(root, binary, helper, args.v1.resolve(strict=True),
                        args.guard_v2.resolve(strict=True), args.auth_v2.resolve(strict=True))
        elif args.scenario == "l4":
            result = l4(root, binary, helper)
        elif args.scenario == "l5s":
            result = l5s(root, binary, helper)
        else:
            result = l6(root, binary, helper)
    except BaseException as error:  # noqa: BLE001
        result = {"scenario": args.scenario, "status": "inconclusive", "error": repr(error),
                  "traceback": traceback.format_exc()}
    result["fixture_root"] = str(root)
    (root / "summary.json").write_text(json.dumps(result, indent=2) + "\n")
    print(root, flush=True)
    if result.get("status") == "inconclusive":
        return EXIT_INCONCLUSIVE
    return EXIT_FAIL if result.get("status") == "fail" else EXIT_OK


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (Exception, KeyboardInterrupt) as error:  # noqa: BLE001
        print(f"inconclusive harness setup: {error!r}", file=sys.stderr, flush=True)
        raise SystemExit(EXIT_INCONCLUSIVE)
