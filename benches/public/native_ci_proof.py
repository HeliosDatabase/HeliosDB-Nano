#!/usr/bin/env python3
"""Native completion proof for the opt-in, unchanged public CI workload set."""
import argparse
import hashlib
import json
import math
import os
from pathlib import Path
import re
import shutil
import signal
import subprocess
import sys
import time

sys.dont_write_bytecode = True
SCHEMA = "nano-public-perf-native-v1"
FEATURES = ["default", "encryption", "ha-tier1", "ring-crypto", "vector-search"]
SHAPES = {
    "run_tps_suite": {
        "environment": {"HELIOS_TPS": "1", "HELIOS_TPS_MODE": "mem", "HELIOS_TPS_N": "1000", "HELIOS_TPS_M": "200"},
        "operations": {"bulk_insert_users(txn)": 1000, "autocommit_insert": 200, "point_lookup_pk": 200,
                       "point_lookup_hot": 200, "update_by_pk": 200, "delete_by_pk": 200,
                       "filter_scan(age>50)": 20, "agg_count_sum_avg": 20, "group_by_status": 20,
                       "join_users_orders": 10, "order_by_limit10": 20},
    },
    "run_fk_bulk_insert_bench": {
        "environment": {"HELIOS_FK_BULK": "1", "HELIOS_FK_PARENTS": "1000", "HELIOS_FK_CHILDREN": "2000"},
        "operations": {"fk_child_insert_txn": 2000},
    },
}
SUMMARY = re.compile(r"^test result: (ok|FAILED)\. (\d+) passed; (\d+) failed; (\d+) ignored; (\d+) measured; (\d+) filtered out;", re.M)
WORK = re.compile(r"^(\S+)\s+(\d+) ops\s+([\d.]+) s\s+([\d.]+) ops/s\s+([\d.]+) us/op\s*$", re.M)


def require(value, message):
    if not value:
        raise ValueError(message)


def sha(path):
    digest = hashlib.sha256()
    with Path(path).open("rb") as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(block)
    return digest.hexdigest()


def write(path, value):
    with Path(path).open("x") as stream:
        stream.write(json.dumps(value, indent=2, sort_keys=True) + "\n")


def data_sha(value):
    return hashlib.sha256(json.dumps(value, sort_keys=True, separators=(",", ":")).encode()).hexdigest()


def read_json(path):
    return json.loads(Path(path).read_bytes())


def command(argv, tree, env):
    return subprocess.check_output(argv, cwd=tree, env=env, timeout=30).decode().strip()


def source_context(tree, env):
    head = command(["git", "rev-parse", "HEAD"], tree, env)
    require(not command(["git", "status", "--porcelain", "--untracked-files=all"], tree, env),
            "native proof requires committed, unchanged source")
    paths = command(["git", "ls-files", "-z"], tree, env).split("\0")
    files = {}
    for name in filter(None, paths):
        path = tree / name
        if path.is_symlink():
            files[name] = {"symlink": os.readlink(path)}
        else:
            require(path.is_file(), "missing tracked source: " + name)
            files[name] = {"sha256": sha(path)}
    configs = {}
    locations = [tree] + list(tree.parents)
    cargo_home = Path(env.get("CARGO_HOME", str(Path.home() / ".cargo"))).resolve()
    for location in locations:
        for name in ("config", "config.toml"):
            path = location / ".cargo" / name
            configs[str(path)] = config_identity(path)
    for name in ("config", "config.toml"):
        path = cargo_home / name
        configs[str(path)] = config_identity(path)
    return {"head": head, "files": files, "ancestor_cargo_configs": configs}


def config_identity(path):
    if not os.path.lexists(path):
        return {"present": False}
    require(path.is_file(), "unreadable Cargo configuration: " + str(path))
    return {"present": True, "sha256": sha(path),
            "symlink_text_sha256": hashlib.sha256(os.readlink(path).encode()).hexdigest() if path.is_symlink() else None}


def tools_context(tree, env):
    result = {}
    for name in ("cargo", "rustc", "rustdoc"):
        path = Path(command(["rustup", "which", name], tree, env)).resolve()
        result[name] = {"path": str(path), "sha256": sha(path),
                        "version": command([str(path), "-vV" if name == "rustc" else "--version"], tree, env)}
    return result


