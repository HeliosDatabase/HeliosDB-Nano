#!/usr/bin/env python3
"""Shared tooling for the public benchmark scripts (benches/public/*.sh).

Stdlib only (argparse/json/re/subprocess) — no pip installs required.

Subcommands
-----------
parse      Parse TPS-harness / sqlite-mirror text output into a workloads JSON.
           Reads stdin or file args. If the same workload label appears more
           than once (e.g. best-of-3 runs concatenated), the entry with the
           highest ops_per_sec wins and `runs_seen` records how many samples
           were folded.

assemble   Combine one or more parsed-workloads JSON files into a single
           results JSON stamped with commit / host / timestamp.

check      Compare a parsed-workloads (or assembled, via --run) JSON against a
           baseline JSON; exit 1 if any baseline workload is slower than
           baseline/threshold or missing.

baseline   Generate a baseline JSON from a parsed-workloads JSON, scaling each
           ops_per_sec down by --scale (conservative headroom for slower
           hardware, e.g. CI runners).

compare    Print a markdown comparison table between two named runs inside an
           assembled results JSON.

Recognised line shapes (see tests/tps_workloads.rs and
benches/external/sqlite_tps_mirror.py):
  label   <ops> ops  <secs> s  <rate> ops/s  <us> us/op [p50=..us p95=..us p99=..us]
  label   <rate> txn/s | commit/s | lookups/s | ops/s        (rate-only benches)
"""

import argparse
import json
import os
import re
import subprocess
import sys
from datetime import datetime, timezone

STD_RE = re.compile(
    r"^(\S+)\s+(\d+) ops\s+([\d.]+) s\s+([\d.]+) ops/s\s+([\d.]+) us/op"
    r"(?:\s+p50=([\d.]+)us p95=([\d.]+)us p99=([\d.]+)us)?\s*$"
)
RATE_RE = re.compile(r"^\s*(.+?)\s{2,}([\d.]+)\s+(ops|txn|commit|lookups)/s\b")


def parse_lines(lines):
    workloads = {}

    def fold(label, entry):
        prev = workloads.get(label)
        if prev is None:
            entry["runs_seen"] = 1
            workloads[label] = entry
        else:
            entry["runs_seen"] = prev["runs_seen"] + 1
            if entry["ops_per_sec"] >= prev["ops_per_sec"]:
                workloads[label] = entry
            else:
                prev["runs_seen"] = entry["runs_seen"]

    for line in lines:
        m = STD_RE.match(line.rstrip())
        if m:
            entry = {
                "ops": int(m.group(2)),
                "secs": float(m.group(3)),
                "ops_per_sec": float(m.group(4)),
                "us_per_op": float(m.group(5)),
                "unit": "ops/s",
            }
            if m.group(6) is not None:
                entry["p50_us"] = float(m.group(6))
                entry["p95_us"] = float(m.group(7))
                entry["p99_us"] = float(m.group(8))
            fold(m.group(1), entry)
            continue
        m = RATE_RE.match(line.rstrip())
        if m:
            label = re.sub(r"\s+", " ", m.group(1).strip())
            fold(label, {"ops_per_sec": float(m.group(2)), "unit": m.group(3) + "/s"})
    return workloads


def cmd_parse(args):
    lines = []
    if args.files:
        for path in args.files:
            with open(path) as fh:
                lines.extend(fh.readlines())
    else:
        lines = sys.stdin.readlines()
    json.dump({"workloads": parse_lines(lines)}, sys.stdout, indent=2)
    print()


def _run(cmd):
    try:
        return subprocess.run(
            cmd, capture_output=True, text=True, timeout=10
        ).stdout.strip()
    except Exception:
        return "unknown"


def host_stamp():
    cpu = "unknown"
    try:
        with open("/proc/cpuinfo") as fh:
            for line in fh:
                if line.startswith("model name"):
                    cpu = line.split(":", 1)[1].strip()
                    break
    except OSError:
        pass
    mem_gb = None
    try:
        with open("/proc/meminfo") as fh:
            for line in fh:
                if line.startswith("MemTotal"):
                    mem_gb = round(int(line.split()[1]) / 1048576, 1)
                    break
    except OSError:
        pass
    return {
        "cpu": cpu,
        "logical_cores": os.cpu_count(),
        "mem_total_gb": mem_gb,
        "kernel": _run(["uname", "-sr"]),
        "rustc": _run(["rustc", "--version"]),
        "python": sys.version.split()[0],
    }


def git_stamp():
    commit = _run(["git", "rev-parse", "--short", "HEAD"])
    dirty = bool(_run(["git", "status", "--porcelain", "--untracked-files=no"]))
    return commit + ("-dirty" if dirty else "")


def cmd_assemble(args):
    runs = {}
    for spec in args.runs:
        name, _, path = spec.partition("=")
        if not path:
            sys.exit(f"assemble: bad run spec {spec!r}, expected name=parsed.json")
        with open(path) as fh:
            runs[name] = json.load(fh)
    params = {}
    for spec in args.param or []:
        k, _, v = spec.partition("=")
        params[k] = v
    out = {
        "script": args.script,
        "commit": git_stamp(),
        "timestamp_utc": datetime.now(timezone.utc).isoformat(timespec="seconds"),
        "host": host_stamp(),
        "params": params,
        "runs": runs,
    }
    with open(args.out, "w") as fh:
        json.dump(out, fh, indent=2)
        fh.write("\n")
    print(f"wrote {args.out}")


def load_workloads(path, run=None):
    with open(path) as fh:
        data = json.load(fh)
    if run is not None:
        data = data["runs"][run]
    return data["workloads"]


