#!/usr/bin/env python3
"""Strict pg35 runner: HeliosDB-Nano (embedded, in memory) against a real PostgreSQL 18.4.

    benches/public/pg35_run.py [--build-only] <out-dir> <label>=<source-tree>[@<target-dir>] ...

`tests/pg35_benchmark.rs` refuses to run without a receipt environment: a
build receipt for the exact test binary, a profile that names it, and a
preflight that identifies the PostgreSQL reference. This script produces all
three from what it actually builds and probes, runs the benchmark, and
tabulates the receipts the harness writes. It needs python3 (standard
library), cargo and git; no psql, pgbench or Docker client.

Each variant is a source tree of this crate. Every tree must carry the same
five harness files (their hash is checked), so that two releases are measured
by the same statements and the same result checks. The first variant is the
baseline of the ratio column. Rounds alternate the order (A B, B A, ...), so
drift in the host's load falls on both sides.

The PostgreSQL reference is the one the harness is pinned to:

    docker run -d --name pg_bench_nano -e POSTGRES_USER=bench -e POSTGRES_PASSWORD=benchpass \
        -e POSTGRES_DB=benchdb -p 25433:5432 postgres:18.4-bookworm

The harness creates and drops only its own tables in `benchdb`.

Environment:
    ROUNDS         rounds; every variant runs once per round          (default 2)
    PG35_CONNSTR   keyword DSN of the reference                       (default: the documented one)
    CARGO          cargo launcher, e.g. "cargo +1.95.0"               (default "cargo")
    MAX_LOAD       a run starts only when the 1-minute load is below  (default 6)
    LOAD_WAIT      seconds to wait for that before giving up          (default 1800)

Read the table with the deployment difference in mind: Nano runs embedded and
in memory inside the test process; PostgreSQL is a server reached over
loopback TCP that flushes its WAL on every commit.
"""
import base64
import hashlib
import hmac
import json
import os
import re
import secrets
import shutil
import socket
import statistics
import struct
import subprocess
import sys
import time
import uuid
from datetime import datetime, timezone
from pathlib import Path

HARNESS = [
    "tests/pg35_benchmark.rs",
    "tests/pg35_support/mod.rs",
    "tests/pg35_support/cells.rs",
    "tests/pg35_support/model.rs",
    "tests/pg35_support/oracles.rs",
]
DEFAULT_DSN = "host=127.0.0.1 port=25433 user=bench password=benchpass dbname=benchdb"
PINNED = {"host": "127.0.0.1", "port": "25433", "user": "bench", "dbname": "benchdb"}


def fail(message):
    print(f"ERROR: {message}", file=sys.stderr)
    sys.exit(1)


def now():
    return datetime.now(timezone.utc).isoformat()


def sha256(data):
    return hashlib.sha256(data).hexdigest()


def sha256_file(path):
    digest = hashlib.sha256()
    with open(path, "rb") as handle:
        for block in iter(lambda: handle.read(1 << 20), b""):
            digest.update(block)
    return digest.hexdigest()


def write_json(path, value):
    path.write_text(json.dumps(value, indent=2, sort_keys=True) + "\n")


def output_of(command, cwd):
    return subprocess.run(command, cwd=cwd, capture_output=True, text=True, check=True).stdout


def load1():
    return os.getloadavg()[0]


# --- PostgreSQL wire client: startup, authentication, simple queries -------


