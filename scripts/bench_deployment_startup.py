#!/usr/bin/env python3
"""Alternate isolated Nano startup/connect measurements for two exact binaries.

Run ONLY inside the shared host's build lock and bounded scope, for example:
  flock /home/gpc/HDB/sprint/coordination/build.lock \
    systemd-run --user --scope --collect -p MemoryMax=24G -p MemorySwapMax=0 -- \
    python3 scripts/bench_deployment_startup.py --baseline /path/base \
      --candidate /path/candidate --output-dir /path/new-evidence

Startup ends at the first complete PostgreSQL trust-authentication handshake,
not a log message or successful bare TCP connection. Every startup receives a
fresh private persistent data directory. This measures fresh-database startup,
not recovery of an existing database. No production endpoints are contacted.
Linux (/proc required), Python 3.9+, standard library only. No benchmark runs
during import. Every measured handshake is followed by an untimed check that
the owned child holds the loopback listening socket; unrelated peers invalidate
the run even if they successfully authenticate a probe.
"""

import argparse
import csv
import hashlib
import json
import math
import os
from pathlib import Path
import platform
import signal
import socket
import statistics
import struct
import subprocess
import tempfile
import time


def read_exact(connection, size, deadline):
    chunks = []
    while size:
        remaining = deadline - time.perf_counter()
        if remaining <= 0:
            raise TimeoutError("PostgreSQL handshake deadline exceeded")
        connection.settimeout(remaining)
        chunk = connection.recv(size)
        if not chunk:
            raise ConnectionError("PostgreSQL peer closed during startup")
        chunks.append(chunk)
        size -= len(chunk)
    return b"".join(chunks)


def connect_and_authenticate(port, timeout):
    """Return milliseconds from connection attempt through ReadyForQuery."""
    started = time.perf_counter()
    deadline = started + timeout
    with socket.create_connection(("127.0.0.1", port), timeout=timeout) as connection:
        connection.settimeout(max(0.001, deadline - time.perf_counter()))
        parameters = b"user\0postgres\0database\0postgres\0\0"
        connection.sendall(struct.pack("!II", len(parameters) + 8, 196608) + parameters)
        authenticated = False
        for _ in range(128):
            header = read_exact(connection, 5, deadline)
            size = struct.unpack("!I", header[1:])[0]
            if not 4 <= size <= 1024 * 1024:
                raise RuntimeError("Invalid PostgreSQL message size: {}".format(size))
            body = read_exact(connection, size - 4, deadline)
            if header[:1] == b"E":
                raise RuntimeError("PostgreSQL startup error: {!r}".format(body))
            if header[:1] == b"R":
                if body != struct.pack("!I", 0):
                    raise RuntimeError("Unexpected password challenge under trust: {!r}".format(body))
                authenticated = True
            if header[:1] == b"Z":
                if not authenticated or body != b"I":
                    raise RuntimeError("Invalid authenticated ReadyForQuery state")
                elapsed_ms = (time.perf_counter() - started) * 1000
                # PostgreSQL Terminate: end each session without leaving an idle peer.
                connection.sendall(b"X" + struct.pack("!I", 4))
                return elapsed_ms
        raise RuntimeError("PostgreSQL peer never sent ReadyForQuery")


def free_port():
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as reservation:
        reservation.bind(("127.0.0.1", 0))
        return reservation.getsockname()[1]


def assert_owns_listener(child, port):
    """Check Linux socket ownership outside all measured intervals.

    Ports cannot be reserved across exec without changing the tested binary's
    interface. Therefore a successful handshake alone is insufficient evidence:
    an unrelated process could win the release-to-bind race. Match the LISTEN
    inode for exactly 127.0.0.1:port against this unreaped child's descriptors.
    """
    if child.poll() is not None:
        raise RuntimeError("owned child exited {} before listener validation".format(child.returncode))
    address = "0100007F:{:04X}".format(port)
    with Path("/proc/net/tcp").open(encoding="ascii") as sockets:
        listening_inodes = {
            fields[9]
            for fields in (line.split() for line in sockets)
            if len(fields) >= 10 and fields[1] == address and fields[3] == "0A"
        }
    owned_inodes = set()
    try:
        descriptors = list(Path("/proc/{}/fd".format(child.pid)).iterdir())
    except FileNotFoundError as error:
        raise RuntimeError("owned child disappeared before socket validation") from error
    for descriptor in descriptors:
        try:
            target = os.readlink(str(descriptor))
        except FileNotFoundError:
            # Connection descriptors can close while we inspect the server;
            # the listener itself must remain open for ownership to pass.
            continue
        if target.startswith("socket:[") and target.endswith("]"):
            owned_inodes.add(target[8:-1])
    if child.poll() is not None or not listening_inodes.intersection(owned_inodes):
        raise RuntimeError("owned child PID {} does not hold the 127.0.0.1:{} listener".format(child.pid, port))