def birth(pid):
    # Popen owns the unreaped child, so even an already-exited short --list
    # process retains this identity until wait() reaps it.
    return int(Path("/proc/%d/stat" % pid).read_text().rsplit(")", 1)[1].split()[19])


def run(argv, tree, env, root, stem, timeout, forward=False, native_elf=None):
    stdout, stderr = root / (stem + ".stdout"), root / (stem + ".stderr")
    record = {"argv": argv, "cwd": str(tree), "timeout_seconds": timeout, "timed_out": False,
              "stdin": "null", "start_new_session": True}
    child = None
    ticks = None
    started = time.monotonic()
    try:
        with stdout.open("xb") as out, stderr.open("xb") as err:
            child = subprocess.Popen(argv, cwd=tree, env=env, stdin=subprocess.DEVNULL,
                                     stdout=out, stderr=err, start_new_session=True)
            ticks = birth(child.pid)
            record.update(pid=child.pid, birth_ticks=ticks, process_group=os.getpgid(child.pid))
            require(record["process_group"] == child.pid, "native child process group differs")
            if native_elf is not None:
                proc = Path("/proc/%d" % child.pid)
                actual_argv = (proc / "cmdline").read_bytes().rstrip(b"\0").split(b"\0")
                actual_argv = [os.fsdecode(value) for value in actual_argv]
                actual_exe = str((proc / "exe").resolve(strict=True))
                require(actual_argv == argv and actual_exe == argv[0]
                        and sha(proc / "exe") == native_elf["sha256"],
                        "actual native process executable/argv differs")
                record["native_execution"] = {"executable": actual_exe, "argv": actual_argv,
                                              "sha256": native_elf["sha256"]}
            try:
                child.wait(timeout=timeout)
            except subprocess.TimeoutExpired:
                record["timed_out"] = True
                require(birth(child.pid) == ticks and os.getpgid(child.pid) == child.pid,
                        "owned native timeout identity changed")
                os.killpg(child.pid, signal.SIGTERM)
                try:
                    child.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    require(birth(child.pid) == ticks and os.getpgid(child.pid) == child.pid,
                            "owned native kill identity changed")
                    os.killpg(child.pid, signal.SIGKILL)
                    child.wait(timeout=10)
            record.update(exit_code=child.returncode, leader_reaped=True)
    finally:
        if child is not None and child.poll() is None:
            # The Popen child is still unreaped, so its PID cannot be reused
            # even if reading its initial birth ticks itself failed.
            require((ticks is None or birth(child.pid) == ticks) and os.getpgid(child.pid) == child.pid,
                    "native cleanup identity changed")
            os.killpg(child.pid, signal.SIGKILL)
            child.wait(timeout=10)
        record.update(elapsed_seconds=time.monotonic() - started)
        if stdout.exists() and stderr.exists():
            record.update(stdout=stdout.name, stderr=stderr.name,
                          stdout_sha256=sha(stdout), stderr_sha256=sha(stderr))
            write(root / (stem + ".json"), record)
            if forward:
                sys.stdout.buffer.write(stdout.read_bytes())
                sys.stdout.buffer.flush()
                sys.stderr.buffer.write(stderr.read_bytes())
                sys.stderr.buffer.flush()
    return record


def elf_identity(path):
    with Path(path).open("rb") as stream:
        header = stream.read(20)
    require(len(header) == 20 and header[:6] == b"\x7fELF\x02\x01", "not a native64 little-endian ELF")
    machine = int.from_bytes(header[18:20], "little")
    require(machine in (62, 183), "unsupported native ELF machine")
    return {"sha256": sha(path), "machine": machine}


