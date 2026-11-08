"""Pure receipt fixtures. Never builds, lists or executes any native artifact."""
import copy
import importlib.util
import json
from pathlib import Path
import socket
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

sys.dont_write_bytecode = True
spec = importlib.util.spec_from_file_location("native_ci_proof_fixture", Path(__file__).with_name("native_ci_proof.py"))
proof = importlib.util.module_from_spec(spec)
spec.loader.exec_module(proof)


def forbidden(*args, **kwargs):
    raise AssertionError("child process/network reached by pure fixture")


class NativeProofFixtures(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.root = Path(self.temp.name) / "proof"
        self.tree = Path(self.temp.name) / "tree"
        self.target = Path(self.temp.name) / "target"
        self.root.mkdir()
        self.sequence = 100
        self.blocks = [patch.object(subprocess, name, forbidden) for name in ("Popen", "run", "check_output")]
        self.blocks += [patch.object(socket, "create_connection", forbidden)]
        for block in self.blocks:
            block.start()
        self.seed()

    def tearDown(self):
        for block in reversed(self.blocks):
            block.stop()
        self.temp.cleanup()

    def put(self, name, data):
        path = self.root / name
        if isinstance(data, dict):
            path.write_text(json.dumps(data))
        else:
            path.write_text(data)

    def receipt(self, stem, argv, stdout):
        self.sequence += 1
        self.put(stem + ".stdout", stdout)
        self.put(stem + ".stderr", "")
        row = {"argv": argv, "cwd": str(self.tree), "exit_code": 0, "timed_out": False,
               "leader_reaped": True, "pid": self.sequence, "birth_ticks": 1000 + self.sequence,
               "process_group": self.sequence, "elapsed_seconds": 0.001,
               "stdin": "null", "start_new_session": True, "timeout_seconds": 120,
               "stdout": stem + ".stdout", "stderr": stem + ".stderr",
               "stdout_sha256": proof.sha(self.root / (stem + ".stdout")),
               "stderr_sha256": proof.sha(self.root / (stem + ".stderr"))}
        self.put(stem + ".json", row)
        return row

    def seed(self):
        elf = bytearray(20)
        elf[:6] = b"\x7fELF\x02\x01"
        elf[18:20] = (62).to_bytes(2, "little")
        (self.root / "native-tps.elf").write_bytes(elf)
        self.elf = proof.elf_identity(self.root / "native-tps.elf")
        self.binary = str(self.target / "perf/deps/tps_workloads-synthetic")
        Path(self.binary).parent.mkdir(parents=True)
        Path(self.binary).write_bytes(elf)
        source = {"head": "synthetic-source-not-a-runtime-receipt", "files": {
            name: {"sha256": "fixture-" + name} for name in
            ("Cargo.toml", "Cargo.lock", "tests/tps_workloads.rs", "benches/public/native_ci_proof.py")},
                  "ancestor_cargo_configs": {"fixture": {"present": False}}}
        self.source_capture = {name: row["sha256"] for name, row in source["files"].items()}
        for name in ("source-before.json", "source-after.json"):
            self.put(name, source)
        tools = {name: {"path": "/synthetic/" + name, "sha256": "fixture", "version": "fixture"}
                 for name in ("cargo", "rustc", "rustdoc")}
        for name in ("tools-before.json", "tools-after.json"):
            self.put(name, tools)
        self.put("resources.json", {"limits": {"memory.max": str(24 * 1024**3), "memory.swap.max": "0"}})
        self.artifact = {"reason": "compiler-artifact", "fresh": False,
                         "target": {"name": "tps_workloads", "kind": ["test"],
                                    "src_path": str(self.tree / "tests/tps_workloads.rs")},
                         "manifest_path": str(self.tree / "Cargo.toml"), "executable": self.binary,
                         "features": proof.FEATURES,
                         "profile": {"test": True, "opt_level": "3", "debuginfo": 1, "debug_assertions": False}}
        build = [tools["cargo"]["path"], "test", "--locked", "--profile", "perf", "--test",
                 "tps_workloads", "--no-run", "--message-format=json"]
        self.receipt("build", build, json.dumps(self.artifact) + '\n{"reason":"build-finished","success":true}\n')
        self.receipt("list-all", [self.binary, "--list", "--format=terse"],
                     "\n".join(name + ": test" for name in list(proof.SHAPES) + ["another_test"]) + "\n")
        for test in proof.SHAPES:
            for suffix, stdout, extra in [("", test + ": test\n", []), ("-ignored", "", ["--ignored"])]:
                self.receipt("list-" + test + suffix,
                             [self.binary, test, "--exact", "--list", "--format=terse"] + extra, stdout)
        invocations = []
        for rep in range(1, 4):
            for test, shape in proof.SHAPES.items():
                stem = "%02d-%s" % (rep, test)
                text = "\nrunning 1 test\n" + "\n".join(
                    "%s %d ops 0.001 s 100.0 ops/s 10.0 us/op" % (label, count)
                    for label, count in shape["operations"].items())
                text += "\n.\ntest result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 2 filtered out;\n"
                process = self.receipt(stem, [self.binary, test, "--exact", "--nocapture",
                                              "--test-threads=1", "--format=terse"], text)
                process["native_execution"] = {"executable": self.binary, "argv": process["argv"],
                                               "sha256": self.elf["sha256"]}
                self.put(stem + ".json", process)
                name = stem + "-completion.json"
                self.put(name, {"repetition": rep, "test": test, "environment": shape["environment"],
                                "elf_before": self.elf, "elf_after": self.elf, "process": process})
                invocations.append(name)
        self.complete = {"schema": proof.SCHEMA, "tree": str(self.tree), "target": str(self.target),
                         "repetitions": 3, "source_context_sha256": proof.data_sha(source),
                         "compiler_artifact": self.artifact, "native_elf": self.elf,
                         "invocations": invocations}
        self.seal()

    def seal(self):
        self.complete["files"] = {str(path.relative_to(self.root)): proof.sha(path)
                                  for path in self.root.iterdir() if path.name != "COMPLETE.json"}
        self.put("COMPLETE.json", self.complete)

    def validate(self):
        return proof.validate_complete(self.root, self.tree, self.target, source_capture=self.source_capture)

    def mutate_native(self, change):
        name = self.complete["invocations"][0]
        row = proof.read_json(self.root / name)
        change(row)
        self.put(name, row)

    def test_valid_synthetic_packet_without_execution(self):
        self.assertEqual(self.validate()["native_tests"], 6)
        self.assertEqual(self.validate()["workloads"], 12)

    def test_missing_and_duplicate_repetition(self):
        self.complete["invocations"].pop()
        self.seal()
        with self.assertRaises(ValueError):
            self.validate()
        self.complete["invocations"].append(self.complete["invocations"][0])
        self.seal()
        with self.assertRaises(ValueError):
            self.validate()

    def test_wrong_test_or_environment(self):
        self.mutate_native(lambda row: row.update(test="another_test"))
        self.seal()
        with self.assertRaises((ValueError, KeyError)):
            self.validate()

    def test_zero_ignored_or_filtered_summary(self):
        name = "01-run_tps_suite.stdout"
        original = (self.root / name).read_text()
        for wrong in ("0 passed; 0 failed; 0 ignored", "1 passed; 0 failed; 1 ignored"):
            with self.subTest(summary=wrong):
                text = original.replace("1 passed; 0 failed; 0 ignored", wrong)
                with self.assertRaises(ValueError):
                    proof.check_native(text, "run_tps_suite", 3)
        with self.assertRaises(ValueError):
            proof.check_native(original.replace("2 filtered out", "0 filtered out"), "run_tps_suite", 3)

    def test_wrong_elf_and_integrity(self):
        (self.root / "native-tps.elf").write_bytes(b"not an ELF, never executed")
        self.seal()
        with self.assertRaises(ValueError):
            self.validate()

    def test_actual_native_process_identity_is_required(self):
        self.mutate_native(lambda row: row["process"].update(native_execution={
            "executable": "/wrong/ELF", "argv": row["process"]["argv"], "sha256": self.elf["sha256"]}))
        row = proof.read_json(self.root / self.complete["invocations"][0])
        self.put("01-run_tps_suite.json", row["process"])
        self.seal()
        with self.assertRaises(ValueError):
            self.validate()

    def test_wrong_compiler_artifact_source_profile_or_freshness(self):
        for change in (lambda row: row.update(fresh=True),
                       lambda row: row["target"].update(src_path=str(self.tree / "tests/wrong.rs")),
                       lambda row: row["profile"].update(debuginfo=0),
                       lambda row: row.update(features=proof.FEATURES + ["internal-tests"])):
            value = copy.deepcopy(self.artifact)
            change(value)
            with self.assertRaises(ValueError):
                proof.select_artifact(json.dumps(value) + '\n{"reason":"build-finished","success":true}\n',
                                      self.tree, self.target)

    def test_missing_or_ignored_selected_native_name(self):
        self.put("list-run_tps_suite.stdout", "another_test: test\n")
        row = proof.read_json(self.root / "list-run_tps_suite.json")
        row["stdout_sha256"] = proof.sha(self.root / "list-run_tps_suite.stdout")
        self.put("list-run_tps_suite.json", row)
        self.seal()
        with self.assertRaises(ValueError):
            self.validate()

    def test_source_config_and_independent_capture_mismatch(self):
        self.source_capture["Cargo.lock"] = "wrong"
        with self.assertRaises(ValueError):
            self.validate()
        self.source_capture["Cargo.lock"] = "fixture-Cargo.lock"
        value = proof.read_json(self.root / "source-after.json")
        value["ancestor_cargo_configs"]["fixture"] = {"present": True}
        self.put("source-after.json", value)
        self.seal()
        with self.assertRaises(ValueError):
            self.validate()

    def test_missing_wrong_count_and_duplicate_workloads(self):
        text = (self.root / "01-run_tps_suite.stdout").read_text()
        for altered in (text.replace("bulk_insert_users(txn) 1000 ops", "bulk_insert_users(txn) 1 ops"),
                        text.replace("bulk_insert_users(txn)", "unknown_workload"),
                        text + "\nbulk_insert_users(txn) 1000 ops 0.001 s 100 ops/s 10 us/op\n"):
            with self.assertRaises(ValueError):
                proof.check_native(altered, "run_tps_suite", 3)

    def test_mutated_log_hash_is_rejected(self):
        with (self.root / "01-run_tps_suite.stdout").open("a") as stream:
            stream.write("\nchanged after completion\n")
        with self.assertRaises(ValueError):
            self.validate()

    def test_timeout_unreaped_and_wrong_argv_are_rejected(self):
        for change in (lambda row: row.update(timed_out=True),
                       lambda row: row.update(leader_reaped=False),
                       lambda row: row.update(argv=[self.binary, "wrong_test"])):
            row = proof.read_json(self.root / "01-run_tps_suite.json")
            change(row)
            with self.assertRaises(ValueError):
                proof.checked_process(self.root, row,
                    [self.binary, "run_tps_suite", "--exact", "--nocapture", "--test-threads=1", "--format=terse"],
                    self.tree)

    def test_exact_three_samples_best_of_three_and_cliff(self):
        expected, raw = {}, b""
        for name in self.complete["invocations"]:
            row = proof.read_json(self.root / name)
            process = row["process"]
            raw += (self.root / process["stdout"]).read_bytes()
            raw += (self.root / process["stderr"]).read_bytes()
            for label, count in proof.SHAPES[row["test"]]["operations"].items():
                expected[label] = {"ops": count, "secs": 0.001, "ops_per_sec": 100.0,
                                   "us_per_op": 10.0, "unit": "ops/s", "runs_seen": 3}
        baseline = {label: {"ops_per_sec": 100.0} for label in expected}
        proof.validate_results(self.root, expected, raw, baseline)
        for change in (lambda d: d.pop(next(iter(d))),
                       lambda d: d[next(iter(d))].update(runs_seen=2),
                       lambda d: d[next(iter(d))].update(ops_per_sec=200.0)):
            altered = copy.deepcopy(expected)
            change(altered)
            with self.assertRaises(ValueError):
                proof.validate_results(self.root, altered, raw, baseline)
        with self.assertRaises(ValueError):
            proof.validate_results(self.root, expected, raw + b"extra raw data", baseline)
        baseline[next(iter(baseline))]["ops_per_sec"] = 251.0
        with self.assertRaises(ValueError):
            proof.validate_results(self.root, expected, raw, baseline)


if __name__ == "__main__":
    unittest.main()