class Pg:
    """Just enough of protocol 3.0 to identify the reference without a client program."""

    def __init__(self, dsn):
        self.buffer = b""
        self.sock = socket.create_connection((dsn["host"], int(dsn["port"])), timeout=30)
        parameters = f"user\0{dsn['user']}\0database\0{dsn['dbname']}\0\0".encode()
        self.sock.sendall(struct.pack("!II", 8 + len(parameters), 196608) + parameters)
        self.authenticate(dsn["user"], dsn.get("password", ""))

    def send(self, kind, body):
        self.sock.sendall(kind + struct.pack("!I", 4 + len(body)) + body)

    def read(self):
        while True:
            if len(self.buffer) >= 5:
                size = struct.unpack("!I", self.buffer[1:5])[0]
                if len(self.buffer) >= 1 + size:
                    kind, body = self.buffer[:1], self.buffer[5 : 1 + size]
                    self.buffer = self.buffer[1 + size :]
                    if kind == b"E":
                        fields = {part[:1]: part[1:].decode(errors="replace") for part in body.split(b"\0") if part}
                        raise RuntimeError(f"PostgreSQL: {fields.get(b'M', 'error')} ({fields.get(b'C', '?')})")
                    return kind, body
            chunk = self.sock.recv(65536)
            if not chunk:
                raise RuntimeError("PostgreSQL closed the connection")
            self.buffer += chunk

    def authenticate(self, user, password):
        scram = {}
        while True:
            kind, body = self.read()
            if kind == b"Z":
                return
            if kind != b"R":
                continue
            code = struct.unpack("!I", body[:4])[0]
            if code == 0:
                continue
            if code == 3:
                self.send(b"p", password.encode() + b"\0")
            elif code == 5:
                inner = hashlib.md5(password.encode() + user.encode()).hexdigest()
                outer = hashlib.md5(inner.encode() + body[4:8]).hexdigest()
                self.send(b"p", b"md5" + outer.encode() + b"\0")
            elif code == 10:
                if b"SCRAM-SHA-256" not in body[4:].split(b"\0"):
                    raise RuntimeError("the server offers no SCRAM-SHA-256")
                scram["nonce"] = base64.b64encode(secrets.token_bytes(18)).decode()
                scram["bare"] = "n=,r=" + scram["nonce"]
                first = ("n,," + scram["bare"]).encode()
                self.send(b"p", b"SCRAM-SHA-256\0" + struct.pack("!I", len(first)) + first)
            elif code == 11:
                server_first = body[4:].decode()
                attributes = dict(item.split("=", 1) for item in server_first.split(","))
                if not attributes["r"].startswith(scram["nonce"]):
                    raise RuntimeError("SCRAM: the server changed the client nonce")
                salted = hashlib.pbkdf2_hmac(
                    "sha256", password.encode(), base64.b64decode(attributes["s"]), int(attributes["i"])
                )
                without_proof = "c=biws,r=" + attributes["r"]
                message = f"{scram['bare']},{server_first},{without_proof}".encode()
                client_key = hmac.digest(salted, b"Client Key", "sha256")
                signature = hmac.digest(hashlib.sha256(client_key).digest(), message, "sha256")
                proof = bytes(a ^ b for a, b in zip(client_key, signature))
                scram["server"] = hmac.digest(hmac.digest(salted, b"Server Key", "sha256"), message, "sha256")
                self.send(b"p", f"{without_proof},p={base64.b64encode(proof).decode()}".encode())
            elif code == 12:
                attributes = dict(item.split("=", 1) for item in body[4:].decode().split(","))
                if base64.b64decode(attributes["v"]) != scram["server"]:
                    raise RuntimeError("SCRAM: the server's signature does not verify")
            else:
                raise RuntimeError(f"authentication request {code} is not supported")

    def query(self, sql):
        self.send(b"Q", sql.encode() + b"\0")
        rows = []
        while True:
            kind, body = self.read()
            if kind == b"D":
                offset, row = 2, []
                for _ in range(struct.unpack("!H", body[:2])[0]):
                    size = struct.unpack("!i", body[offset : offset + 4])[0]
                    offset += 4
                    if size < 0:
                        row.append(None)
                    else:
                        row.append(body[offset : offset + size].decode())
                        offset += size
                rows.append(row)
            elif kind == b"Z":
                return rows


def parse_dsn(text):
    if '"' in text or "'" in text:
        fail("PG35_CONNSTR must be a plain keyword DSN without quotes")
    dsn = dict(item.split("=", 1) for item in text.split())
    for key, value in PINNED.items():
        if dsn.get(key) != value:
            fail(f"the harness accepts only {key}={value} (PG35_CONNSTR has {key}={dsn.get(key)})")
    return dsn


