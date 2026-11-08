#!/usr/bin/env python3
"""Matched pristine/candidate captures using the unchanged frozen v5 comparator."""
import argparse
import fcntl
import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys

HERE = Path(__file__).resolve().parent
BUNDLE_SHA = 'ab3fc4d38d6120283df857cfd5a24f7002cbb74b3a6f9952f03ee01ab3706568'
STAGES = ('clippy-capture', 'clippy-internal-capture', 'clippy-optins-capture', 'clippy-fips-capture')
ENV_KEYS = ('RUSTUP_TOOLCHAIN', 'CARGO_HOME', 'RUSTUP_HOME', 'RUSTFLAGS', 'CARGO_ENCODED_RUSTFLAGS',
            'CARGO_BUILD_JOBS', 'CARGO_BUILD_TARGET', 'CARGO_PROFILE_TEST_DEBUG', 'CARGO_PROFILE_DEV_DEBUG',
            'CMAKE_BUILD_PARALLEL_LEVEL', 'RUST_TEST_THREADS', 'CC', 'CXX', 'AR', 'LIBCLANG_PATH')


def sha(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def write(path, data):
    path.write_text(json.dumps(data, indent=2, sort_keys=True) + '\n')


def command(argv, cwd, env, timeout=60):
    return subprocess.check_output(argv, cwd=cwd, env=env, timeout=timeout)


def source(tree, role, model, head, env):
    def git(*args):
        return command(['git', '-C', str(tree), *args], tree, env)
    if git('rev-parse', 'HEAD').decode().strip() != head:
        raise ValueError(role + ' HEAD mismatch')
    if git('diff', '--name-only', 'HEAD') or git('ls-files', '--others', '--exclude-standard'):
        raise ValueError(role + ' is not pristine/clean')
    expected = model['entries'][role]
    extra = set(model['auxiliary_paths']) if role == 'candidate' else set()
    entries = {}
    for row in git('ls-tree', '-rz', '--full-tree', 'HEAD').split(b'\0'):
        if not row:
            continue
        info, name = row.split(b'\t', 1)
        mode, kind, oid = info.decode().split()
        if kind != 'blob':
            raise ValueError('unsupported committed input')
        entries[name.decode()] = {'mode': mode, 'oid': oid}
    if set(entries) != set(expected) | extra:
        raise ValueError(role + ' complete source path set mismatch')
    physical = {}
    for name, record in expected.items():
        if entries[name] != {k: record[k] for k in ('mode', 'oid')}:
            raise ValueError(role + ' Git input changed: ' + name)
        path = tree / name
        if record['mode'] == '120000':
            if not path.is_symlink() or os.readlink(path) != record['target_text']:
                raise ValueError(role + ' symlink changed: ' + name)
        else:
            if path.is_symlink() or not path.is_file() or sha(path) != record['sha256']:
                raise ValueError(role + ' physical input changed: ' + name)
            physical[name] = record['sha256']
    for name in extra:
        path = tree / name
        if entries[name]['mode'] != '100644' or path.is_symlink() or path.read_bytes() != git('show', 'HEAD:' + name):
            raise ValueError('candidate auxiliary input changed: ' + name)
    archive = None
    if role == 'baseline':
        archive = hashlib.sha256(git('archive', '--format=tar', head)).hexdigest()
        if archive != model['baseline_archive_sha256']:
            raise ValueError('pristine inherited archive identity mismatch')
    return {'role': role, 'head': head, 'git_entries': entries, 'product_paths': len(expected),
            'regular_inputs': len(physical), 'physical_digest': hashlib.sha256(json.dumps(physical, sort_keys=True).encode()).hexdigest(),
            'pristine_archive_sha256': archive, 'canonical_source_pass': True}


def tools(env, cwd):
    result = {}
    for name in ('rustc', 'cargo', 'clippy-driver'):
        path = Path(command(['rustup', 'which', name], cwd, env).decode().strip())
        version = command([str(path), '-vV' if name == 'rustc' else '--version'], cwd, env).decode()
        result[name] = {'path': str(path), 'sha256': sha(path), 'version': version}
    return result


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--candidate', type=Path, required=True)
    parser.add_argument('--baseline', type=Path, required=True)
    parser.add_argument('--out', type=Path, required=True)
    parser.add_argument('--stage', choices=STAGES, required=True)
    parser.add_argument('--head', required=True)
    parser.add_argument('--model-sha256', required=True)
    args = parser.parse_args()
    args.out = args.out.resolve()
    args.out.mkdir(parents=True, exist_ok=False)
    result = {'schema': 'matched-clippy-source31-v1', 'stage': args.stage, 'status': 'invalid',
              'success': False, 'release_acceptance_pass': False, 'roles': {}}
    model = None
    trees = {'baseline': args.baseline.resolve(), 'candidate': args.candidate.resolve()}
    env = os.environ.copy()
    env.update(CARGO_BUILD_JOBS='2', CARGO_TERM_COLOR='never', CARGO_PROFILE_TEST_DEBUG='0',
               CARGO_PROFILE_DEV_DEBUG='0', CMAKE_BUILD_PARALLEL_LEVEL='2', NANO_FINAL_GATE_BOUNDED='1')
    before = {}
    fleet = None
    try:
        model_path = HERE / 'model.json'
        if sha(model_path) != args.model_sha256:
            raise ValueError('model bootstrap pin mismatch')
        model = json.loads(model_path.read_text())
        if tuple(model['stages']) != STAGES:
            raise ValueError('matched comparison stages changed')
        bundle = HERE / 'bundle-sha256.json'
        if sha(bundle) != BUNDLE_SHA:
            raise ValueError('unchanged v5 package manifest changed')
        pins = json.loads(bundle.read_text())
        for name, digest in pins.items():
            if sha(HERE / name) != digest:
                raise ValueError('unchanged v5 member changed: ' + name)
        group = next(x.split(':', 2)[2].strip() for x in Path('/proc/self/cgroup').read_text().splitlines() if x.startswith('0::'))
        cg = Path('/sys/fs/cgroup') / group.lstrip('/')
        limits = {n: (cg / n).read_text().strip() for n in ('memory.max', 'memory.swap.max')}
        if limits != {'memory.max': str(24 * 1024**3), 'memory.swap.max': '0'}:
            raise ValueError('24GiB/no-swap bounded service required')
        fleet = Path('/home/gpc/HDB/sprint/coordination/build.lock').open('r+')
        fcntl.flock(fleet, fcntl.LOCK_EX | fcntl.LOCK_NB)
        heads = {'baseline': model['baseline_commit'], 'candidate': args.head}
        for role, tree in trees.items():
            before[role] = source(tree, role, model, heads[role], env)
            write(args.out / (role + '-source-before.json'), before[role])
        tool_identity = tools(env, trees['candidate'])
        write(args.out / 'host.json', {'cgroup': group, 'limits': limits, 'meminfo': Path('/proc/meminfo').read_text(),
              'uname': list(os.uname()), 'cpu_count': os.cpu_count(), 'matched_environment': {k: env.get(k) for k in ENV_KEYS},
              'toolchain': tool_identity, 'baseline_choice': model['baseline_choice'], 'vm_namespace': 'exclusive hosted VM fleet'})
        stage_model = json.loads((HERE / 'commands.json').read_text())[args.stage]
        result['exact_capture_argv'] = stage_model['commands'][0]
        result['features'] = stage_model['features']
        for role, tree in trees.items():
            role_out = args.out / role
            role_out.mkdir()
            target = role_out / 'fresh-target'
            if target.exists():
                raise ValueError('nonfresh isolated ' + role + ' target')
            capture = role_out / 'capture'
            subprocess.run([sys.executable, str(HERE / 'inventory.py'), str(tree), str(capture), '--label', 'matched-clippy-' + role + '-' + args.stage], env=env, check=True, timeout=120)
            argv = [sys.executable, str(HERE / 'run_stage.py'), args.stage, '--tree', str(tree), '--target', str(target),
                    '--out', str(role_out / 'stage'), '--source-manifest', str(capture / 'source-sha256.json'),
                    '--expected-manifest', str(capture / 'expected-manifest.json'), '--execute']
            checked = subprocess.run(argv, env=env)
            result['roles'][role] = {'adapter_argv': argv, 'capture_exit': checked.returncode,
                                     'fresh_target_before_capture': True, 'target': str(target)}
            write(args.out / 'result.json', result)
        logs = [args.out / role / 'stage/00.log' for role in ('baseline', 'candidate')]
        if not all(log.is_file() for log in logs):
            raise ValueError('both actual Cargo JSON logs are required')
        diff_argv = [sys.executable, str(HERE / 'clippy-diff.py'), *map(str, logs)]
        with (args.out / 'comparison.json').open('x') as out, (args.out / 'comparison.stderr').open('x') as err:
            diff = subprocess.run(diff_argv, env=env, stdout=out, stderr=err, timeout=120)
        comparison = json.loads((args.out / 'comparison.json').read_text())
        result['comparison'] = {'argv': diff_argv, 'exit': diff.returncode, 'output_sha256': sha(args.out / 'comparison.json'),
                                'log_sha256': {role: sha(log) for role, log in zip(('baseline', 'candidate'), logs)}}
        tool_after = tools(env, trees['candidate'])
        if tool_after != tool_identity:
            raise ValueError('matched compiler/Clippy identity changed')
        invalid = any(r['capture_exit'] for r in result['roles'].values()) or comparison['baseline_compile_errors'] or comparison['candidate_compile_errors']
        result['status'] = 'invalid' if invalid else 'regression' if diff.returncode else 'pass'
        result['success'] = result['status'] == 'pass'
    except BaseException as error:
        result['error'] = {'type': type(error).__name__, 'message': str(error)}
        result['status'] = 'invalid'
        result['success'] = False
    finally:
        if model is not None:
            for role, tree in trees.items():
                try:
                    head = model['baseline_commit'] if role == 'baseline' else args.head
                    after = source(tree, role, model, head, env)
                    write(args.out / (role + '-source-after.json'), after)
                    if before.get(role) != after:
                        raise ValueError(role + ' source moved')
                except BaseException as error:
                    result['status'] = 'invalid'
                    result['success'] = False
                    result.setdefault('final_guard_errors', []).append({'role': role, 'type': type(error).__name__, 'message': str(error)})
        write(args.out / 'result.json', result)
    print(json.dumps({'stage': args.stage, 'status': result['status'], 'success': result['success']}), flush=True)
    return 0 if result['success'] else 1


if __name__ == '__main__':
    raise SystemExit(main())
