#!/usr/bin/env python3
"""Fourth isolated CI tier, reusing the unchanged v5 runner and count tool."""
import argparse
import fcntl
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import re
import subprocess
import sys

sys.dont_write_bytecode = True
HERE = Path(__file__).resolve().parent
V5_SHA = 'ab3fc4d38d6120283df857cfd5a24f7002cbb74b3a6f9952f03ee01ab3706568'
COUNT_SHA = '02a9a0898e9f58fda682552a52c92ea5e283556022fa0f9b8c3e8679cd540c5a'
ORT_SPEC_SHA = '00dde27899411b54d1a0878cd6a91231f6475b8b5ad76ce5947c30b08665e5e9'
ORT_HELPER_SHA = 'ba6845dd2476846e7320233e7fbb1d35c3b53aa5abee0af91d638321ecf64a6a'


def sha(path):
    digest = hashlib.sha256()
    with Path(path).open('rb') as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b''):
            digest.update(block)
    return digest.hexdigest()


def write(path, value):
    Path(path).write_text(json.dumps(value, indent=2, sort_keys=True) + '\n')


def pinned_json(path, expected):
    data = Path(path).read_bytes()
    if hashlib.sha256(data).hexdigest() != expected:
        raise ValueError('input pin mismatch: ' + str(path))
    return json.loads(data)


def module(name, path):
    spec = importlib.util.spec_from_file_location(name, path)
    result = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(result)
    return result


def count_tier(out, tree, target, capture, env):
    judge = HERE / 'count-adjudicator/adjudicate.py'
    receipt = out / 'stage/receipt.json'
    if not receipt.is_file():
        return None
    plan_out = out / 'count-plan'
    argv = [sys.executable, str(judge), 'plan', '--receipt', str(receipt), '--receipt-sha', sha(receipt),
            '--tree', str(tree), '--target-root', str(target), '--source-manifest', str(capture / 'source-sha256.json'),
            '--source-sha', sha(capture / 'source-sha256.json'), '--expected-manifest', str(capture / 'expected-manifest.json'),
            '--expected-sha', sha(capture / 'expected-manifest.json'), '--bundle', str(HERE), '--out', str(plan_out)]
    result = subprocess.run(argv, env=env)
    if result.returncode:
        return result.returncode
    plan = plan_out / 'plan.json'
    listed = out / 'native-lists'
    result = subprocess.run([sys.executable, str(judge), 'collect', '--plan', str(plan), '--plan-sha', sha(plan),
                             '--out', str(listed), '--execute-list-only'], env=env)
    if result.returncode:
        return result.returncode
    inventory = listed / 'inventory.json'
    return subprocess.run([sys.executable, str(judge), 'adjudicate', '--plan', str(plan), '--plan-sha', sha(plan),
                           '--inventory', str(inventory), '--inventory-sha', sha(inventory),
                           '--out', str(out / 'count-report')], env=env).returncode


