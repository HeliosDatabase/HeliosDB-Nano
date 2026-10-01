#!/usr/bin/env python3
"""Native logical credential CLI-level live control (archive issue 1).

Run only under the shared heavy lock. Uses the reviewed wire fixture read-only and
preserves all child data/logs.

Controls:
  * valid   : primary+standby share HELIOSDB_REPLICATION_AUTH_TOKEN -> standby
              connects and a committed row is visible on the standby.
  * missing : standby without the token -> primary logs
              "native replication authentication required"; standby never reaches
              "Connected to primary".
  * wrong   : standby with a different token -> primary logs
              "native replication authentication failed"; standby never connects.

Return 0 only when all three controls hold; 1 = observed assertion failure;
2 = setup/interruption/identity uncertainty.
"""
import argparse
import importlib.util
import json
import os
import tempfile
import time
import traceback
from pathlib import Path

TOKEN_ENV = "HELIOSDB_REPLICATION_AUTH_TOKEN"


def load_helper(path):
    spec = importlib.util.spec_from_file_location("owned_wire_fixture", path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def check(condition, message):
    if not condition:
        raise AssertionError(message)


def log_text(path):
    try:
        return path.read_text(errors="replace")
    except FileNotFoundError:
        return ""


def wait_for(path, needle, timeout=15.0):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if needle in log_text(path):
            return True
        time.sleep(0.2)
    return False


def run(root, binary, helper):
    result = {"status": "inconclusive", "checks": []}
    fixture = helper.Fixture(root / "control", binary)
    try:
        ppg, prepl, spg, srepl, spg2, srepl2, spg3, srepl3, epg, eprepl = fixture.ports(10)

        # --- empty env credential must be refused, never downgraded ---
        os.environ[TOKEN_ENV] = ""
        primary_empty, plog_empty = fixture.start("primary_empty_token", epg, eprepl, "primary")
        deadline = time.monotonic() + 20
        while time.monotonic() < deadline and primary_empty.poll() is None:
            time.sleep(0.2)
        check(primary_empty.poll() is not None, "primary with empty env token did not refuse to start")
        empty_log = log_text(plog_empty)
        check(
            "auth_token" in empty_log and "empty" in empty_log,
            f"missing empty-credential diagnostic: {empty_log[-400:]}",
        )
        check("Server ready!" not in empty_log, "empty credential must not start a ready server")
        result["checks"].append("empty env credential: primary refuses to start (no downgrade)")
        # Reap the intentionally-exited child so the fixture's alive() check does
        # not flag it for the remaining controls.
        primary_empty.wait(timeout=5)
        fixture.children.remove(primary_empty)

        # --- valid ---
        os.environ[TOKEN_ENV] = "native-secret-valid"
        primary, plog = fixture.start("primary", ppg, prepl, "primary")
        fixture.ready(primary, plog, ppg)
        fixture.wait_native_owner(primary, prepl)
        standby, slog = fixture.start("standby_ok", spg, srepl, "standby", prepl, ppg)
        fixture.ready(standby, slog, spg, standby=True)
        result["checks"].append("valid token: standby connected to primary")

        source = helper.Wire(fixture, primary, ppg, "primary")
        replica = helper.Wire(fixture, standby, spg, "standby")
        for sql in [
            "CREATE TABLE standby_write_probe (id INT PRIMARY KEY, note TEXT)",
            "BEGIN",
            "INSERT INTO standby_write_probe VALUES (1, 'ok')",
            "COMMIT",
        ]:
            response = source.simple(sql)
            check(not response["errors"], f"setup failed: {response}")
        helper.exact_rows(source, [["1", "ok"]])
        helper.exact_rows(replica, [["1", "ok"]])
        result["checks"].append("valid token: committed row replicated and visible on standby")

        # --- missing credential ---
        os.environ.pop(TOKEN_ENV, None)
        standby_missing, slog_missing = fixture.start("standby_missing", spg2, srepl2, "standby", prepl, ppg)
        check(
            wait_for(plog, "native replication authentication required"),
            "primary did not log a missing-credential rejection",
        )
        check(
            "Connected to primary" not in log_text(slog_missing),
            "standby without token connected to the primary",
        )
        result["checks"].append("missing token: rejected before registration/WAL; standby not connected")

        # --- wrong credential ---
        os.environ[TOKEN_ENV] = "native-secret-wrong"
        standby_wrong, slog_wrong = fixture.start("standby_wrong", spg3, srepl3, "standby", prepl, ppg)
        check(
            wait_for(plog, "native replication authentication failed"),
            "primary did not log a wrong-credential rejection",
        )
        check(
            "Connected to primary" not in log_text(slog_wrong),
            "standby with the wrong token connected to the primary",
        )
        result["checks"].append("wrong token: rejected before registration/WAL; standby not connected")

        fixture.alive()
        result["status"] = "pass"
    except AssertionError as error:
        result.update(status="regression_failed", error=str(error), traceback=traceback.format_exc())
    except BaseException as error:  # noqa: BLE001
        result.update(status="inconclusive", error=repr(error), traceback=traceback.format_exc())
    finally:
        try:
            fixture.close()
        except BaseException as error:  # noqa: BLE001
            result.update(status="inconclusive", cleanup_error=repr(error))
        (fixture.root / "native-auth-result.json").write_text(json.dumps(result, indent=2) + "\n")
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--output-dir", type=Path, required=True)
    parser.add_argument("--helper", type=Path,
                        default=Path(__file__).resolve().parent / "probe_standby_wire_writes.py")
    args = parser.parse_args()
    helper = load_helper(args.helper.resolve(strict=True))
    binary = args.binary.resolve(strict=True)
    script = Path(__file__).resolve(strict=True)
    args.output_dir.mkdir(parents=True, exist_ok=True)
    root = Path(tempfile.mkdtemp(prefix="native-auth-", dir=args.output_dir.resolve()))
    identities = {str(path): helper.digest(path) for path in (binary, script, args.helper.resolve())}
    result = {"identities_before": identities, "fixture_root": str(root)}
    try:
        result["control"] = run(root, binary, helper)
    finally:
        try:
            result["identities_after"] = {str(path): helper.digest(path) for path in (binary, script, args.helper.resolve())}
            result["identity_unchanged"] = result["identities_after"] == identities
        except Exception as error:  # noqa: BLE001
            result.update(identity_unchanged=False, identity_error=repr(error))
        (root / "result.json").write_text(json.dumps(result, indent=2) + "\n")
        print(root, flush=True)
    control = result["control"]
    if not result.get("identity_unchanged") or control["status"] == "inconclusive":
        return 2
    return 1 if control["status"] == "regression_failed" else 0


if __name__ == "__main__":
    try:
        code = main()
    except (Exception, KeyboardInterrupt) as error:  # noqa: BLE001
        print(f"inconclusive harness setup: {error!r}", flush=True)
        code = 2
    raise SystemExit(code)
