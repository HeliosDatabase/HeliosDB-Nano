#!/usr/bin/env python3
"""Isolated CI adapter for the unchanged v5 correctness runner; draft only."""
import argparse
import fcntl
import hashlib
import json
import os
from pathlib import Path
import signal
import subprocess
import sys

HERE = Path(__file__).resolve().parent
V5_BUNDLE_SHA256 = '1c2884e87666bc46e4b4571c174467b46a374ac52f285a95398d433146fbd5a2'
COUNT_MANIFEST_SHA256 = '1e223b3ed2e218e62402e0c27f1ec96b4ad936f60c38897b7c07ff1b77bc1423'


def git(tree, *args):
    return subprocess.check_output(['git', '-C', str(tree), *args], timeout=30)


def sha(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def source(tree, model, expected_sha):
    if git(tree, 'rev-parse', 'HEAD').decode().strip() != expected_sha:
        raise RuntimeError('event/checkout HEAD mismatch')
    if git(tree, 'diff', '--name-only', 'HEAD') or git(tree, 'ls-files', '--others', '--exclude-standard'):
        raise RuntimeError('checkout is not clean')
    entries = {}
    for row in git(tree, 'ls-tree', '-rz', '--full-tree', 'HEAD').split(b'\0'):
        if not row:
            continue
        meta, rawname = row.split(b'\t', 1)
        mode, kind, oid = meta.decode().split()
        if kind != 'blob':
            raise RuntimeError('unsupported Git source entry')
        entries[rawname.decode()] = {'mode': mode, 'oid': oid}
    expected = model['entries']
    if set(entries) != set(expected) | set(model['auxiliary_paths']):
        raise RuntimeError('unexpected committed source paths')
    for name, item in expected.items():
        path = tree / name
        if entries[name] != {key: item[key] for key in ('mode', 'oid')}:
            raise RuntimeError('product Git identity changed: ' + name)
        if item['mode'] == '120000':
            if not path.is_symlink() or os.readlink(path) != item['target_text']:
                raise RuntimeError('product symlink changed: ' + name)
        elif path.is_symlink() or not path.is_file() or sha(path) != item['sha256']:
            raise RuntimeError('product physical bytes changed: ' + name)
    for name in model['auxiliary_paths']:
        path = tree / name
        if entries[name]['mode'] != '100644' or path.is_symlink() or not path.is_file() or path.read_bytes() != git(tree, 'show', 'HEAD:' + name):
            raise RuntimeError('auxiliary bytes differ from committed input: ' + name)
    return {'head': expected_sha, 'product_paths': len(expected), 'all_paths': len(entries), 'canonical_source_pass': True}


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--tree', required=True, type=Path)
    parser.add_argument('--out', required=True, type=Path)
    parser.add_argument('--stage', choices=['core-default', 'core-internal', 'core-optins'], required=True)
    parser.add_argument('--head', required=True)
    parser.add_argument('--model-sha256', required=True)
    args = parser.parse_args()
    tree = args.tree.resolve()
    model_path = HERE / 'model.json'
    if sha(model_path) != args.model_sha256:
        raise RuntimeError('canonical product model pin mismatch')
    model = json.loads(model_path.read_text())
    for manifest, digest, field in [(HERE / 'bundle-sha256.json', V5_BUNDLE_SHA256, None), (HERE / 'count-adjudicator/manifest.json', COUNT_MANIFEST_SHA256, 'files')]:
        if sha(manifest) != digest:
            raise RuntimeError('unchanged validation package manifest pin mismatch')
        items = json.loads(manifest.read_text())
        if field:
            items = items[field]
        for name, record in items.items():
            expected = record['sha256'] if isinstance(record, dict) else record
            if sha(manifest.parent / name) != expected:
                raise RuntimeError('unchanged validation package member mismatch: ' + name)
    group = next(row.split(':', 2)[2].strip() for row in Path('/proc/self/cgroup').read_text().splitlines() if row.startswith('0::'))
    cgroup = Path('/sys/fs/cgroup') / group.lstrip('/')
    limits = {name: (cgroup / name).read_text().strip() for name in ['memory.max', 'memory.swap.max']}
    if limits != {'memory.max': str(24 * 1024**3), 'memory.swap.max': '0'}:
        raise RuntimeError('24 GiB no-swap isolated scope required')
    # Each hosted VM owns its isolated fleet namespace. This is the same
    # exclusion checked by the unchanged native-list collector on that VM.
    fleet = Path('/home/gpc/HDB/sprint/coordination/build.lock').open('r+')
    fcntl.flock(fleet, fcntl.LOCK_EX | fcntl.LOCK_NB)
    args.out.mkdir(parents=True, exist_ok=False)
    before = source(tree, model, args.head)
    (args.out / 'source-before.json').write_text(json.dumps(before, indent=2) + '\n')
    (args.out / 'host.json').write_text(json.dumps({'cgroup': group, 'limits': limits, 'meminfo': Path('/proc/meminfo').read_text(), 'uname': list(os.uname()), 'cpu_count': os.cpu_count(), 'test_threads': os.environ.get('RUST_TEST_THREADS'), 'namespace': 'isolated hosted CI VM, not the shared development host'}, indent=2) + '\n')
    capture = args.out / 'capture'
    subprocess.run([sys.executable, str(HERE / 'inventory.py'), str(tree), str(capture), '--label', 'cloud-' + args.stage + '-' + args.head], check=True)
    target = args.out / 'fresh-target'
    if target.exists():
        raise RuntimeError('Cargo target must be fresh')
    env = os.environ.copy()
    env.update(NANO_FINAL_GATE_BOUNDED='1', CARGO_BUILD_JOBS='2', CARGO_TERM_COLOR='never')
    env.update(CARGO_TARGET_DIR=str(target), CARGO_PROFILE_TEST_DEBUG='0', CARGO_PROFILE_DEV_DEBUG='0')
    preflight = []
    features = json.loads((HERE / 'commands.json').read_text())[args.stage]['features']
    for index, row in enumerate(model['targeted_preflight']):
        argv = ['cargo', 'test', '--locked']
        if features:
            argv += ['--features', ','.join(features)]
        argv += row['cargo_args'] + ['--', '--nocapture', '--test-threads=2']
        log = args.out / ('preflight-%02d.log' % index)
        timed_out = False
        with log.open('x') as stream:
            checked = subprocess.Popen(argv, env=env, cwd=tree, stdout=stream, stderr=subprocess.STDOUT, start_new_session=True)
            try:
                checked.wait(timeout=row.get('timeout_seconds', 1800))
            except subprocess.TimeoutExpired:
                timed_out = True
                try:
                    os.killpg(checked.pid, signal.SIGTERM)
                except ProcessLookupError:
                    pass
                try:
                    checked.wait(timeout=15)
                except subprocess.TimeoutExpired:
                    try:
                        os.killpg(checked.pid, signal.SIGKILL)
                    except ProcessLookupError:
                        pass
                    checked.wait()
        text = log.read_text(errors='replace')
        missing = [marker for marker in row['required_markers'] if marker not in text]
        entry = {'argv': argv, 'exit_code': checked.returncode, 'timed_out': timed_out, 'log_sha256': sha(log), 'missing_markers': missing}
        preflight.append(entry)
        (args.out / 'preflight-receipt.json').write_text(json.dumps(preflight, indent=2) + '\n')
        if checked.returncode or missing or timed_out:
            after = source(tree, model, args.head)
            (args.out / 'source-after.json').write_text(json.dumps(after, indent=2) + '\n')
            (args.out / 'adapter-result.json').write_text(json.dumps({'stage': args.stage, 'preflight_failed': True, 'source_guards_pass': before == after, 'release_acceptance_pass': False}, indent=2) + '\n')
            return checked.returncode or 1
    command = [sys.executable, str(HERE / 'run_stage.py'), args.stage, '--tree', str(tree), '--target', str(target), '--out', str(args.out / 'stage'), '--source-manifest', str(capture / 'source-sha256.json'), '--expected-manifest', str(capture / 'expected-manifest.json'), '--execute']
    result = subprocess.run(command, env=env)
    count_result = None
    receipt = args.out / 'stage/receipt.json'
    if receipt.is_file():
        judge = HERE / 'count-adjudicator/adjudicate.py'
        plan_out = args.out / 'count-plan'
        plan_command = [sys.executable, str(judge), 'plan', '--receipt', str(receipt), '--receipt-sha', sha(receipt), '--tree', str(tree), '--target-root', str(target), '--source-manifest', str(capture / 'source-sha256.json'), '--source-sha', sha(capture / 'source-sha256.json'), '--expected-manifest', str(capture / 'expected-manifest.json'), '--expected-sha', sha(capture / 'expected-manifest.json'), '--bundle', str(HERE), '--out', str(plan_out)]
        planned = subprocess.run(plan_command, env=env)
        if planned.returncode == 0:
            plan = plan_out / 'plan.json'
            listed_out = args.out / 'native-lists'
            listed = subprocess.run([sys.executable, str(judge), 'collect', '--plan', str(plan), '--plan-sha', sha(plan), '--out', str(listed_out), '--execute-list-only'], env=env)
            if listed.returncode == 0:
                inventory = listed_out / 'inventory.json'
                adjudicated = subprocess.run([sys.executable, str(judge), 'adjudicate', '--plan', str(plan), '--plan-sha', sha(plan), '--inventory', str(inventory), '--inventory-sha', sha(inventory), '--out', str(args.out / 'count-report')], env=env)
                count_result = adjudicated.returncode
            else:
                count_result = listed.returncode
        else:
            count_result = planned.returncode
    after = source(tree, model, args.head)
    (args.out / 'source-after.json').write_text(json.dumps(after, indent=2) + '\n')
    if before != after:
        raise RuntimeError('source identity moved during gate')
    (args.out / 'adapter-result.json').write_text(json.dumps({'stage': args.stage, 'command': command, 'exit_code': result.returncode, 'native_count_exit_code': count_result, 'fresh_target_before_build': True, 'source_guards_pass': True, 'release_acceptance_pass': False, 'note': 'Requirement-specific invariant review and the remaining quality matrix remain required; isolated hosted VM has its own exclusive fleet namespace.'}, indent=2) + '\n')
    return result.returncode or count_result or (1 if count_result is None else 0)


if __name__ == '__main__':
    sys.exit(main())