def cmd_check(args):
    with open(args.baseline) as fh:
        baseline = json.load(fh)
    measured = load_workloads(args.results, args.run)
    threshold = args.threshold or baseline.get("threshold_default", 2.5)
    print(f"perf gate: threshold = fail if measured < baseline / {threshold}")
    print(f"baseline:  {args.baseline} (generated {baseline.get('generated_at', '?')} "
          f"on commit {baseline.get('commit', '?')})")
    print(f"{'workload':<28} {'baseline/s':>14} {'measured/s':>14} {'ratio':>7}  status")
    print("-" * 78)
    failures = []
    for label, base in sorted(baseline["workloads"].items()):
        base_rate = base["ops_per_sec"]
        got = measured.get(label)
        if got is None:
            failures.append(label)
            print(f"{label:<28} {base_rate:>14.0f} {'MISSING':>14} {'-':>7}  FAIL")
            continue
        rate = got["ops_per_sec"]
        ratio = rate / base_rate if base_rate else float("inf")
        ok = rate >= base_rate / threshold
        if not ok:
            failures.append(label)
        print(f"{label:<28} {base_rate:>14.0f} {rate:>14.0f} {ratio:>6.2f}x  "
              f"{'ok' if ok else 'FAIL'}")
    print("-" * 78)
    if failures:
        print(f"PERF GATE FAILED: {len(failures)} workload(s) more than {threshold}x "
              f"slower than baseline (or missing): {', '.join(failures)}")
        print("If hardware changed legitimately, regenerate the baseline: "
              "benches/public/regen_ci_baseline.sh (see benches/public/README.md).")
        sys.exit(1)
    print("perf gate passed.")


def cmd_baseline(args):
    measured = load_workloads(args.results, args.run)
    workloads = {}
    for label, entry in sorted(measured.items()):
        if args.only and label not in args.only:
            continue
        workloads[label] = {
            "ops_per_sec": round(entry["ops_per_sec"] * args.scale, 1),
            "measured_ops_per_sec": round(entry["ops_per_sec"], 1),
            "unit": entry.get("unit", "ops/s"),
        }
    out = {
        "note": args.note,
        "generated_at": datetime.now(timezone.utc).isoformat(timespec="seconds"),
        "commit": git_stamp(),
        "host_cpu": host_stamp()["cpu"],
        "scale_applied": args.scale,
        "threshold_default": args.threshold,
        "workloads": workloads,
    }
    with open(args.out, "w") as fh:
        json.dump(out, fh, indent=2)
        fh.write("\n")
    print(f"wrote {args.out} ({len(workloads)} workloads, scale={args.scale})")


def cmd_compare(args):
    with open(args.results) as fh:
        data = json.load(fh)
    left = data["runs"][args.left]["workloads"]
    right = data["runs"][args.right]["workloads"]
    ln, rn = args.left_name or args.left, args.right_name or args.right
    print(f"| workload | {ln} (ops/s) | {rn} (ops/s) | {ln} / {rn} |")
    print("|---|---:|---:|---:|")
    for label in [k for k in left if k in right]:
        lr, rr = left[label]["ops_per_sec"], right[label]["ops_per_sec"]
        ratio = lr / rr if rr else float("inf")
        mark = f"**{ratio:.2f}x**" if ratio >= 1.0 else f"{ratio:.2f}x"
        print(f"| {label} | {lr:,.0f} | {rr:,.0f} | {mark} |")


def main():
    p = argparse.ArgumentParser(description=__doc__,
                                formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = p.add_subparsers(dest="cmd", required=True)

    sp = sub.add_parser("parse", help="parse bench output text into workloads JSON")
    sp.add_argument("files", nargs="*", help="input files (default: stdin)")
    sp.set_defaults(func=cmd_parse)

    sp = sub.add_parser("assemble", help="combine parsed runs into stamped results JSON")
    sp.add_argument("--script", required=True)
    sp.add_argument("--out", required=True)
    sp.add_argument("--param", action="append", metavar="K=V")
    sp.add_argument("runs", nargs="+", metavar="name=parsed.json")
    sp.set_defaults(func=cmd_assemble)

    sp = sub.add_parser("check", help="gate measured results against a baseline")
    sp.add_argument("--baseline", required=True)
    sp.add_argument("--results", required=True)
    sp.add_argument("--run", help="run name when --results is an assembled file")
    sp.add_argument("--threshold", type=float, default=None,
                    help="override baseline threshold_default")
    sp.set_defaults(func=cmd_check)

    sp = sub.add_parser("baseline", help="generate baseline JSON from measured results")
    sp.add_argument("--results", required=True)
    sp.add_argument("--run", help="run name when --results is an assembled file")
    sp.add_argument("--out", required=True)
    sp.add_argument("--scale", type=float, default=0.5,
                    help="multiply measured rates by this factor (default 0.5)")
    sp.add_argument("--threshold", type=float, default=2.5)
    sp.add_argument("--note", default="")
    sp.add_argument("--only", nargs="*", help="restrict to these workload labels")
    sp.set_defaults(func=cmd_baseline)

    sp = sub.add_parser("compare", help="markdown comparison table of two runs")
    sp.add_argument("--results", required=True)
    sp.add_argument("--left", required=True)
    sp.add_argument("--right", required=True)
    sp.add_argument("--left-name")
    sp.add_argument("--right-name")
    sp.set_defaults(func=cmd_compare)

    args = p.parse_args()
    args.func(args)


if __name__ == "__main__":
    main()
