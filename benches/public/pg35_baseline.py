#!/usr/bin/env python3
"""The pg35 baseline: the best time each category has reached in a release.

    benches/public/pg35_baseline.py update [--platform TEXT] <baseline.json> <release>=<variant>@<results.json> ...
    benches/public/pg35_baseline.py check  [--fail-over RATIO] <baseline.json> <variant>@<results.json> ...
    benches/public/pg35_baseline.py table  <baseline.json>

The rule: the baseline of a category is the best (lowest) time per statement any
release has reached in it. A release that reaches a new best sets the baseline;
nothing else moves it. A slower release leaves the baseline with the release
that set it, so a regression stays visible until some later release beats the
old time.

"Time" is what `pg35_run.py` records for one round of one variant: the mean time
per statement over the timed iterations of a category. The best round counts. A
minimum is used and not a median because noise on a busy host only ever adds
time; the fastest round is the one least disturbed.

`update` reads the `results.json` files `pg35_run.py` writes. `<release>` is the
name the baseline carries ("4.42.0"), `<variant>` the label that run gave the
build ("v4.42.0"). Rounds are counted once: every round is known by the hash of
its receipt, so giving the same file twice changes nothing. A release is one
commit; rounds of a different commit under the same release name are refused.
Builds that are not releases do not belong in the baseline.

`check` compares runs with the baseline without changing it and prints one row
per category: the baseline, who set it, the best time of the given rounds and
their ratio. With `--fail-over 1.15` it exits 1 when any category is more than
15% slower than its baseline.

`table` prints the baseline as a Markdown table.

The baseline file carries the release, its commit, the hashes of the harness
binaries measured, the compiler version, the dates and the hardware class it is
told with `--platform`. It carries no path and no machine name. Times on other
hardware differ: compare a run with a baseline measured on the same machine
class, or rebuild the baseline there (`update` with a new file).

Python 3 standard library only.
"""
import json
import sys
from pathlib import Path

RULE = (
    "The baseline of a category is the best (lowest) time per statement any release has reached in it. "
    "A release that reaches a new best sets the baseline; nothing else moves it."
)
TIME = "mean time per statement over the timed iterations of a category in one round, in microseconds; the best round counts"


def fail(message):
    print(f"ERROR: {message}", file=sys.stderr)
    sys.exit(1)


def read_rounds(spec):
    """`<variant>@<results.json>` -> (variant record, {category: [time per round]}, [(receipt hash, date)])."""
    variant, separator, path = spec.partition("@")
    if not separator or not variant or not path:
        fail(f"'{spec}' is not <variant>@<results.json>")
    try:
        results = json.loads(Path(path).read_text())
    except (OSError, ValueError) as error:
        fail(f"cannot read {path}: {error}")
    record = next((item for item in results.get("variants", []) if item.get("label") == variant), None)
    if record is None:
        labels = ", ".join(item.get("label", "?") for item in results.get("variants", []))
        fail(f"{path} has no variant '{variant}' (it has: {labels})")
    arms = [arm for arm in results.get("arms", []) if arm.get("label") == variant]
    times = {}
    for category in results.get("categories", []):
        rounds = category.get("nano_runs_us", {}).get(variant)
        if not rounds:
            fail(f"{path}: category '{category.get('category')}' has no rounds of '{variant}'")
        if len(rounds) != len(arms):
            fail(f"{path}: '{variant}' has {len(arms)} runs but {len(rounds)} times for '{category['category']}'")
        times[category["category"]] = [float(value) for value in rounds]
    if not times:
        fail(f"{path} holds no categories")
    identities = [(arm["receipt_sha256"], str(arm.get("ended_at", ""))[:10]) for arm in arms]
    return record, times, identities


def recompute(baseline):
    """Every category's baseline from the releases' best times; returns the categories whose holder changed."""
    moved = []
    for name, category in baseline["categories"].items():
        release, best = min(category["best_us"].items(), key=lambda item: (item[1], item[0]))
        if category.get("set_by") != release:
            moved.append((name, category.get("set_by"), category.get("baseline_us"), release, best))
        category["baseline_us"] = best
        category["set_by"] = release
    return moved


def duration(microseconds):
    if microseconds >= 1000:
        return f"{microseconds / 1000:.2f} ms"
    if microseconds >= 100:
        return f"{microseconds:.0f} µs"
    if microseconds >= 10:
        return f"{microseconds:.1f} µs"
    return f"{microseconds:.2f} µs"