def select_artifact(text, tree, target):
    messages = [json.loads(line) for line in text.splitlines() if line.strip()]
    finished = [row["success"] for row in messages if row.get("reason") == "build-finished"]
    require(finished == [True], "missing/failed/duplicate Cargo build-finished")
    require(not any(row.get("reason") == "compiler-message" and row.get("message", {}).get("level") == "error"
                    for row in messages), "compiler error in native build")
    found = [row for row in messages if row.get("reason") == "compiler-artifact"
             and row.get("target", {}).get("name") == "tps_workloads" and row.get("executable")]
    require(len(found) == 1, "exactly one compiler-owned TPS executable required")
    row = found[0]
    require(row["fresh"] is False, "stale/fresh Cargo TPS artifact is not source compilation proof")
    require(row["target"]["kind"] == ["test"] and row["profile"]["test"] is True,
            "wrong Cargo target kind/profile")
    require(sorted(row["features"]) == FEATURES and row["profile"]["opt_level"] == "3"
            and row["profile"]["debuginfo"] == 1 and not row["profile"]["debug_assertions"],
            "default features/perf profile changed")
    require(Path(row["manifest_path"]).resolve() == tree / "Cargo.toml"
            and Path(row["target"]["src_path"]).resolve() == tree / "tests/tps_workloads.rs",
            "wrong compiled source target")
    executable = Path(row["executable"]).resolve()
    require(executable.parent == target / "perf/deps", "compiler executable escapes owned perf target")
    return row


def names(text):
    rows = text.splitlines()
    require(all(row.endswith(": test") for row in rows), "malformed native test listing")
    values = [row[:-6] for row in rows]
    require(len(values) == len(set(values)), "duplicate native listed tests")
    return values


def check_native(text, test, total):
    require(SUMMARY.findall(text) == [("ok", "1", "0", "0", "0", str(total - 1))],
            "native test did not execute exactly one passing, nonignored parent")
    require(re.findall(r"^running (\d+) test(?:s)?$", text, re.M) == ["1"],
            "native test count missing/duplicated")
    rows = WORK.findall(text)
    require(len(rows) == len({row[0] for row in rows}), "duplicate native workload")
    actual = {}
    for name, ops, seconds, rate, micros in rows:
        require(all(math.isfinite(float(value)) for value in (seconds, rate, micros))
                and float(rate) > 0 and float(seconds) >= 0 and float(micros) >= 0,
                "invalid workload timing")
        actual[name] = int(ops)
    require(actual == SHAPES[test]["operations"], "missing/extra/wrong-operation workload")


def checked_process(root, record, expected_argv, tree):
    require(record["argv"] == expected_argv and record["cwd"] == str(tree)
            and type(record["exit_code"]) is int and record["exit_code"] == 0
            and record["timed_out"] is False and record["leader_reaped"] is True
            and type(record["pid"]) is int and record["pid"] > 0
            and type(record["birth_ticks"]) is int and record["birth_ticks"] > 0,
            "wrong/failed/unowned process")
    require(record["process_group"] == record["pid"] and record["start_new_session"] is True
            and record["stdin"] == "null" and 0 < record["timeout_seconds"] <= 5400
            and math.isfinite(record["elapsed_seconds"]) and record["elapsed_seconds"] >= 0,
            "unbounded/malformed native process receipt")
    for name in ("stdout", "stderr"):
        path = (root / record[name]).resolve()
        require(path.parent == root and sha(path) == record[name + "_sha256"],
                "process log identity differs")