def probe_reference(dsn, setting_names):
    """The same identity the harness probes before and after its run (tests/pg35_support/mod.rs)."""
    pg = Pg(dsn)
    number, version, database, address, port, started = pg.query(
        "SELECT current_setting('server_version_num')::int,version(),current_database(),"
        "inet_server_addr()::text,inet_server_port(),"
        "to_char(pg_postmaster_start_time() AT TIME ZONE 'UTC','YYYY-MM-DD\"T\"HH24:MI:SS.US\"Z\"')"
    )[0]
    system = pg.query("SELECT system_identifier::text FROM pg_control_system()")[0][0]
    wanted = ",".join(f"'{name}'" for name in setting_names)
    settings = dict(
        pg.query(f"SELECT name::text,setting::text FROM pg_settings WHERE name IN({wanted}) ORDER BY name")
    )
    if len(settings) != len(setting_names):
        fail("the reference does not report every setting the harness records")
    endpoint = f"{dsn['host']}:{dsn['port']}"
    inputs = {
        "database": database,
        "endpoint": endpoint,
        "postmaster_started_at": started,
        "server_address": address,
        "server_port": port,
        "system_identifier": system,
        "version": version,
        "version_num": number,
    }
    # serde_json's compact form of a BTreeMap<String, String>: sorted keys, no spaces.
    canonical = json.dumps(inputs, sort_keys=True, separators=(",", ":"), ensure_ascii=False)
    return {
        "server_name": "pg_bench_nano",
        "version_num": int(number),
        "version": version,
        "database": database,
        "endpoint": endpoint,
        "identity_inputs": inputs,
        "instance_id": sha256(canonical.encode()),
        "settings": settings,
    }


# --- Source and build ------------------------------------------------------


def harness_hash(tree):
    """The framing `harness_source_hash` compiles into the test binary."""
    digest = hashlib.sha256()
    for name in HARNESS:
        path = tree / name
        if not path.is_file():
            fail(f"{tree} has no {name}")
        digest.update(name.encode() + b"\0" + path.read_bytes() + b"\0")
    return digest.hexdigest()


def harness_settings(tree):
    source = (tree / "tests/pg35_support/mod.rs").read_text()
    block = re.search(r"const PG_SETTINGS: \[&str; (\d+)\] = \[(.*?)\];", source, re.S)
    names = re.findall(r'"([a-z_]+)"', block.group(2)) if block else []
    if not block or len(names) != int(block.group(1)):
        fail("cannot read PG_SETTINGS from tests/pg35_support/mod.rs")
    return names


def source_manifest(tree):
    """Path -> SHA-256 of every file git tracks or would track (every file outside target/ when not a git tree)."""
    if (tree / ".git").exists():
        names = output_of(["git", "ls-files", "-z", "--cached", "--others", "--exclude-standard"], tree).split("\0")
    else:
        names = [
            str(path.relative_to(tree))
            for path in tree.rglob("*")
            if path.is_file() and "target" not in path.relative_to(tree).parts
        ]
    return {name: sha256_file(tree / name) for name in sorted(names) if name and (tree / name).is_file()}


def has_statement_offload(tree):
    """Whether the product source exposes the switch the harness pins on; older releases do not."""
    module = tree / "src/runtime_offload.rs"
    return module.is_file() and all(
        marker in module.read_text() for marker in ("pub fn set_statement_offload", "pub fn statement_offload")
    )


