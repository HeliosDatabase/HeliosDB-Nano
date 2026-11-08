#!/usr/bin/env python3
"""Run frozen v5 supplemental stages on one isolated hosted VM."""
import argparse
import fcntl
import json
import os
from pathlib import Path
import subprocess
import sys

from launch import COUNT_MANIFEST_SHA256, HERE, V5_BUNDLE_SHA256, sha, source

GROUPS = {
    'physical': ['physical-default', 'physical-internal', 'physical-persistent', 'ddl-window', 'readiness20', 'archive-reserve-native', 'p4-scale'],
    'measure': ['docs', 'legacy-network', 'recall-measure', 'applier-throughput', 'codegraph-field', 'public-perf-smoke'],
}


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--tree', required=True, type=Path)
    parser.add_argument('--out', required=True, type=Path)
    parser.add_argument('--group', required=True, choices=['physical', 'measure'])
    parser.add_argument('--head', required=True)
    parser.add_argument('--model-sha256', required=True)
    args = parser.parse_args()
    tree = args.tree.resolve()
    model_path = HERE / 'model.json'
    if sha(model_path) != args.model_sha256:
        raise RuntimeError('canonical model pin mismatch')
    model = json.loads(model_path.read_text())
    for manifest, pin, field in [(HERE / 'bundle-sha256.json', V5_BUNDLE_SHA256, None), (HERE / 'count-adjudicator/manifest.json', COUNT_MANIFEST_SHA256, 'files')]:
        if sha(manifest) != pin:
            raise RuntimeError('validation manifest changed')
        rows = json.loads(manifest.read_text())
        rows = rows[field] if field else rows
        for name, item in rows.items():
            if sha(manifest.parent / name) != (item['sha256'] if isinstance(item, dict) else item):
                raise RuntimeError('validation member changed: ' + name)
    group = next(row.split(':', 2)[2].strip() for row in Path('/proc/self/cgroup').read_text().splitlines() if row.startswith('0::'))
    cgroup = Path('/sys/fs/cgroup') / group.lstrip('/')
    limits = {name: (cgroup / name).read_text().strip() for name in ['memory.max', 'memory.swap.max']}
    if limits != {'memory.max': str(24 * 1024**3), 'memory.swap.max': '0'}:
        raise RuntimeError('24 GiB zero-swap scope required')
    fleet = Path('/home/gpc/HDB/sprint/coordination/build.lock').open('r+')
    fcntl.flock(fleet, fcntl.LOCK_EX | fcntl.LOCK_NB)
    args.out.mkdir(parents=True, exist_ok=False)
    before = source(tree, model, args.head)
    (args.out / 'source-before.json').write_text(json.dumps(before, indent=2) + '\n')
    (args.out / 'host.json').write_text(json.dumps({'namespace': 'isolated hosted VM', 'cgroup': group, 'limits': limits, 'uname': list(os.uname()), 'meminfo': Path('/proc/meminfo').read_text()}, indent=2) + '\n')
    capture = args.out / 'capture'
    subprocess.run([sys.executable, str(HERE / 'inventory.py'), str(tree), str(capture), '--label', 'supplemental-' + args.group + '-' + args.head], check=True)
    target = args.out / 'fresh-target'
    if target.exists():
        raise RuntimeError('target must be fresh and owned')
    wanted = model['supplemental_groups'][args.group]
    if wanted != GROUPS[args.group]:
        raise RuntimeError('supplemental group must contain every exact frozen stage in order')
    env = os.environ.copy()
    env.update(NANO_FINAL_GATE_BOUNDED='1', CARGO_BUILD_JOBS='2', CARGO_TERM_COLOR='never')
    alias = Path('/home/gpc/HDB/sprint/baselines/nano/4420-resume-20261107/final-gates-v5')
    if args.group == 'physical' and (alias.resolve() != HERE or sha(alias / 'readiness20.py') != sha(HERE / 'readiness20.py')):
        raise RuntimeError('unchanged readiness recipe helper alias differs')
    receipts = []
    for stage in wanted:
        source(tree, model, args.head)
        argv = [sys.executable, str(HERE / 'run_stage.py'), stage, '--tree', str(tree), '--target', str(target), '--out', str(args.out / stage), '--source-manifest', str(capture / 'source-sha256.json'), '--expected-manifest', str(capture / 'expected-manifest.json'), '--execute']
        print('SUPPLEMENTAL_STAGE_BEGIN ' + stage, flush=True)
        result = subprocess.run(argv, env=env)
        receipts.append({'stage': stage, 'argv': argv, 'exit_code': result.returncode})
        (args.out / 'stages.json').write_text(json.dumps(receipts, indent=2) + '\n')
        print('SUPPLEMENTAL_STAGE_END ' + stage + ' exit=' + str(result.returncode), flush=True)
        source(tree, model, args.head)
    after = source(tree, model, args.head)
    (args.out / 'source-after.json').write_text(json.dumps(after, indent=2) + '\n')
    complete = len(receipts) == len(wanted) and all(row['exit_code'] == 0 for row in receipts) and before == after
    (args.out / 'adapter-result.json').write_text(json.dumps({'group': args.group, 'all_stages_attempted': len(receipts) == len(wanted), 'source_guards_pass': before == after, 'commands_green': complete, 'release_acceptance_pass': False, 'review_required': 'Exact native counts and requirement-specific invariants in raw logs/receipts; timing-only and approximate recall tables are not acceptance.'}, indent=2) + '\n')
    return 0 if complete else 1


if __name__ == '__main__':
    sys.exit(main())