def inference_proof(out, target, capture, command, test_source_sha):
    receipt_path = out / 'receipt.json'
    receipt = json.loads(receipt_path.read_bytes())
    if receipt['stage'] != 'embed-runtime' or receipt['failed'] is not False or len(receipt['commands']) != 1:
        raise ValueError('ignored inference stage is incomplete/failed')
    for key, path in [('source', capture / 'source-sha256.json'), ('expected', capture / 'expected-manifest.json')]:
        identity = receipt['manifest_inputs'][key]
        if identity['unchanged'] is not True or identity['sha256_before'] != sha(path) or identity['sha256_after'] != sha(path):
            raise ValueError('inference source/expected capture changed')
    row = receipt['commands'][0]
    if row['argv'] != command or row['exit'] != 0 or row['timed_out'] or row['completeness_problems']:
        raise ValueError('ignored inference command did not pass unchanged')
    if row['features']['explicit'] != ['code-embed'] or not row['toolchain']['rustc'].startswith('rustc 1.92.0 '):
        raise ValueError('inference feature/toolchain mismatch')
    expected = {'code_graph_local_embedder': {'status': 'ok', 'passed': 1, 'failed': 0, 'ignored': 0}}
    if row['results'] != expected:
        raise ValueError('inference must execute exactly one actual unignored test')
    log_path = Path(row['log'])
    text = log_path.read_text(errors='replace')
    summaries = re.findall(r'^test result: (ok|FAILED)\. (\d+) passed; (\d+) failed; (\d+) ignored; (\d+) measured; (\d+) filtered out;', text, re.M)
    if summaries != [('ok', '1', '0', '0', '0', '0')] or len(re.findall(r'^running 1 test$', text, re.M)) != 1:
        raise ValueError('ignored inference raw counts are not exact/nonzero')
    if 'test fastembed_populates_body_vec ... ' not in text:
        raise ValueError('actual named ignored inference test was not observed')
    binaries = [item for item in row['binaries'] if not item.get('role') and Path(item['path']).name.startswith('code_graph_local_embedder-')]
    if len(binaries) != 1:
        raise ValueError('inference needs exactly one captured native test executable')
    binary = Path(binaries[0]['path']).resolve()
    if (not binary.is_relative_to(target.resolve()) or not os.access(binary, os.X_OK)
            or sha(binary) != binaries[0]['sha256']):
        raise ValueError('inference native executable identity mismatch')
    with binary.open('rb') as stream:
        if stream.read(4) != b'\x7fELF':
            raise ValueError('inference artifact is not a native ELF executable')
    return {'actual_inference_pass': True, 'test': 'fastembed_populates_body_vec', 'parents_passed': 1,
            'nonempty_vector_dimension_asserted_by_pinned_test': 384, 'test_source_sha256': test_source_sha,
            'receipt_sha256': sha(receipt_path), 'log_sha256_postrun': sha(log_path),
            'native_binary': str(binary), 'native_binary_sha256': sha(binary),
            'command': command, 'release_acceptance_pass': False}


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--tree', type=Path, required=True)
    parser.add_argument('--out', type=Path, required=True)
    parser.add_argument('--head', required=True)
    parser.add_argument('--model-sha256', required=True)
    parser.add_argument('--execute', action='store_true', required=True)
    args = parser.parse_args()
    tree, out = args.tree.resolve(), args.out.resolve()
    if out.is_relative_to(tree):
        raise ValueError('CI output, cache and Cargo target must be outside product checkout')
    model = pinned_json(HERE / 'embed-model.json', args.model_sha256)
    ort_spec = pinned_json(HERE / 'ort-prereq.json', ORT_SPEC_SHA)
    inputs = {HERE / 'embed-model.json': args.model_sha256, HERE / 'ort-prereq.json': ORT_SPEC_SHA,
              HERE / 'ort_prereq.py': ORT_HELPER_SHA, HERE / 'launch.py': model['base_launch_sha256'],
              HERE / 'model.json': model['base_model_sha256'], Path(__file__): sha(Path(__file__))}
    for manifest, expected, field in [(HERE / 'bundle-sha256.json', V5_SHA, None),
                                       (HERE / 'count-adjudicator/manifest.json', COUNT_SHA, 'files')]:
        data = pinned_json(manifest, expected)
        inputs[manifest] = expected
        for name, record in (data[field] if field else data).items():
            inputs[manifest.parent / name] = record['sha256'] if isinstance(record, dict) else record
    def guard_inputs():
        if any(sha(path) != expected for path, expected in inputs.items()):
            raise ValueError('frozen adapter/validator input changed')
    guard_inputs()
    group = next(row.split(':', 2)[2].strip() for row in Path('/proc/self/cgroup').read_text().splitlines() if row.startswith('0::'))
    cgroup = Path('/sys/fs/cgroup') / group.lstrip('/')
    limits = {name: (cgroup / name).read_text().strip() for name in ['memory.max', 'memory.swap.max']}
    if limits != {'memory.max': str(24 * 1024**3), 'memory.swap.max': '0'}:
        raise ValueError('isolated 24GiB no-swap CI service required')
    fleet = Path('/home/gpc/HDB/sprint/coordination/build.lock').open('r+')
    fcntl.flock(fleet, fcntl.LOCK_EX | fcntl.LOCK_NB)
    out.mkdir(parents=True, exist_ok=False)
    adapter = module('unchanged_root_adapter', HERE / 'launch.py')
    ort = module('fixed_ort_prerequisite', HERE / 'ort_prereq.py')
    report = {'success': False, 'release_acceptance_pass': False, 'core_stage': 'core-embed-optins'}
    before = None
    runtime = None
    try:
        before = adapter.source(tree, model, args.head)
        write(out / 'source-before.json', before)
        write(out / 'host.json', {'cgroup': group, 'limits': limits, 'uname': list(os.uname()),
                                 'meminfo': Path('/proc/meminfo').read_text(), 'cpu_count': os.cpu_count(),
                                 'namespace': 'isolated hosted CI VM, not the shared development host'})
        runtime, runtime_env = ort.prepare(out / 'ort', ort_spec)
        write(out / 'ort/actual-load.json', ort.load_api(runtime, ort_spec))
        capture = out / 'capture'
        subprocess.run([sys.executable, str(HERE / 'inventory.py'), str(tree), str(capture),
                        '--label', 'cloud-code-embed-' + args.head], check=True)
        target = out / 'fresh-target'
        if target.exists():
            raise ValueError('code-embed Cargo target must be new')
        env = os.environ.copy()
        env.update(runtime_env)
        env.update(NANO_FINAL_GATE_BOUNDED='1', CARGO_BUILD_JOBS='2', CARGO_TERM_COLOR='never',
                   CARGO_TARGET_DIR=str(target), CARGO_PROFILE_TEST_DEBUG='0', CARGO_PROFILE_DEV_DEBUG='0',
                   RUSTUP_TOOLCHAIN='1.92.0', RUST_TEST_THREADS='2')
        write(out / 'environment.json', {key: env[key] for key in list(runtime_env) + ['RUSTUP_TOOLCHAIN', 'RUST_TEST_THREADS', 'CARGO_BUILD_JOBS', 'CARGO_PROFILE_TEST_DEBUG', 'CARGO_PROFILE_DEV_DEBUG', 'CARGO_TARGET_DIR']})
        commands = json.loads((HERE / 'commands.json').read_bytes())
        def stage(name, destination):
            guard_inputs()
            if adapter.source(tree, model, args.head) != before:
                raise ValueError('source changed before ' + name)
            return subprocess.run([sys.executable, str(HERE / 'run_stage.py'), name, '--tree', str(tree),
                                   '--target', str(target), '--out', str(destination),
                                   '--source-manifest', str(capture / 'source-sha256.json'),
                                   '--expected-manifest', str(capture / 'expected-manifest.json'), '--execute'], env=env).returncode
        core_exit = stage('core-embed-optins', out / 'stage')
        count_exit = count_tier(out, tree, target, capture, env)
        report.update(core_exit=core_exit, native_count_exit=count_exit)
        # Preserve independent inference evidence even when the complete tier is red.
        embed_exit = stage('embed-runtime', out / 'embed-runtime')
        report['embed_exit'] = embed_exit
        proof = inference_proof(out / 'embed-runtime', target, capture, commands['embed-runtime']['commands'][0],
                                model['entries']['tests/code_graph_local_embedder.rs']['sha256'])
        cache = ort.model_inventory(Path(runtime_env['HF_HOME']), ort_spec)
        write(out / 'model-cache-identity.json', cache)
        if not cache['default_model_onnx_files']:
            raise ValueError('actual default model/cache identity is absent')
        write(out / 'inference-proof.json', proof)
        ort.verify_runtime(runtime, ort_spec)
        guard_inputs()
        after = adapter.source(tree, model, args.head)
        write(out / 'source-after.json', after)
        if before != after:
            raise ValueError('source changed during code-embed gates')
        report.update(core_exit=core_exit, native_count_exit=count_exit, embed_exit=embed_exit,
                      source_guards_pass=True, inference_proof=proof,
                      success=core_exit == 0 and count_exit == 0 and embed_exit == 0)
    except Exception as error:
        report['error'] = {'type': type(error).__name__, 'message': str(error)}
    finally:
        try:
            guard_inputs()
            if runtime is not None:
                ort.verify_runtime(runtime, ort_spec)
            if before is not None:
                after = adapter.source(tree, model, args.head)
                write(out / 'source-after.json', after)
                report['source_guards_pass'] = before == after
                if before != after:
                    raise ValueError('source changed at final guard')
        except Exception as error:
            report['success'] = False
            report['final_guard_error'] = {'type': type(error).__name__, 'message': str(error)}
        write(out / 'adapter-result.json', report)
    return 0 if report['success'] else 1


if __name__ == '__main__':
    raise SystemExit(main())