def build(out, label, tree, target_dir, cargo):
    directory = out / "build" / label
    profile_path = directory / "profile.json"
    manifest = source_manifest(tree)
    manifest_hash = sha256(json.dumps(manifest, sort_keys=True).encode())
    if profile_path.is_file():
        profile = json.loads(profile_path.read_text())
        binary = Path(profile["pg35_binary"])
        if (
            profile["source_manifest_sha256"] == manifest_hash
            and binary.is_file()
            and sha256_file(binary) == profile["pg35_binary_sha256"]
        ):
            print(f"[{label}] reusing the build of this exact source: {binary}")
            return profile
        fail(f"{directory} holds a build of different source; use a new out-dir")
    directory.mkdir(parents=True)

    toolchain = [argument[1:] for argument in cargo[1:] if argument.startswith("+")]
    rustc = output_of(["rustc", *(f"+{name}" for name in toolchain), "-Vv"], tree)
    cargo_version = output_of([*cargo, "-Vv"], tree)
    host = re.search(r"^host: (\S+)$", rustc, re.M).group(1)
    offload = has_statement_offload(tree)
    test_arguments = ["--cfg", "pg35_has_statement_offload"] if offload else []
    command = [*cargo, "rustc", "--profile", "release", "--locked", "--test", "pg35_benchmark"]
    command += ["--message-format=json", "--", *test_arguments]
    environment = dict(os.environ)
    if target_dir:
        environment["CARGO_TARGET_DIR"] = str(target_dir)
    recorded_environment = {
        name: value
        for name, value in sorted(environment.items())
        if name == "RUSTFLAGS" or name.startswith("CARGO_PROFILE_RELEASE_") or name.startswith("CARGO_BUILD_")
    }
    recorded_environment.setdefault("RUSTFLAGS", "")

    print(f"[{label}] building the pg35 harness in {tree} ({' '.join(command)})")
    started = now()
    with open(directory / "build.stdout", "wb") as stdout, open(directory / "build.stderr", "wb") as stderr:
        code = subprocess.run(command, cwd=tree, env=environment, stdout=stdout, stderr=stderr).returncode
    if code != 0:
        fail(f"the build of '{label}' failed ({code}); see {directory / 'build.stderr'}")
    artifact = None
    for line in (directory / "build.stdout").read_text().splitlines():
        message = json.loads(line) if line.startswith("{") else {}
        target = message.get("target", {})
        if message.get("reason") == "compiler-artifact" and target.get("name") == "pg35_benchmark":
            if "test" in target.get("kind", []) and message.get("executable"):
                artifact = message
    if artifact is None:
        fail(f"cargo reported no pg35_benchmark test executable for '{label}'")
    if source_manifest(tree) != manifest:
        fail(f"the source of '{label}' changed during its build")
    binary = directory / "pg35_benchmark"
    shutil.copy2(artifact["executable"], binary)
    binary_hash = sha256_file(binary)
    write_json(directory / "source-manifest.json", manifest)

    git = (tree / ".git").exists()
    common = {
        "build_profile": "release",
        "cargo": cargo_version,
        "commit": output_of(["git", "rev-parse", "HEAD"], tree).strip() if git else "not a git tree",
        "features": artifact["features"],
        "pg35_offload_available": offload,
        "rustc": rustc,
        "source_manifest_sha256": manifest_hash,
        "target": host,
    }
    receipt_path = directory / "build-receipt.json"
    write_json(
        receipt_path,
        {
            **common,
            "schema_version": 1,
            "evidence_kind": "build",
            "exit_code": code,
            "command": command,
            "cwd": str(tree),
            "started_at": started,
            "finished_at": now(),
            "target_dir": str(target_dir) if target_dir else "default",
            "compiler_artifact": artifact,
            "binary": str(binary),
            "binary_sha256": binary_hash,
            "harness_source_sha256": harness_hash(tree),
            "source_manifest": str(directory / "source-manifest.json"),
            "uncommitted_changes": output_of(["git", "status", "--porcelain"], tree).splitlines() if git else None,
            "build_environment": recorded_environment,
            "test_rustc_args": test_arguments,
        },
    )
    profile = {
        **common,
        "label": label,
        "source_tree": str(tree),
        "pg35_binary": str(binary),
        "pg35_binary_sha256": binary_hash,
        "pg35_source_sha256": harness_hash(tree),
        "pg35_build_receipt": str(receipt_path),
        "pg35_build_receipt_sha256": sha256_file(receipt_path),
        "pg35_build_environment": recorded_environment,
    }
    write_json(profile_path, profile)
    return profile


# --- Runs and summary ------------------------------------------------------


def wait_for_load(limit, patience):
    deadline = time.monotonic() + patience
    while load1() >= limit:
        if time.monotonic() > deadline:
            fail(f"the 1-minute load stayed at or above {limit} for {patience} s; a run under load is noise")
        time.sleep(10)
    return load1()


