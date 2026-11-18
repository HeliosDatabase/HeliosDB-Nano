#!/usr/bin/env python3
"""Export a release benchmark run as one machine-readable JSON document.

    benches/public/export_results.py [--platform TEXT] [--note TEXT] <release-bench-out-dir> [<bench-engines-dir>] > summary.json

<release-bench-out-dir> is what `benches/public/release_bench.sh` wrote (its `pg35/`,
`wire/` and `RESULTS.md`); <bench-engines-dir> holds the `run-*.txt` outputs of
`docs/benchmarks/bench-engines.sh` and, optionally, its `provenance.txt`. `--platform`
states the hardware class of a run whose report predates the `platform` row;
`--note` adds one sentence to the document.

The document carries every number the run produced, with what identifies it:
the commits and sha256 of the binaries measured, the toolchain and the PostgreSQL
reference's version, settings and identity hash. It carries no file paths and
nothing that identifies the machine (no host name, CPU model or kernel build;
only the hardware class: architecture, CPU count, memory class), so it can be published next to the prose report
(`docs/benchmarks/results/`). Standard library only.
"""
import json
import re
import statistics
import sys
from pathlib import Path

SCHEMA = "heliosdb-nano-release-benchmark/1"


def results_table(results_md: str) -> dict:
    """The provenance table at the top of RESULTS.md: `| key | value |` rows."""
    table = {}
    for line in results_md.splitlines():
        if line.startswith("## "):
            break
        m = re.match(r"\|\s*(.+?)\s*\|\s*(.+?)\s*\|$", line)
        if m and m.group(1).strip() and not set(m.group(1)) <= {"-", " "}:
            key = m.group(1).strip("`")
            if key == "host":
                # Reports of older runs named the machine (kernel build, CPU model).
                # The published copy carries the hardware class only: architecture
                # and CPU count, which is what the client counts are read against.
                cpus = re.search(r"(\d+) CPUs", m.group(2))
                arch = re.search(r"\b(x86_64|aarch64|arm64)\b", m.group(2))
                if cpus and arch:
                    table["platform"] = f"Linux {arch.group(1)}, {cpus.group(1)} logical CPUs"
                continue
            table[key] = m.group(2).replace("`", "")
    return table


def strip_paths(value):
    if isinstance(value, str):
        return re.sub(r"(/[^\s/]+)+/", "", value)
    return value


def pg35(out: Path) -> dict:
    raw = json.loads((out / "pg35" / "results.json").read_text())
    reference = raw["reference"]
    variants = []
    for variant in raw["variants"]:
        variants.append({
            "label": variant["label"],
            "commit": variant.get("commit"),
            "build_profile": variant.get("build_profile"),
            "features": variant.get("features"),
            "harness_source_sha256": variant.get("pg35_source_sha256"),
            "harness_binary_sha256": variant.get("pg35_binary_sha256"),
            "rustc": (variant.get("rustc") or "").splitlines()[0] if variant.get("rustc") else None,
        })
    labels = [v["label"] for v in variants]
    categories = []
    for row in raw["categories"]:
        entry = {"category": row["category"], "postgres_us": round(row["postgres_us"], 3), "nano": {}}
        for label in labels:
            us = row["nano_us"].get(label)
            runs = row.get("nano_runs_us", {}).get(label, [])
            entry["nano"][label] = {
                "median_us": round(us, 3) if us is not None else None,
                "runs_us": [round(r, 3) for r in runs],
                "x_postgres": round(row["postgres_us"] / us, 2) if us else None,
            }
        if len(labels) >= 2 and all(entry["nano"][l]["median_us"] for l in labels[:2]):
            entry["ratio_time_" + labels[1] + "_vs_" + labels[0]] = round(
                entry["nano"][labels[1]]["median_us"] / entry["nano"][labels[0]]["median_us"], 3)
        categories.append(entry)
    arms = [{
        "job": arm["job"], "label": arm["label"], "round": arm.get("round"),
        "started_at": arm.get("started_at"), "ended_at": arm.get("ended_at"),
        "load1_before": arm.get("load1_before"), "load1_after": arm.get("load1_after"),
        "strict_receipt_sha256": arm.get("receipt_sha256"),
    } for arm in raw["arms"]]
    return {
        "description": "35 SQL categories; Nano embedded and in memory, PostgreSQL over loopback TCP; "
                       "300 timed iterations per category after 2 warm-ups, every result checked; "
                       "median over rounds; times per statement in microseconds",
        "postgres": {
            "version": reference.get("version"),
            "instance_id_sha256": reference.get("instance_id"),
            "settings": reference.get("settings"),
        },
        "variants": variants,
        "categories": categories,
        "runs": arms,
    }