def update(arguments):
    platform = None
    if "--platform" in arguments:
        at = arguments.index("--platform")
        if at + 1 >= len(arguments):
            fail("--platform needs a text")
        platform = arguments[at + 1]
        del arguments[at : at + 2]
    if len(arguments) < 2:
        fail("update needs <baseline.json> and at least one <release>=<variant>@<results.json>")
    path = Path(arguments[0])
    if path.is_file():
        baseline = json.loads(path.read_text())
    else:
        baseline = {"schema_version": 1, "benchmark": "pg35", "releases": {}, "categories": {}}
    baseline["rule"] = RULE
    baseline["time"] = TIME
    baseline["harness"] = "tests/pg35_benchmark.rs through benches/public/pg35_run.py (strict: results checked, 300 timed iterations)"
    if platform:
        baseline["platform"] = platform

    for argument in arguments[1:]:
        release, separator, spec = argument.partition("=")
        if not separator or not release:
            fail(f"'{argument}' is not <release>=<variant>@<results.json>")
        record, times, identities = read_rounds(spec)
        entry = baseline["releases"].setdefault(
            release,
            {"commit": record["commit"], "harness_binaries_sha256": [], "rustc": [], "rounds": 0, "measured": [], "receipts_sha256": []},
        )
        if entry["commit"] != record["commit"]:
            fail(f"release {release} is commit {entry['commit'][:12]}; {spec} measured {record['commit'][:12]}")
        if baseline["categories"] and set(times) != set(baseline["categories"]):
            fail(f"{spec} does not hold the baseline's categories")
        fresh = [index for index, (receipt, _) in enumerate(identities) if receipt not in entry["receipts_sha256"]]
        for name, rounds in times.items():
            category = baseline["categories"].setdefault(name, {"best_us": {}})
            candidates = [rounds[index] for index in fresh]
            if release in category["best_us"]:
                candidates.append(category["best_us"][release])
            if candidates:
                category["best_us"][release] = round(min(candidates), 3)
        for index in fresh:
            receipt, day = identities[index]
            entry["receipts_sha256"].append(receipt)
            if day and day not in entry["measured"]:
                entry["measured"].append(day)
        entry["rounds"] = len(entry["receipts_sha256"])
        entry["measured"].sort()
        compiler = record.get("rustc", "").splitlines()[0] if record.get("rustc") else ""
        for collection, value in ((entry["harness_binaries_sha256"], record.get("pg35_binary_sha256")), (entry["rustc"], compiler)):
            if value and value not in collection:
                collection.append(value)
        print(f"{release}: {len(fresh)} new round(s) from {spec}, {entry['rounds']} in all")

    moved = recompute(baseline)
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(baseline, indent=1, ensure_ascii=False) + "\n")
    for name, before, before_time, release, best in moved:
        if before is None:
            continue
        print(f"new baseline: {name}: {duration(best)} by {release} (was {duration(before_time)} by {before})")
    holders = {}
    for category in baseline["categories"].values():
        holders[category["set_by"]] = holders.get(category["set_by"], 0) + 1
    print("baseline held by: " + ", ".join(f"{release} in {count}" for release, count in sorted(holders.items())))


def check(arguments):
    limit = None
    if "--fail-over" in arguments:
        at = arguments.index("--fail-over")
        if at + 1 >= len(arguments):
            fail("--fail-over needs a ratio")
        limit = float(arguments[at + 1])
        del arguments[at : at + 2]
    if len(arguments) < 2:
        fail("check needs <baseline.json> and at least one <variant>@<results.json>")
    baseline = json.loads(Path(arguments[0]).read_text())
    best = {}
    rounds = 0
    for spec in arguments[1:]:
        _, times, identities = read_rounds(spec)
        rounds += len(identities)
        for name, values in times.items():
            best[name] = min([*values, best.get(name, float("inf"))])
    if set(best) != set(baseline["categories"]):
        fail("the runs do not hold the baseline's categories")
    print(f"| Category | Baseline | Set by | Best of {rounds} round(s) | Time / baseline |")
    print("|---|---:|---|---:|---:|")
    over, records = [], []
    for name, category in baseline["categories"].items():
        ratio = best[name] / category["baseline_us"]
        if ratio < 1:
            records.append(name)
        if limit is not None and ratio > limit:
            over.append(name)
        print(f"| {name} | {duration(category['baseline_us'])} | {category['set_by']} | {duration(best[name])} | {ratio:.2f} |")
    print()
    print(f"Better than the baseline in {len(records)} of {len(best)} categories" + (": " + ", ".join(records) + "." if records else "."))
    if limit is not None:
        print(f"More than {limit:.2f} times the baseline in {len(over)}" + (": " + ", ".join(over) + "." if over else "."))
        if over:
            sys.exit(1)


def table(arguments):
    if len(arguments) != 1:
        fail("table needs <baseline.json>")
    baseline = json.loads(Path(arguments[0]).read_text())
    releases = sorted(baseline["releases"])
    print("| Category | Baseline | Set by | " + " | ".join(f"Best of {release}" for release in releases) + " |")
    print("|---|---:|---|" + "---:|" * len(releases))
    for name, category in baseline["categories"].items():
        cells = [duration(category["best_us"][release]) if release in category["best_us"] else "" for release in releases]
        print(f"| {name} | {duration(category['baseline_us'])} | {category['set_by']} | " + " | ".join(cells) + " |")


def main():
    arguments = sys.argv[1:]
    commands = {"update": update, "check": check, "table": table}
    if not arguments or arguments[0] not in commands:
        print(__doc__, file=sys.stderr)
        sys.exit(2)
    commands[arguments[0]](arguments[1:])


if __name__ == "__main__":
    main()