def validate_complete(root, tree, target, prepared=None, source_capture=None):
    """Pure receipt/file validation, also used by the independent stage consumer."""
    root, tree, target = Path(root).resolve(), Path(tree).resolve(), Path(target).resolve()
    complete = read_json(root / "COMPLETE.json") if prepared is None else prepared
    require(complete["schema"] == SCHEMA and complete["repetitions"] == 3,
            "wrong completion schema/repetition count")
    require(complete["tree"] == str(tree) and complete["target"] == str(target),
            "completion source/target context differs")
    files = complete["files"]
    stems = ["build", "list-all"] + ["list-" + test + suffix for test in SHAPES for suffix in ("", "-ignored")]
    native_stems = ["%02d-%s" % (rep, test) for rep in range(1, 4) for test in SHAPES]
    wanted_files = {"source-before.json", "source-after.json", "tools-before.json", "tools-after.json",
                    "resources.json", "native-tps.elf"}
    wanted_files.update(stem + suffix for stem in stems + native_stems for suffix in (".json", ".stdout", ".stderr"))
    wanted_files.update(stem + "-completion.json" for stem in native_stems)
    require(set(files) == wanted_files, "missing/unexpected native proof file identities")
    actual_files = {str(path.relative_to(root)) for path in root.rglob("*") if path.is_file()}
    require(actual_files == set(files) | ({"COMPLETE.json"} if prepared is None else set()),
            "missing/extra proof files")
    for name, expected in files.items():
        path = (root / name).resolve()
        require(path.is_relative_to(root) and not (root / name).is_symlink()
                and sha(path) == expected, "proof file hash/path changed")
    before, after = read_json(root / "source-before.json"), read_json(root / "source-after.json")
    require(before == after and data_sha(before) == complete["source_context_sha256"], "source/config changed")
    if source_capture is not None:
        tracked = before["files"]
        require(set(source_capture) <= set(tracked), "proof source path set differs from independent capture")
        for name, record in tracked.items():
            if "sha256" in record:
                require(source_capture.get(name) == record["sha256"], "compiled source differs: " + name)
        require(all(name in tracked for name in ("Cargo.toml", "Cargo.lock", "tests/tps_workloads.rs",
                                                "benches/public/native_ci_proof.py")),
                "required compiled source is missing")
    require(read_json(root / "resources.json")["limits"] ==
            {"memory.max": str(24 * 1024**3), "memory.swap.max": "0"}, "unbounded native proof")
    tools = read_json(root / "tools-before.json")
    require(tools == read_json(root / "tools-after.json"), "compiler identity changed")
    build = read_json(root / "build.json")
    expected_build = [tools["cargo"]["path"], "test", "--locked", "--profile", "perf",
                      "--test", "tps_workloads", "--no-run", "--message-format=json"]
    checked_process(root, build, expected_build, tree)
    artifact = select_artifact((root / "build.stdout").read_text(), tree, target)
    require(artifact == complete["compiler_artifact"], "compiled artifact changed")
    binary = str(Path(artifact["executable"]).resolve())
    require(elf_identity(root / "native-tps.elf") == complete["native_elf"], "preserved native ELF differs")
    require(elf_identity(binary) == complete["native_elf"], "compiler executable changed after native invocations")
    full = read_json(root / "list-all.json")
    checked_process(root, full, [binary, "--list", "--format=terse"], tree)
    listed = names((root / "list-all.stdout").read_text())
    require(set(SHAPES) <= set(listed), "intended native tests missing")
    for test in SHAPES:
        for suffix, expected_names, extra in [("", [test], []), ("-ignored", [], ["--ignored"])]:
            stem = "list-" + test + suffix
            row = read_json(root / (stem + ".json"))
            checked_process(root, row, [binary, test, "--exact", "--list", "--format=terse"] + extra, tree)
            require(names((root / (stem + ".stdout")).read_text()) == expected_names,
                    "intended native test is missing/ignored/wrong")
    identities, process_identities = set(), set()
    for name in complete["invocations"]:
        row = read_json(root / name)
        key = (row["repetition"], row["test"])
        require(key not in identities, "duplicate invocation identity")
        identities.add(key)
        require(row["test"] in SHAPES and row["environment"] == SHAPES[row["test"]]["environment"],
                "wrong native workload environment")
        stem = "%02d-%s" % key
        require(name == stem + "-completion.json" and row["process"] == read_json(root / (stem + ".json")),
                "native completion does not match its actual process receipt")
        expected = [binary, row["test"], "--exact", "--nocapture", "--test-threads=1", "--format=terse"]
        checked_process(root, row["process"], expected, tree)
        identity = (row["process"]["pid"], row["process"]["birth_ticks"])
        require(identity not in process_identities, "native process receipt reused across invocations")
        process_identities.add(identity)
        require(row["elf_before"] == row["elf_after"] == complete["native_elf"], "native executable changed")
        require(row["process"].get("native_execution") == {
            "executable": binary, "argv": expected, "sha256": complete["native_elf"]["sha256"]},
            "actual native process identity missing/different")
        check_native((root / row["process"]["stdout"]).read_text(), row["test"], len(listed))
    require(identities == {(rep, test) for rep in range(1, 4) for test in SHAPES},
            "missing/extra repetition or intended test")
    return {"schema": SCHEMA, "completion_sha256": sha(root / "COMPLETE.json") if prepared is None else None,
            "native_elf": complete["native_elf"], "source_context_sha256": complete["source_context_sha256"],
            "native_tests": 6, "repetitions": 3, "workloads": 12,
            "compiler_artifact": artifact, "release_acceptance_pass": False}