def wire(out: Path) -> dict:
    rows = [json.loads(line) for line in (out / "wire" / "results.jsonl").read_text().splitlines() if line.strip()]
    labels = list(dict.fromkeys(r["label"] for r in rows))
    cells = {}
    for r in rows:
        cells.setdefault((r["mode"], r["protocol"], r["clients"]), {}).setdefault(r["label"], []).append(r)
    out_cells = []
    for (mode, protocol, clients), by_label in sorted(cells.items()):
        entry = {"mode": mode, "protocol": protocol, "clients": clients, "variants": {}}
        for label in labels:
            runs = by_label.get(label, [])
            if not runs:
                continue
            entry["variants"][label] = {
                "ops_per_sec_median": round(statistics.median(r["ops_per_sec"] for r in runs), 1),
                "ops_per_sec_runs": [r["ops_per_sec"] for r in runs],
                "p50_us_median": statistics.median(r["p50_us"] for r in runs),
                "p99_us_median": statistics.median(r["p99_us"] for r in runs),
                "errors": sum(r["errors"] for r in runs),
                "load1_before": [r["load_before"] for r in runs],
            }
        if len(labels) >= 2 and all(l in entry["variants"] for l in labels[:2]):
            entry["ratio_ops_" + labels[1] + "_vs_" + labels[0]] = round(
                entry["variants"][labels[1]]["ops_per_sec_median"] / entry["variants"][labels[0]]["ops_per_sec_median"], 3)
        out_cells.append(entry)
    provenance = (out / "wire" / "provenance.txt").read_text() if (out / "wire" / "provenance.txt").exists() else ""
    binaries = {m.group(1): m.group(2) for m in re.finditer(r"variant (\S+): \S+ sha256=([0-9a-f]{64})", provenance)}
    client = re.search(r"client=\S+ sha256=([0-9a-f]{64})", provenance)
    settings = re.search(r"seconds=(\d+) warmup=(\d+) rows=(\d+)", provenance)
    return {
        "description": "one-row UPDATE by primary key over the PostgreSQL wire against a running server; "
                       "'extended' is a prepared statement bound per execution, 'simple' the statement as text; "
                       "each client has its own connection and its own rows; client and server on one host",
        "server_sha256": binaries,
        "client_sha256": client.group(1) if client else None,
        "seconds_per_run": int(settings.group(1)) if settings else None,
        "warmup_seconds": int(settings.group(2)) if settings else None,
        "rows": int(settings.group(3)) if settings else None,
        "cells": out_cells,
    }


def pgbench(engines: Path) -> dict:
    runs = []
    for path in sorted(engines.glob("run-*.txt")):
        data, engine, test = [], None, None
        for line in path.read_text().splitlines():
            m = re.match(r"### (.+?) \(", line)
            if m:
                engine = m.group(1)
                continue
            m = re.match(r"  (\S.*?):$", line)
            if m:
                test = m.group(1)
                continue
            m = re.match(r"  c=(\d+)\s+tps=([\d.]+)\s+lat=([\d.]+)ms", line)
            if m and engine:
                data.append({"engine": engine, "test": test, "clients": int(m.group(1)),
                             "tps": float(m.group(2)), "latency_ms": float(m.group(3))})
                continue
            m = re.match(r"    (\S+) -> (\d+)", line)
            if m and engine:
                data.append({"engine": engine, "test": test, "size": m.group(1), "ms": int(m.group(2))})
        runs.append({"run": path.stem, "cells": data})
    provenance = (engines / "provenance.txt").read_text() if (engines / "provenance.txt").exists() else ""
    image = re.search(r"client image (\S+)", provenance)
    script = re.search(r"script sha256 ([0-9a-f]{64})", provenance)
    return {
        "description": "pgbench and psql from the postgres:18.4-bookworm image against PostgreSQL and each Nano server; "
                       "8 s per cell; runs listed in the order made (the Nano servers alternate)",
        "client_image": image.group(1) if image else None,
        "script_sha256": script.group(1) if script else None,
        "runs": runs,
    }


def main() -> None:
    platform = None
    if "--platform" in sys.argv:
        at = sys.argv.index("--platform")
        platform = sys.argv[at + 1]
        del sys.argv[at:at + 2]
    note = None
    if "--note" in sys.argv:
        at = sys.argv.index("--note")
        note = sys.argv[at + 1]
        del sys.argv[at:at + 2]
    if len(sys.argv) < 2:
        sys.exit(__doc__)
    out = Path(sys.argv[1])
    results_md = (out / "RESULTS.md").read_text()
    table = results_table(results_md)
    document = {
        "schema": SCHEMA,
        "produced_by": "benches/public/release_bench.sh",
        **({"note": note} if note else {}),
        "run": {key: strip_paths(value) for key, value in {**table, **({"platform": platform} if platform else {})}.items()},
        "pg35": pg35(out) if (out / "pg35" / "results.json").exists() else None,
        "wire": wire(out) if (out / "wire" / "results.jsonl").exists() else None,
        "pgbench": pgbench(Path(sys.argv[2])) if len(sys.argv) > 2 else None,
    }
    json.dump(document, sys.stdout, indent=1, sort_keys=False)
    sys.stdout.write("\n")


if __name__ == "__main__":
    main()
