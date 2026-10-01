#!/usr/bin/env python3
"""Owned raw-wire admission regression. Run only under the shared heavy lock.

Imports the reviewed fixture helper read-only; preserves all child data and wire
frames. Return 0 only after exact row checks + SQLSTATE/protocol recovery checks;
1 is an observed assertion failure, 2 is setup/interruption/identity uncertainty.
No SQL UDF/sequence read-only or complete storage security claim.
"""
import argparse
import importlib.util
import json
from pathlib import Path
import select
import signal
import struct
import tempfile
import time
import traceback


def load_helper(path):
    spec = importlib.util.spec_from_file_location("owned_wire_fixture", path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def check(condition, message):
    if not condition:
        raise AssertionError(message)


def check_refusal(response, expected_status="I"):
    check([error.get("C") for error in response["errors"]] == ["25006"],
          f"expected one 25006 error: {response}")
    check(not response["copy_entered"] and not response["tags"] and not response["rows"],
          f"write entered COPY/returned success/data: {response}")
    check(response.get("ready_status") == expected_status, f"wrong transaction status: {response}")


def parse_bind(wire, statement, portal, row_id, parse=True, returning=False):
    if parse:
        sql = b"INSERT INTO standby_write_probe (id, note) VALUES ($1, $2)"
        if returning:
            sql += b" RETURNING id"
        wire.send(b"P", statement + b"\0" + sql + b"\0" + struct.pack("!HII", 2, 23, 25))
    parameters = [str(row_id).encode(), b"must-not-write"]
    bind = portal + b"\0" + statement + b"\0" + struct.pack("!HH", 0, 2)
    for value in parameters:
        bind += struct.pack("!I", len(value)) + value
    wire.send(b"B", bind + struct.pack("!H", 0))
    wire.send(b"H")
    expected = [b"1", b"2"] if parse else [b"2"]
    observed = []
    deadline = time.monotonic() + 5
    while len(observed) < len(expected):
        kind, data = wire.recv(deadline)
        if kind in (b"N", b"S"):
            continue
        observed.append(kind)
        check(kind != b"E", f"Parse/Bind unexpectedly refused before Execute: {wire.error(data) if kind == b'E' else kind}")
    check(observed == expected, f"wrong Parse/Bind frames: {observed}")


def reject_execute(wire, portal, status="I"):
    wire.send(b"E", portal + b"\0" + struct.pack("!I", 0))
    wire.send(b"H")
    deadline = time.monotonic() + 5
    for _ in range(16):
        kind, data = wire.recv(deadline)
        if kind in (b"N", b"S"):
            continue
        check(kind == b"E", f"Execute did not fail before mutation: {kind!r}, {data!r}")
        check(wire.error(data).get("C") == "25006", f"wrong Execute SQLSTATE: {wire.error(data)}")
        break
    else:
        raise AssertionError("missing Execute ErrorResponse")
    # A pipelined Execute/Flush must be ignored after error, and ReadyForQuery
    # belongs to the following Sync. select peeks readiness without consuming an
    # incomplete frame or introducing an unauthenticated extra connection.
    wire.send(b"E", portal + b"\0" + struct.pack("!I", 0))
    wire.send(b"H")
    check(not select.select([wire.sock], [], [], 0.2)[0], "backend sent messages before recovery Sync")
    wire.send(b"S")
    recovery = wire.drain()
    check(not recovery["errors"] and not recovery["tags"] and not recovery["rows"]
          and not recovery["copy_entered"] and recovery.get("ready_status") == status,
          f"bad Sync recovery: {recovery}")


def extended_read(wire, parse):
    statement, portal = b"allowed_read", b"allowed_read_portal"
    if parse:
        wire.send(b"P", statement + b"\0SELECT id FROM standby_write_probe WHERE id = $1\0" + struct.pack("!HI", 1, 23))
    wire.send(b"B", portal + b"\0" + statement + b"\0" + struct.pack("!HHI", 0, 1, 1) + b"1" + struct.pack("!H", 0))
    wire.send(b"E", portal + b"\0" + struct.pack("!I", 0))
    wire.send(b"S")
    response = wire.drain()
    check(response["rows"] == [["1"]] and not response["errors"] and response.get("ready_status") == "I", f"extended read/cache reuse failed: {response}")


def recovered_read(wire):
    response = wire.simple("SELECT 731 AS admission_recovery")
    check(response["rows"] == [["731"]] and not response["errors"]
          and response.get("ready_status") == "I", f"connection not recovered: {response}")


def run_mode(helper, root, binary, mode):
    fixture = helper.Fixture(root / mode, binary)
    result = {"mode": mode, "status": "inconclusive", "checks": []}
    try:
        ppg, prepl, spg, srepl = fixture.ports(4)
        primary, plog = fixture.start("primary", ppg, prepl, "primary")
        fixture.ready(primary, plog, ppg)
        fixture.wait_native_owner(primary, prepl)
        standby, slog = fixture.start("standby", spg, srepl, "standby", prepl, ppg)
        fixture.ready(standby, slog, spg, standby=True)
        source = helper.Wire(fixture, primary, ppg, "primary")
        replica = helper.Wire(fixture, standby, spg, "standby")
        for sql in ["CREATE TABLE standby_write_probe (id INT PRIMARY KEY, note TEXT)",
                    "BEGIN", "INSERT INTO standby_write_probe VALUES (1, 'seed')", "COMMIT"]:
            response = source.simple(sql)
            if response["errors"]:
                raise RuntimeError(f"fixture setup failed: {response}")
        before = [["1", "seed"]]
        helper.exact_rows(source, before)
        helper.exact_rows(replica, before)
        # Source controls distinguish broken protocol/fixture from denied writes.
        control_mode = "copy" if mode == "copy" else "extended"
        control = source.write(control_mode, 2, "primary-control")
        if control["errors"] or not control["tags"] or (control_mode == "copy" and not control["copy_entered"]):
            raise RuntimeError(f"primary protocol control failed: {control}")
        before += [["2", "primary-control"]]
        helper.exact_rows(source, before)
        helper.exact_rows(replica, before)
        if mode == "copy":
            response = replica.write("copy", 90, "must-not-write")
            check_refusal(response)
            result["checks"].append("autocommit COPY refused before CopyInResponse")
            recovered_read(replica)
            check(not replica.simple("BEGIN")["errors"], "BEGIN refused")
            response = replica.write("copy", 91, "must-not-write")
            check_refusal(response, "E")
            failed = replica.simple("SELECT 1")
            check([e.get("C") for e in failed["errors"]] == ["25P02"] and failed.get("ready_status") == "E", f"transaction did not fail: {failed}")
            check(not replica.simple("ROLLBACK")["errors"], "ROLLBACK recovery failed")
            recovered_read(replica)
            result["checks"].append("COPY denial marks transaction failed; ROLLBACK recovers")
        elif mode == "extended":
            extended_read(replica, parse=True)
            parse_bind(replica, b"named_write", b"named_portal", 90)
            reject_execute(replica, b"named_portal")
            recovered_read(replica)
            # Reuse the same named prepared statement after Sync; no reparsing.
            parse_bind(replica, b"named_write", b"cached_portal", 91, parse=False)
            reject_execute(replica, b"cached_portal")
            recovered_read(replica)
            parse_bind(replica, b"", b"", 92)
            reject_execute(replica, b"")
            recovered_read(replica)
            parse_bind(replica, b"returning_write", b"returning_portal", 94, returning=True)
            reject_execute(replica, b"returning_portal")
            extended_read(replica, parse=False)
            check(not replica.simple("BEGIN")["errors"], "BEGIN refused")
            parse_bind(replica, b"named_write", b"transaction_portal", 93, parse=False)
            reject_execute(replica, b"transaction_portal", "E")
            failed = replica.simple("SELECT 1")
            check([e.get("C") for e in failed["errors"]] == ["25P02"], "prepared denial did not mark transaction failed")
            check(not replica.simple("ROLLBACK")["errors"], "ROLLBACK recovery failed")
            recovered_read(replica)
            result["checks"].append("named, unnamed, reused and RETURNING prepared writes denied at Execute; cached reads, Sync and rollback recover")
        else:
            for sql in [
                "/* prefix */ INSERT INTO standby_write_probe VALUES (90, 'blocked')",
                "UPDATE standby_write_probe SET note='blocked' WHERE id=1",
                "DELETE FROM standby_write_probe WHERE id=1",
                "SELECT * INTO forbidden_copy FROM standby_write_probe",
                "CREATE TABLE forbidden_ddl (id INT)",
                "DO $$ INSERT INTO standby_write_probe VALUES (91, 'blocked'); $$",
                "WITH c AS (SELECT 92 AS id) INSERT INTO standby_write_probe SELECT id, 'blocked' FROM c",
                "SELECT 1; INSERT INTO standby_write_probe VALUES (93, 'blocked')",
            ]:
                response = replica.simple(sql)
                # A preceding read in a simple-query batch may emit rows before
                # the write is denied. It still owes exactly one error and Ready.
                if sql.startswith("SELECT 1;"):
                    check([e.get("C") for e in response["errors"]] == ["25006"]
                          and not response["copy_entered"] and response.get("ready_status") == "I", str(response))
                else:
                    check_refusal(response)
                recovered_read(replica)
                result["checks"].append(sql)
        # Query aliases defeat stale result cache reuse; exact source/replica
        # rows rule out forwarding and local writes, rather than trusting tags.
        for _ in range(3):
            check(source.rows() == before and replica.rows() == before,
                  "exact source/replica rows changed after rejected statements")
        result["status"] = "pass"
    except AssertionError as error:
        result.update(status="regression_failed", error=str(error), traceback=traceback.format_exc())
    except (Exception, KeyboardInterrupt) as error:
        result.update(status="inconclusive", error=repr(error), traceback=traceback.format_exc())
    finally:
        try:
            fixture.close()
        except BaseException as error:
            result.update(status="inconclusive", cleanup_error=repr(error))
        (fixture.root / "admission-result.json").write_text(json.dumps(result, indent=2) + "\n")
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--output-dir", type=Path, required=True)
    parser.add_argument("--helper", type=Path, default=Path(__file__).resolve().parent.parent / "probe_standby_wire_writes.py")
    parser.add_argument("--mode", choices=["all", "simple", "copy", "extended"], default="all")
    args = parser.parse_args()
    helper_path = args.helper.resolve(strict=True)
    helper = load_helper(helper_path)
    binary = args.binary.resolve(strict=True)
    script = Path(__file__).resolve(strict=True)
    args.output_dir.mkdir(parents=True, exist_ok=True)
    root = Path(tempfile.mkdtemp(prefix="standby-admission-", dir=args.output_dir.resolve()))
    identities = {str(path): helper.digest(path) for path in (binary, script, helper_path)}
    result = {"identities_before": identities, "fixture_root": str(root), "modes": []}
    def interrupted(number, _frame):
        raise InterruptedError(f"interrupted by signal {number}")
    for sig in (signal.SIGINT, signal.SIGTERM):
        signal.signal(sig, interrupted)
    modes = ["simple", "copy", "extended"] if args.mode == "all" else [args.mode]
    try:
        for mode in modes:
            result["modes"].append(run_mode(helper, root, binary, mode))
            if result["modes"][-1]["status"] == "inconclusive":
                break
    finally:
        try:
            result["identities_after"] = {str(path): helper.digest(path) for path in (binary, script, helper_path)}
            result["identity_unchanged"] = result["identities_after"] == identities
        except Exception as error:
            result.update(identity_unchanged=False, identity_error=repr(error))
        (root / "result.json").write_text(json.dumps(result, indent=2) + "\n")
        print(root, flush=True)
    if not result["identity_unchanged"] or len(result["modes"]) != len(modes) or any(r["status"] == "inconclusive" for r in result["modes"]):
        return 2
    return 1 if any(r["status"] == "regression_failed" for r in result["modes"]) else 0


if __name__ == "__main__":
    try:
        code = main()
    except (Exception, KeyboardInterrupt) as error:
        print(f"inconclusive harness setup: {error!r}", flush=True)
        code = 2
    raise SystemExit(code)