def identity(binary):
    digest = hashlib.sha256()
    with binary.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    version = subprocess.run(
        [str(binary), "--version"], check=True, capture_output=True, text=True, timeout=10
    )
    return {"path": str(binary), "sha256": digest.hexdigest(), "version": version.stdout.strip()}


def stop_owned_child(child, timeout):
    """Signal only this Popen child; reap it before releasing its private fixture."""
    if child.poll() is not None:
        return {"forced": False, "exit_code": child.returncode}
    child.send_signal(signal.SIGTERM)
    try:
        child.wait(timeout=timeout)
        return {"forced": False, "exit_code": child.returncode}
    except subprocess.TimeoutExpired:
        child.kill()
        child.wait(timeout=5)
        return {"forced": True, "exit_code": child.returncode}


def measure(binary, arm, round_number, warmup, args, output):
    label = "{}-{:03d}-{}".format("warmup" if warmup else "sample", round_number, arm)
    log_path = output / (label + ".log")
    port = free_port()
    result = {
        "arm": arm,
        "round": round_number,
        "warmup": warmup,
        "port": port,
        "log": log_path.name,
        "load_before": list(os.getloadavg()),
    }
    with tempfile.TemporaryDirectory(prefix="nano-startup-ab-") as fixture:
        env = os.environ.copy()
        env.pop("HELIOSDB_NANO_READY_FILE", None)
        env["RUST_LOG"] = "warn"
        command = [
            str(binary), "start", "--data-dir", str(Path(fixture) / "data"),
            "--listen", "127.0.0.1", "--port", str(port), "--http-port", "0", "--auth", "trust",
        ]
        with log_path.open("xb") as log:
            started = time.perf_counter()
            child = subprocess.Popen(
                command, cwd=fixture, env=env, stdin=subprocess.DEVNULL, stdout=log, stderr=log
            )
            result["pid"] = child.pid
            try:
                deadline = started + args.startup_timeout
                last_error = None
                while time.perf_counter() < deadline:
                    if child.poll() is not None:
                        raise RuntimeError("{} exited {} before readiness; see {}".format(arm, child.returncode, log_path))
                    try:
                        connect_and_authenticate(port, min(args.connect_timeout, max(0.001, deadline - time.perf_counter())))
                        result["startup_ms"] = (time.perf_counter() - started) * 1000
                        assert_owns_listener(child, port)
                        break
                    except (OSError, TimeoutError, ConnectionError) as error:
                        last_error = error
                        time.sleep(min(args.poll_interval, max(0, deadline - time.perf_counter())))
                else:
                    raise TimeoutError("{} startup timed out: {}; see {}".format(arm, last_error, log_path))
                result["connection_ms"] = []
                for _ in range(args.connections_per_start):
                    elapsed_ms = connect_and_authenticate(port, args.connect_timeout)
                    assert_owns_listener(child, port)
                    result["connection_ms"].append(elapsed_ms)
            finally:
                result["shutdown"] = stop_owned_child(child, args.shutdown_timeout)
    if result["shutdown"]["forced"]:
        raise RuntimeError("{} required forced shutdown; see {}".format(arm, log_path))
    if result["shutdown"]["exit_code"] != 0:
        raise RuntimeError("{} shutdown exited {}; see {}".format(arm, result["shutdown"]["exit_code"], log_path))
    return result


def describe(samples):
    ordered = sorted(samples)
    return {
        "count": len(ordered),
        "median_ms": statistics.median(ordered),
        "p95_ms": ordered[max(0, math.ceil(len(ordered) * 0.95) - 1)],
        "min_ms": ordered[0],
        "max_ms": ordered[-1],
    }


def summarize(records):
    measured = [record for record in records if not record["warmup"]]
    summary = {}
    for metric in ("startup", "connection"):
        arms = {}
        for arm in ("baseline", "candidate"):
            selected = [record for record in measured if record["arm"] == arm]
            samples = ([record["startup_ms"] for record in selected] if metric == "startup" else
                       [elapsed for record in selected for elapsed in record["connection_ms"]])
            arms[arm] = describe(samples)
        arms["candidate_delta_percent"] = {
            key: (arms["candidate"][key] / arms["baseline"][key] - 1) * 100
            for key in ("median_ms", "p95_ms")
        }
        summary[metric] = arms
    return summary


