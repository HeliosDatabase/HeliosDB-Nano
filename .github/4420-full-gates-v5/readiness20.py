#!/usr/bin/env python3
"""Run real exact readiness controls concurrently after a same-stage Cargo build.
Preparation only; this program has not executed product tests.
"""
import argparse
from concurrent.futures import ThreadPoolExecutor, as_completed
import hashlib
import json
import os
from pathlib import Path
import signal
import subprocess
import time
import threading
import re

HERE = Path(__file__).resolve().parent

def sha(path):
    digest = hashlib.sha256()
    with path.open('rb') as stream:
        while chunk := stream.read(1024 * 1024): digest.update(chunk)
    return digest.hexdigest()

def artifacts(text):
    found = {}
    for line in text.splitlines():
        try: item = json.loads(line)
        except json.JSONDecodeError: continue
        if item.get('reason') != 'compiler-artifact' or not item.get('executable'): continue
        if 'test' not in item.get('target', {}).get('kind', []): continue
        if not item.get('profile', {}).get('test'): continue
        name = item['target']['name']
        executable = Path(item['executable']).resolve()
        if name in found and found[name] != executable: raise ValueError('ambiguous test executable '+name)
        found[name] = executable
    return found

def complete(text, name):
    summaries = re.findall(r'test result: (ok|FAILED)\. (\d+) passed; (\d+) failed; (\d+) ignored;', text)
    return bool(summaries and summaries[-1] == ('ok', '1', '0', '0') and
                re.search(r'\btest '+re.escape(name)+r' \.\.\. (?:[^\n]*\n)*?ok\b', text))

def terminate(process):
    # A timeout affects only this invocation and its daemon children, all in
    # the process group the runner created. It cannot target another job.
    if process.poll() is not None: return
    os.killpg(process.pid, signal.SIGTERM)
    try: process.wait(timeout=5)
    except subprocess.TimeoutExpired:
        os.killpg(process.pid, signal.SIGKILL)
        process.wait()

def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--tree-env', action='store_true', required=True)
    parser.add_argument('--build-log', required=True)
    parser.add_argument('--repeat', type=int, default=20)
    parser.add_argument('--workers', type=int, default=2)
    parser.add_argument('--case-timeout', type=int, default=1200)
    args = parser.parse_args()
    if os.environ.get('NANO_FINAL_GATE_BOUNDED') != '1': raise SystemExit('requires bounded stage runner')
    if args.repeat != 20 or not 2 <= args.workers <= 4: raise SystemExit('acceptance requires20 repeats and2-4 actual parallel workers')
    tree = Path(os.environ['NANO_FINAL_GATE_TREE']).resolve()
    out = Path(os.environ['NANO_FINAL_GATE_OUT']).resolve()
    binaries = artifacts((out/args.build_log).read_text())
    cases = json.loads((HERE/'readiness-cases.json').read_text())
    assert len(cases) == 6 and len({(x['target'], x['case']) for x in cases}) == 6
    paths = {row['target']: binaries[row['target']] for row in cases}
    identity = {target: {'path': str(path), 'sha256': sha(path)} for target, path in paths.items()}
    native = out/'readiness20-native'; native.mkdir(exist_ok=False)
    cancelled = threading.Event()
    active_lock = threading.Lock()
    active = {}
    def stop_all(_signal, _frame):
        cancelled.set()
        with active_lock:
            for process in active.values():
                if process.poll() is None:
                    try: os.killpg(process.pid, signal.SIGKILL)
                    except ProcessLookupError: pass
    signal.signal(signal.SIGTERM, stop_all)
    signal.signal(signal.SIGINT, stop_all)
    def run(row, repeat):
        binary = paths[row['target']]
        assert sha(binary) == identity[row['target']]['sha256'], 'test binary identity moved'
        log = native/f"{row['target']}-{row['case']}-{repeat:02d}.log"
        argv = [str(binary), '--exact', row['case'], '--nocapture', '--test-threads=1']
        start = time.monotonic(); timed_out = False
        with log.open('w') as stream:
            with active_lock:
                if cancelled.is_set():
                    stream.write('stage cancelled before this native case started\n')
                    process = None
                else:
                    process = subprocess.Popen(argv, cwd=tree, stdout=stream, stderr=subprocess.STDOUT, start_new_session=True)
                    active[process.pid] = process
            if process is None:
                code = 125
            else:
                try: code = process.wait(timeout=args.case_timeout)
                except subprocess.TimeoutExpired:
                    timed_out = True; terminate(process); code = 124
                finally:
                    with active_lock: active.pop(process.pid, None)
        end = time.monotonic()
        text = log.read_text(errors='replace')
        passed = code == 0 and not timed_out and complete(text, row['case'])
        return {**row, 'repeat': repeat, 'argv': argv, 'binary_sha256': identity[row['target']]['sha256'],
                'started_monotonic': start, 'finished_monotonic': end, 'elapsed_s': end-start,
                'exit': code, 'timed_out': timed_out, 'one_exact_pass': passed, 'log': str(log)}
    receipts = []
    with ThreadPoolExecutor(max_workers=args.workers) as pool:
        pending = [pool.submit(run, row, repeat) for repeat in range(1, args.repeat+1) for row in cases]
        for future in as_completed(pending): receipts.append(future.result())
    for row in receipts:
        row['overlapped_another_native_case'] = any(other is not row and
            other['started_monotonic'] < row['finished_monotonic'] and
            row['started_monotonic'] < other['finished_monotonic'] for other in receipts)
    wanted = {(row['target'], row['case'], repeat) for row in cases for repeat in range(1, args.repeat+1)}
    actual = {(row['target'], row['case'], row['repeat']) for row in receipts}
    unchanged = all(sha(paths[target]) == value['sha256'] for target, value in identity.items())
    passed = len(receipts) == 120 and actual == wanted and unchanged and all(
        row['one_exact_pass'] and row['overlapped_another_native_case'] for row in receipts)
    report = {'prepared_stage': 'readiness20', 'expected': 120, 'actual_unique': len(actual), 'workers': args.workers,
              'binary_identities': identity, 'binary_identity_unchanged': unchanged, 'passed': passed,
              'cases': sorted(receipts, key=lambda row: (row['target'], row['case'], row['repeat']))}
    (out/'readiness20-receipt.json').write_text(json.dumps(report, indent=2)+'\n')
    print('READINESS20_RECEIPT '+json.dumps({'expected':120, 'actual_unique':len(actual), 'passed':passed, 'workers':args.workers}))
    raise SystemExit(0 if passed else 1)

if __name__ == '__main__': main()
