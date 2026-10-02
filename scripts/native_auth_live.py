#!/usr/bin/env python3
"""Native logical replication token CLI-level live control (archive issue 1; R6).

Run only under the shared heavy lock. Uses the reviewed wire fixture read-only and
preserves all child data/logs.

One owner-only 32-byte token file authenticates both replication channels. The
token is never read from TOML or the environment.

Controls:
  * valid   : primary+standby share one token file -> standby connects and a
              committed row is visible on the standby.
  * missing : standby without a token file -> primary logs
              "native replication authentication required"; standby never reaches
              "Connected to primary".
  * wrong   : standby with a different token file -> primary logs
              "native replication authentication failed"; standby never connects.
  * sloppy  : primary with a group/other-readable (0644) token file refuses to
              start rather than downgrading.

Return 0 only when all controls hold; 1 = observed assertion failure;
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

TOKEN_BYTES = 32


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


def write_token(path, mode=0o600):
    path.write_bytes(os.urandom(TOKEN_BYTES))
    os.chmod(path, mode)
    return path


def run(root, binary, helper):
    result = {"status": "inconclusive", "checks": []}
    fixture = helper.Fixture(root / "control", binary)
    try:
        ppg, prepl, spg, srepl, spg2, srepl2, spg3, srepl3, epg, eprepl = fixture.ports(10)

        token_valid = write_token(root / "token_valid")
        token_wrong = write_token(root / "token_wrong")
        token_world = write_token(root / "token_world", mode=0o644)

        # --- a group/other-readable token file must be refused, never downgraded ---
        primary_empty, plog_empty = fixture.start(
            "primary_sloppy_token", epg, eprepl, "primary", token_file=token_world)
        deadline = time.monotonic() + 20
        while time.monotonic() < deadline and primary_empty.poll() is None:
            time.sleep(0.2)
        check(primary_empty.poll() is not None, "primary with a 0644 token file did not refuse to start")
        empty_log = log_text(plog_empty)
        check("replication token" in empty_log, f"missing token-file diagnostic: {empty_log[-400:]}")
        check("Server ready!" not in empty_log, "a sloppy token file must not start a ready server")
        result["checks"].append("world-readable token file: primary refuses to start (no downgrade)")
        # Reap the intentionally-exited child so the fixture's alive() check does
        # not flag it for the remaining controls.
        primary_empty.wait(timeout=5)
        fixture.children.remove(primary_empty)

        # --- valid ---
        primary, plog = fixture.start("primary", ppg, prepl, "primary", token_file=token_valid)
        fixture.ready(primary, plog, ppg)
        fixture.wait_native_owner(primary, prepl)
        standby, slog = fixture.start(
            "standby_ok", spg, srepl, "standby", prepl, ppg, token_file=token_valid)
        fixture.ready(standby, slog, spg, standby=True)
        result["checks"].append("valid token file: standby connected to primary")

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
        result["checks"].append("valid token file: committed row replicated and visible on standby")

        # --- missing token file ---
        standby_missing, slog_missing = fixture.start("standby_missing", spg2, srepl2, "standby", prepl, ppg)
        check(
            wait_for(plog, "native replication authentication required"),
            "primary did not log a missing-token rejection",
        )
        check(
            "Connected to primary" not in log_text(slog_missing),
            "standby without a token file connected to the primary",
        )
        result["checks"].append("missing token file: rejected before registration/WAL; standby not connected")

        # --- wrong token file ---
        standby_wrong, slog_wrong = fixture.start(
            "standby_wrong", spg3, srepl3, "standby", prepl, ppg, token_file=token_wrong)
        check(
            wait_for(plog, "native replication authentication failed"),
            "primary did not log a wrong-token rejection",
        )
        check(
            "Connected to primary" not in log_text(slog_wrong),
            "standby with the wrong token file connected to the primary",
        )
        result["checks"].append("wrong token file: rejected before registration/WAL; standby not connected")

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