def write_json(path, value):
    with path.open("w", encoding="utf-8") as handle:
        json.dump(value, handle, indent=2, sort_keys=True)
        handle.write("\n")


def terminate_harness(signum, _frame):
    # Convert the runner's termination request into stack unwinding so the
    # active measurement's finally block still reaps its exact child.
    raise SystemExit(128 + signum)


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--baseline", required=True, type=Path)
    parser.add_argument("--candidate", required=True, type=Path)
    parser.add_argument("--output-dir", required=True, type=Path)
    parser.add_argument("--repetitions", type=int, default=15)
    parser.add_argument("--warmups", type=int, default=2, help="excluded startup pairs before measured pairs")
    parser.add_argument("--connections-per-start", type=int, default=10)
    parser.add_argument("--startup-timeout", type=float, default=20)
    parser.add_argument("--connect-timeout", type=float, default=2)
    parser.add_argument("--shutdown-timeout", type=float, default=10)
    parser.add_argument("--poll-interval", type=float, default=0.005)
    args = parser.parse_args()
    signal.signal(signal.SIGTERM, terminate_harness)
    if platform.system() != "Linux" or not Path("/proc/net/tcp").is_file():
        parser.error("Linux with readable /proc/net/tcp and child /proc/PID/fd is required")
    for key in ("repetitions", "connections_per_start", "startup_timeout", "connect_timeout", "shutdown_timeout", "poll_interval"):
        if getattr(args, key) <= 0:
            parser.error("--{} must be positive".format(key.replace("_", "-")))
    if args.warmups < 0:
        parser.error("--warmups must be nonnegative")
    binaries = {"baseline": args.baseline.resolve(strict=True), "candidate": args.candidate.resolve(strict=True)}
    output = args.output_dir.resolve()
    output.mkdir(parents=True, exist_ok=True)
    if any(output.iterdir()):
        parser.error("--output-dir must be empty; existing evidence is never overwritten")
    metadata = {
        "binaries": {arm: identity(binary) for arm, binary in binaries.items()},
        "platform": platform.platform(),
        "python": platform.python_version(),
        "started_utc": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
        "settings": {key: str(value) if isinstance(value, Path) else value for key, value in vars(args).items()},
        "method": "Fresh private persistent directory each start; startup through authenticated PG ReadyForQuery; connections through ReadyForQuery; untimed child listener inode ownership validation after every handshake; alternating arm order; nearest-rank p95; warmups excluded.",
        "limitations": "Fresh-database startup, not recovery; connection samples within a startup share process state; p95 with 15 startup samples is the maximum; no automatic acceptance threshold.",
    }
    write_json(output / "metadata.json", metadata)
    records = []
    try:
        for index in range(args.warmups + args.repetitions):
            warmup = index < args.warmups
            arms = ("baseline", "candidate") if index % 2 == 0 else ("candidate", "baseline")
            for arm in arms:
                record = measure(binaries[arm], arm, index + 1, warmup, args, output)
                records.append(record)
                write_json(output / "samples.json", records)
                print("{} round {} {} startup={:.3f} ms connect_median={:.3f} ms".format(
                    "warmup" if warmup else "sample", index + 1, arm, record["startup_ms"],
                    statistics.median(record["connection_ms"])), flush=True)
    except BaseException as error:
        write_json(output / "failure.json", {"error": repr(error), "completed_samples": len(records)})
        raise
    # Detect a concurrent binary replacement: mixed-artifact timings are invalid.
    final_identities = {arm: identity(binary) for arm, binary in binaries.items()}
    if final_identities != metadata["binaries"]:
        write_json(output / "failure.json", {"error": "binary identity changed during measurements", "final_binaries": final_identities})
        raise RuntimeError("binary identity changed during measurements")
    summary = summarize(records)
    write_json(output / "summary.json", summary)
    with (output / "summary.csv").open("w", newline="", encoding="utf-8") as handle:
        writer = csv.writer(handle)
        writer.writerow(["metric", "baseline_median_ms", "candidate_median_ms", "median_delta_percent", "baseline_p95_ms", "candidate_p95_ms", "p95_delta_percent"])
        for metric, values in summary.items():
            writer.writerow([metric, values["baseline"]["median_ms"], values["candidate"]["median_ms"], values["candidate_delta_percent"]["median_ms"], values["baseline"]["p95_ms"], values["candidate"]["p95_ms"], values["candidate_delta_percent"]["p95_ms"]])
    print(json.dumps(summary, indent=2, sort_keys=True), flush=True)


if __name__ == "__main__":
    main()