def run_arm(out, profile, round_number, preflight, dsn_text, limit, patience):
    job = re.sub(r"[^A-Za-z0-9_-]", "_", f"{profile['label']}-r{round_number}")
    directory = out / "arms" / job
    if directory.exists():
        fail(f"{directory} exists; a receipt is never overwritten, use a new out-dir")
    directory.mkdir(parents=True)
    load_before = wait_for_load(limit, patience)
    receipt = directory / "strict-receipt.json"
    environment = dict(
        os.environ,
        PG35_STRICT_JSON=str(receipt),
        PG35_RUN_ID=uuid.uuid4().hex,
        PG35_JOB_ID=job,
        PG35_PROFILE_JSON=str(out / "build" / profile["label"] / "profile.json"),
        PG35_PREFLIGHT_JSON=str(preflight),
        PG35_CONNSTR=dsn_text,
        PG35_ITERS="300",
        HELIOSDB_NANO_STATEMENT_OFFLOAD="on",
    )
    environment.pop("PG35_PG_LABEL", None)
    command = [profile["pg35_binary"], "--exact", "pg35_benchmark", "--ignored", "--nocapture"]
    print(f"[{job}] running (load {load_before:.2f})", flush=True)
    started = now()
    with open(directory / "pg35.log", "wb") as log:
        code = subprocess.run(command, cwd=directory, env=environment, stdout=log, stderr=subprocess.STDOUT).returncode
    log_text = (directory / "pg35.log").read_text(errors="replace")
    if code != 0 or "test result: ok. 1 passed" not in log_text or not receipt.is_file():
        fail(f"the run '{job}' did not complete ({code}); see {directory / 'pg35.log'}")
    arm = {
        "job": job,
        "label": profile["label"],
        "round": round_number,
        "command": command,
        "started_at": started,
        "ended_at": now(),
        "load1_before": round(load_before, 2),
        "load1_after": round(load1(), 2),
        "receipt": str(receipt),
        "receipt_sha256": sha256_file(receipt),
    }
    write_json(directory / "arm.json", arm)
    return arm


def microseconds(value):
    if value >= 1000:
        return f"{value / 1000:.2f} ms"
    return f"{value:.1f} us" if value >= 100 else f"{value:.2f} us"


def summarize(out, profiles, arms, reference):
    labels = [profile["label"] for profile in profiles]
    baseline = labels[0]
    nano = {label: {} for label in labels}
    postgres, order = {}, []
    for arm in arms:
        for category in json.loads(Path(arm["receipt"]).read_text())["categories"]:
            name = category["category"]
            if name not in postgres:
                order.append(name)
            nano[arm["label"]].setdefault(name, []).append(
                category["nano_elapsed_ns"] / category["nano_successes"] / 1000
            )
            postgres.setdefault(name, []).append(
                category["postgres_elapsed_ns"] / category["postgres_successes"] / 1000
            )
    rows = []
    for name in order:
        row = {"category": name, "postgres_us": statistics.median(postgres[name]), "nano_us": {}, "nano_runs_us": {}}
        for label in labels:
            row["nano_us"][label] = statistics.median(nano[label][name])
            row["nano_runs_us"][label] = nano[label][name]
        rows.append(row)
    write_json(out / "results.json", {"reference": reference, "variants": profiles, "arms": arms, "categories": rows})

    rounds = max(arm["round"] for arm in arms)
    settings = reference["settings"]
    lines = [
        f"pg35: {len(rows)} categories, 300 timed iterations each (after 2 warm-ups), median of {rounds} round(s).",
        "",
        "Nano runs embedded and in memory inside the test process. PostgreSQL is "
        f"`{reference['version'].split(' on ')[0]}`, a server reached over loopback TCP with "
        f"`fsync={settings['fsync']}`, `synchronous_commit={settings['synchronous_commit']}`. Times are per statement; "
        "`x PG` is PostgreSQL's time divided by Nano's.",
        "",
    ]
    header = "| category | PostgreSQL |"
    rule = "|---|---:|"
    for label in labels:
        header += f" {label} | x PG |"
        rule += "---:|---:|"
    for label in labels[1:]:
        header += f" {label} / {baseline} (time) |"
        rule += "---:|"
    lines += [header, rule]
    for row in rows:
        line = f"| {row['category']} | {microseconds(row['postgres_us'])} |"
        for label in labels:
            value = row["nano_us"][label]
            line += f" {microseconds(value)} | {row['postgres_us'] / value:,.1f} |"
        for label in labels[1:]:
            line += f" {row['nano_us'][label] / row['nano_us'][baseline]:.2f} |"
        lines.append(line)
    lines.append("")
    for label in labels:
        wins = sum(1 for row in rows if row["nano_us"][label] < row["postgres_us"])
        lines.append(f"- `{label}`: faster than PostgreSQL in {wins} of {len(rows)} categories.")
    lines.append("")
    lines.append("| run | load before | load after |")
    lines.append("|---|---:|---:|")
    for arm in arms:
        lines.append(f"| {arm['job']} | {arm['load1_before']} | {arm['load1_after']} |")
    text = "\n".join(lines) + "\n"
    (out / "summary.md").write_text(text)
    print(text)