def validate_results(root, measured, raw, baseline):
    """Bind the unchanged best-of-three table to all six native stdout logs."""
    root = Path(root)
    complete = read_json(root / "COMPLETE.json")
    expected, raw_parts = {}, []
    for name in complete["invocations"]:
        row = read_json(root / name)
        process = row["process"]
        stdout = (root / process["stdout"]).read_bytes()
        raw_parts.extend([stdout, (root / process["stderr"]).read_bytes()])
        for label, ops, seconds, rate, micros in WORK.findall(stdout.decode()):
            entry = {"ops": int(ops), "secs": float(seconds), "ops_per_sec": float(rate),
                     "us_per_op": float(micros), "unit": "ops/s", "runs_seen": 1}
            previous = expected.get(label)
            if previous is not None:
                entry["runs_seen"] = previous["runs_seen"] + 1
            if previous is None or entry["ops_per_sec"] >= previous["ops_per_sec"]:
                expected[label] = entry
            else:
                previous["runs_seen"] = entry["runs_seen"]
    require(raw == b"".join(raw_parts), "public raw log differs from actual native logs")
    require(len(expected) == 12 and all(item["runs_seen"] == 3 for item in expected.values())
            and measured == expected and set(baseline) == set(expected),
            "public measured table is not exact best-of-three native output")
    require(all(math.isfinite(item["ops_per_sec"]) and item["ops_per_sec"] > 0
                and measured[label]["ops_per_sec"] >= item["ops_per_sec"] / 2.5
                for label, item in baseline.items()), "public baseline 2.5x cliff exceeded")


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--tree", type=Path, required=True)
    parser.add_argument("--target", type=Path, required=True)
    parser.add_argument("--out", type=Path, required=True)
    parser.add_argument("--repetitions", type=int, default=3)
    args = parser.parse_args()
    tree, target, root = args.tree.resolve(), args.target.resolve(), args.out.resolve()
    require(sys.platform == "linux" and args.repetitions == 3, "proof is Linux CI with exactly three repetitions")
    require(os.environ.get("HELIOS_PERF_NATIVE_PROOF") == "1", "explicit native proof mode required")
    require(os.environ.get("NANO_FINAL_GATE_BOUNDED") == "1", "outer fleet-serialized gate required")
    require(not (target / "perf").exists(), "perf target must be fresh before proof-owned compilation")
    require(not root.exists(), "completion output must be new")
    root.mkdir(parents=True)
    group = next(line.split(":", 2)[2].strip() for line in Path("/proc/self/cgroup").read_text().splitlines()
                 if line.startswith("0::"))
    cgroup = Path("/sys/fs/cgroup") / group.lstrip("/")
    limits = {name: (cgroup / name).read_text().strip() for name in ("memory.max", "memory.swap.max")}
    require(limits == {"memory.max": str(24 * 1024**3), "memory.swap.max": "0"}, "24GiB/no-swap scope required")
    write(root / "resources.json", {"cgroup": group, "limits": limits,
                                   "fleet_lock_owner": "outer frozen gate controller"})
    env = os.environ.copy()
    require(not env.get("RUSTFLAGS") and not env.get("CARGO_ENCODED_RUSTFLAGS")
            and not env.get("RUSTC_WRAPPER") and not env.get("RUSTC_WORKSPACE_WRAPPER")
            and not env.get("CARGO_BUILD_TARGET")
            and not any(key.startswith("CARGO_PROFILE_PERF_") for key in env),
            "undeclared compiler/profile override")
    fixed_knobs = {key: value for shape in SHAPES.values() for key, value in shape["environment"].items()}
    inherited = [key for key, value in env.items()
                 if (key.startswith("HELIOS_TPS") or key.startswith("HELIOS_FK"))
                 and fixed_knobs.get(key) != value]
    require(not inherited, "unsupported inherited workload knobs: " + ", ".join(sorted(inherited)))
    # Never log inherited credentials or arbitrary HELIOS variables. Pin only
    # this gate's seven public workload knobs; internal libtest invoke controls
    # cannot turn a native --list into execution.
    env = {key: value for key, value in env.items()
           if not key.startswith("HELIOS_") and not key.startswith("__RUST_TEST_INVOKE")}
    env.update(CARGO_TARGET_DIR=str(target), CARGO_TERM_COLOR="never", RUST_TEST_THREADS="1")
    before = source_context(tree, env)
    tools = tools_context(tree, env)
    write(root / "source-before.json", before)
    write(root / "tools-before.json", tools)
    env.update(RUSTC=tools["rustc"]["path"], RUSTDOC=tools["rustdoc"]["path"])
    build_argv = [tools["cargo"]["path"], "test", "--locked", "--profile", "perf", "--test",
                  "tps_workloads", "--no-run", "--message-format=json"]
    built = run(build_argv, tree, env, root, "build", 5400)
    require(built["exit_code"] == 0 and not built["timed_out"], "proof native build failed")
    artifact = select_artifact((root / "build.stdout").read_text(), tree, target)
    binary = Path(artifact["executable"]).resolve()
    elf = elf_identity(binary)
    shutil.copyfile(binary, root / "native-tps.elf")
    require(elf_identity(root / "native-tps.elf") == elf, "native ELF copy differs")
    listing = run([str(binary), "--list", "--format=terse"], tree, env, root, "list-all", 30)
    require(listing["exit_code"] == 0 and not listing["timed_out"], "native list failed")
    all_names = names((root / "list-all.stdout").read_text())
    for test in SHAPES:
        for suffix, wanted, extra in [("", [test], []), ("-ignored", [], ["--ignored"])]:
            stem = "list-" + test + suffix
            listed = run([str(binary), test, "--exact", "--list", "--format=terse"] + extra,
                         tree, env, root, stem, 30)
            require(listed["exit_code"] == 0 and not listed["timed_out"]
                    and names((root / (stem + ".stdout")).read_text()) == wanted, "exact native list mismatch")
    invocations = []
    for rep in range(1, 4):
        for test, shape in SHAPES.items():
            run_env = dict(env)
            run_env.update(shape["environment"])
            stem = "%02d-%s" % (rep, test)
            first = elf_identity(binary)
            process = run([str(binary), test, "--exact", "--nocapture", "--test-threads=1", "--format=terse"],
                          tree, run_env, root, stem, 120, forward=True, native_elf=elf)
            last = elf_identity(binary)
            require(process["exit_code"] == 0 and not process["timed_out"], "native workload failed")
            check_native((root / process["stdout"]).read_text(), test, len(all_names))
            require(first == last == elf, "native workload ELF changed")
            receipt = {"repetition": rep, "test": test, "environment": shape["environment"],
                       "elf_before": first, "elf_after": last, "process": process}
            name = stem + "-completion.json"
            write(root / name, receipt)
            invocations.append(name)
    after = source_context(tree, env)
    after_tools = tools_context(tree, env)
    write(root / "source-after.json", after)
    write(root / "tools-after.json", after_tools)
    require(before == after and tools == after_tools, "source/config/compiler changed during native proof")
    files = {str(path.relative_to(root)): sha(path) for path in root.rglob("*") if path.is_file()}
    complete = {"schema": SCHEMA, "tree": str(tree), "target": str(target), "repetitions": 3,
                "source_context_sha256": data_sha(before), "compiler_artifact": artifact,
                "native_elf": elf, "invocations": invocations, "files": files,
                "release_acceptance_pass": False}
    validate_complete(root, tree, target, prepared=complete)
    write(root / "COMPLETE.json", complete)


if __name__ == "__main__":
    try:
        main()
    except (OSError, ValueError, KeyError, subprocess.SubprocessError) as error:
        print("native public perf proof refused: " + str(error), file=sys.stderr)
        raise SystemExit(1)