def main():
    arguments = sys.argv[1:]
    build_only = "--build-only" in arguments
    arguments = [argument for argument in arguments if argument != "--build-only"]
    if len(arguments) < 2:
        print(__doc__, file=sys.stderr)
        sys.exit(2)
    out = Path(arguments[0]).resolve()
    out.mkdir(parents=True, exist_ok=True)
    cargo = os.environ.get("CARGO", "cargo").split()
    rounds = int(os.environ.get("ROUNDS", "2"))
    limit = float(os.environ.get("MAX_LOAD", "6"))
    patience = float(os.environ.get("LOAD_WAIT", "1800"))
    dsn_text = os.environ.get("PG35_CONNSTR", DEFAULT_DSN)
    dsn = parse_dsn(dsn_text)

    variants = []
    for argument in arguments[1:]:
        label, _, rest = argument.partition("=")
        tree, _, target_dir = rest.partition("@")
        if not label or not tree or any(character in label for character in "/ \t"):
            fail(f"'{argument}' is not <label>=<source-tree>[@<target-dir>]")
        variants.append((label, Path(tree).resolve(), Path(target_dir).resolve() if target_dir else None))
    if len({label for label, _, _ in variants}) != len(variants):
        fail("variant labels must differ")
    hashes = {label: harness_hash(tree) for label, tree, _ in variants}
    if len(set(hashes.values())) != 1:
        fail(f"the variants do not carry the same harness source: {hashes}")

    profiles = [build(out, label, tree, target_dir, cargo) for label, tree, target_dir in variants]
    if build_only:
        return

    try:
        reference = probe_reference(dsn, harness_settings(variants[0][1]))
    except (OSError, RuntimeError) as error:
        fail(f"cannot identify the PostgreSQL reference at {dsn['host']}:{dsn['port']}: {error}\n{__doc__}")
    if reference["version_num"] != 180004:
        fail(f"the harness requires PostgreSQL 18.4; the reference reports {reference['version']}")
    preflight = out / "postgres-preflight.json"
    write_json(
        preflight,
        {
            "schema_version": 1,
            "evidence_kind": "executed_reference_preflight",
            "captured_at": now(),
            "producer": "benches/public/pg35_run.py",
            "producer_sha256": sha256_file(Path(__file__).resolve()),
            "method": "read-only SQL over the wire; the container, if any, was not inspected",
            "reference": reference,
        },
    )

    arms = []
    for round_number in range(1, rounds + 1):
        order = profiles if round_number % 2 else list(reversed(profiles))
        for profile in order:
            arms.append(run_arm(out, profile, round_number, preflight, dsn_text, limit, patience))
    if probe_reference(dsn, harness_settings(variants[0][1])) != reference:
        fail("the PostgreSQL reference changed during the runs")
    summarize(out, profiles, arms, reference)


if __name__ == "__main__":
    main()
