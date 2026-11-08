#!/usr/bin/env python3.11
"""Synthetic parser/provenance checks only; every child-process API is blocked."""
import argparse
import copy
import hashlib
import importlib.util
import json
from pathlib import Path
import sys
import tempfile

sys.dont_write_bytecode = True
HERE = Path(__file__).resolve().parent
spec = importlib.util.spec_from_file_location('count_aid', HERE / 'adjudicate.py')
aid = importlib.util.module_from_spec(spec)
spec.loader.exec_module(aid)


def forbidden(*args, **kwargs):
    raise AssertionError('synthetic check attempted a child process')


aid.subprocess.Popen = forbidden
aid.subprocess.check_output = forbidden
aid.subprocess.run = forbidden
passed = []
rejected = []


def yes(label, action):
    assert action(), label
    passed.append(label)


def no(label, action):
    try:
        value = action()
    except (aid.Refusal, OSError, KeyError, ValueError):
        rejected.append(label)
        return
    assert value is False, 'unexpectedly accepted: ' + label
    rejected.append(label)


def result(names, ignored=(), skipped=(), failed=0):
    selected = set(names) - set(skipped)
    return {'target': 'synthetic', 'running': len(selected), 'observed_names': sorted(selected),
            'summary': {'status': 'FAILED' if failed else 'ok', 'passed': len(selected - set(ignored)) - failed,
                        'failed': failed, 'ignored': len(set(ignored) & selected), 'measured': 0, 'filtered_out': len(skipped)}}


def run_checks():
    dangerous = {'PATH': '/synthetic', '__RUST_TEST_INVOKE': 'must-never-run', '__RUST_TEST_BENCH_BENCHMARKS': 'private', 'RUST_TEST_INVOKE': 'legacy-private', 'RUST_TEST_THREADS': '4'}
    yes('secondary libtest invoker is removed before native discovery', lambda: aid.listing_environment(dangerous) == ({'PATH': '/synthetic', 'RUST_TEST_THREADS': '4'}, ['RUST_TEST_INVOKE', '__RUST_TEST_BENCH_BENCHMARKS', '__RUST_TEST_INVOKE']) and dangerous['__RUST_TEST_INVOKE'] == 'must-never-run')
    names = {'generated::case_0': 'test', 'generated::case_1': 'test', 'bench_macro': 'benchmark',
             'slow_ignored': 'test', 'nested::lock_management::lease': 'test'}
    ignored = {'slow_ignored': 'test'}
    selected = result(names, ignored, {'nested::lock_management::lease'})
    yes('macro/benchmark/exact ignored and only permitted skip', lambda: aid.exact_count(selected, names, ignored, True)['correctness_green'])
    yes('known main0/cfg0 inventories', lambda: aid.exact_count(result({}), {}, {}, True)['correctness_green'])
    yes('all-ignored target remains exact zero runnable', lambda: aid.exact_count(result(ignored, ignored), ignored, ignored, False)['correctness_green'])
    yes('empty terse listing has no fabricated footer', lambda: aid.parse_listing('') == {})
    yes('terse native macro and benchmark names', lambda: aid.parse_listing('generated::case_0: test\nbench_macro: benchmark\n') == {'generated::case_0': 'test', 'bench_macro': 'benchmark'})
    yes('failed target can have complete exact count but never correctness green', lambda: (lambda p: p['count_complete'] and not p['correctness_green'])(aid.exact_count(result(names, ignored, failed=1), names, ignored, False)))
    for label, text in [('duplicate name', 'a: test\na: test\n'), ('footer rejected', 'a: test\n1 test, 0 benchmarks\n'),
                        ('malformed kind', 'a: unknown\n'), ('whitespace name', ' a: test\n')]:
        no(label, lambda text=text: aid.parse_listing(text))
    no('ignored name not in all list', lambda: aid.exact_count(selected, names, {'ghost': 'test'}, True))
    no('ignored kind mismatch', lambda: aid.exact_count(selected, names, {'slow_ignored': 'benchmark'}, True))
    for key in ['running', 'passed', 'failed', 'ignored', 'measured', 'filtered_out']:
        altered = copy.deepcopy(selected)
        if key == 'running': altered[key] += 1
        else: altered['summary'][key] += 1
        no('exact native count rejects changed ' + key, lambda altered=altered: aid.exact_count(altered, names, ignored, True)['count_complete'])
    altered = copy.deepcopy(selected); altered['observed_names'].remove('generated::case_1')
    no('missing generated parent line', lambda: aid.exact_count(altered, names, ignored, True)['count_complete'])
    no('unpermitted filter has no native count waiver', lambda: aid.exact_count(selected, names, ignored, False)['count_complete'])
    log = ('Running unittests src/lib.rs (/synthetic/target/deps/lib-fixture)\n'
           'running 2 tests\ntest macro::parent ... ok\nrunning 1 test\n'
           'test crash_child ... ok\ntest result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s\n'
           'test second ... ok\ntest result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s\n'
           'Running unittests src/main.rs (/synthetic/target/deps/main-fixture)\nrunning 0 tests\n'
           'test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s\n')
    yes('nested child summary cannot replace outer parent count', lambda: (lambda rows: len(rows) == 2 and rows[0]['running'] == rows[0]['summary']['passed'] == 2 and rows[0]['nested_summary_records'] == 1)(aid.parse_segments(log)))
    no('duplicate Cargo target', lambda: aid.parse_segments(log + log))
    no('unidentified target without Running record', lambda: aid.parse_segments('running 0 tests\n'))
    no('truncated native target', lambda: aid.parse_segments('Running tests/example.rs (/synthetic/target/deps/x)\nrunning 1 test\n'))
    with tempfile.TemporaryDirectory(prefix='nano-count-synthetic-') as temp:
        root = Path(temp)
        file = root / 'pin'; file.write_text('before'); pins = {str(file): aid.sha(file)}
        yes('stable evidence bytes', lambda: aid.pin_check(pins) is None)
        file.write_text('after'); no('linked evidence mutation', lambda: aid.pin_check(pins))
        tree = root / 'tree'; tree.mkdir(); (tree / 'a.rs').write_text('synthetic source')
        source = {'a.rs': aid.sha(tree / 'a.rs')}
        yes('exact source contents and file set', lambda: aid.source_guard(tree, source, {'a.rs'})['file_set_verified'])
        no('untracked source appearance', lambda: aid.source_guard(tree, source, {'a.rs', 'new.rs'}))
        no('source disappearance', lambda: aid.source_guard(tree, source, set()))
        (tree / 'a.rs').write_text('mutated'); no('source bytes mutation', lambda: aid.source_guard(tree, source, {'a.rs'}))
        no('absolute source key forbidden for v5 capture', lambda: aid.source_guard(tree, {str(tree / 'a.rs'): '0' * 64}, set()))
    check_context()
    check_critical()


def dump(path, data):
    path.write_text(json.dumps(data, indent=2) + '\n')
    return aid.sha(path)


def check_context():
    # Fake ELF headers are data fixtures only. Popen/check_output/run remain
    # forbidden; context receives an explicit synthetic source-file set.
    with tempfile.TemporaryDirectory(prefix='nano-count-context-') as temp:
        root = Path(temp); tree = root / 'tree'; tree.mkdir(); (tree / 'src').mkdir(); (tree / 'tests').mkdir()
        target = root / 'target'; deps = target / 'debug/deps'; deps.mkdir(parents=True)
        (tree / 'Cargo.toml').write_text('[package]\nname="fixture"\nversion="0.0.0"\n[lib]\npath="src/lib.rs"\n[[bin]]\nname="fixture"\npath="src/main.rs"\n')
        for rel in ['src/lib.rs', 'src/main.rs', 'tests/generated.rs', 'tests/disabled.rs']:
            (tree / rel).write_text('// synthetic source only\n')
        source = {str(p.relative_to(tree)): aid.sha(p) for p in tree.rglob('*') if p.is_file()}
        source_path = root / 'source.json'; source_sha = dump(source_path, source)
        commands = json.loads((HERE.parent / 'final-gates-v6/commands.json').read_text())
        expected = {'source': str(tree), 'tiers': {tier: commands[stage].get('features', []) for stage, tier in aid.STAGES.items()},
                    'targets': [{'target': name, 'source': 'tests/' + name + '.rs', 'sha256': source['tests/' + name + '.rs'],
                                 'tiers': {tier: {'minimum_passed': int(name == 'generated')} for tier in aid.STAGES.values()}}
                                for name in ['generated', 'disabled']]}
        expected_path = root / 'expected.json'; expected_sha = dump(expected_path, expected)
        binaries = []
        for name in ['lib', 'main', 'generated', 'disabled']:
            binary = deps / name; binary.write_bytes(b'\x7fELFsynthetic-never-executed-' + name.encode()); binary.chmod(0o700)
            binaries.append({'path': str(binary), 'sha256': aid.sha(binary)})
        receipt_path = root / 'receipt.json'; log_path = root / 'run.log'
        request = {'tree': str(tree), 'target_root': str(target), 'receipt': str(receipt_path), 'source_manifest': str(source_path), 'source_sha': source_sha,
                   'expected_manifest': str(expected_path), 'expected_sha': expected_sha, 'bundle': str(HERE.parent / 'final-gates-v6')}
        for stage in aid.STAGES:
            units = [('unittests src/lib.rs', 'lib', ['generated_macro::case_0', 'generated_macro::case_1'])]
            if stage != 'lib': units += [('unittests src/main.rs', 'main', []), ('tests/generated.rs', 'generated', ['macro_case']), ('tests/disabled.rs', 'disabled', [])]
            text = ''; results = {}
            for label, name, names in units:
                text += f'Running {label} ({deps / name})\nrunning {len(names)} tests\n'
                text += ''.join('test ' + n + ' ... ok\n' for n in names)
                text += f'test result: ok. {len(names)} passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s\n'
                key = label if label.startswith('unittests ') else Path(label).stem
                results[key] = {'status': 'ok', 'passed': len(names), 'failed': 0, 'ignored': 0}
            log_path.write_text(text)
            features = commands[stage].get('features', [])
            receipt = {'stage': stage, 'source_manifest_sha256': source_sha, 'expected_manifest_sha256': expected_sha, 'features': features,
                       'manifest_inputs': {name: {'sha256_before': digest, 'sha256_after': digest, 'unchanged': True} for name, digest in [('source', source_sha), ('expected', expected_sha)]},
                       'failed': False, 'environment': {}, 'commands': [{'argv': commands[stage]['commands'][0], 'features': {'explicit': features}, 'log': str(log_path),
                           'results': results, 'binaries': [b for b in binaries if Path(b['path']).name in [name for _, name, _ in units]],
                           'exit': 0, 'timed_out': False, 'completeness_problems': []}]}
            request['receipt_sha'] = dump(receipt_path, receipt)
            yes('full v5 schema/native path link ' + stage, lambda: aid.context(request, set(source))['stage'] == stage)
        daemon = target / 'debug/heliosdb-nano'; daemon.write_bytes(b'\x7fELFsynthetic-daemon-never-executed')
        receipt['commands'][0]['binaries'].append({'path': str(daemon), 'sha256': aid.sha(daemon), 'role': 'Cargo daemon path'})
        request['receipt_sha'] = dump(receipt_path, receipt)
        ctx = aid.context(request, set(source))
        yes('daemon receipt bytes pinned but never selected for native listing', lambda: str(daemon) in ctx['pins'] and all(r['binary'] != str(daemon) for r in ctx['records']))
        before = daemon.read_bytes(); daemon.write_bytes(b'mutated-daemon')
        no('daemon evidence mutation after intake', lambda: aid.pin_check(ctx['pins']))
        daemon.write_bytes(before)
        # Exercise real context rejection paths, not a parallel imitation.
        for label, mutate in [
            ('wrong v5 source SHA', lambda r: r.update(source_manifest_sha256='0' * 64)),
            ('failed original manifest guard', lambda r: r['manifest_inputs']['source'].update(unchanged=False)),
            ('different executed argv', lambda r: r['commands'][0]['argv'].append('--ignored')),
            ('different final summary', lambda r: r['commands'][0]['results']['generated'].update(passed=2)),
            ('duplicate native hash', lambda r: r['commands'][0]['binaries'].append(copy.deepcopy(r['commands'][0]['binaries'][0]))),
            ('wrong native hash', lambda r: r['commands'][0]['binaries'][0].update(sha256='0' * 64)),
            ('unknown binary artifact role', lambda r: r['commands'][0]['binaries'][-1].update(role='unrecognized')),
            ('v5 feature mismatch', lambda r: r.update(features=[]))]:
            changed = copy.deepcopy(receipt); mutate(changed); request['receipt_sha'] = dump(receipt_path, changed)
            no(label, lambda: aid.context(request, set(source)))
        request['receipt_sha'] = dump(receipt_path, receipt)
        original_log = log_path.read_text()
        log_path.write_text(original_log.split('Running tests/disabled.rs')[0])
        no('missing cfg-zero Cargo target is still rejected', lambda: aid.context(request, set(source)))
        log_path.write_text(original_log)
        expected_missing = copy.deepcopy(expected); expected_missing['targets'] = expected_missing['targets'][:-1]
        request['expected_sha'] = dump(expected_path, expected_missing)
        changed = copy.deepcopy(receipt); changed['expected_manifest_sha256'] = request['expected_sha']; changed['manifest_inputs']['expected'] = {'sha256_before': request['expected_sha'], 'sha256_after': request['expected_sha'], 'unchanged': True}
        request['receipt_sha'] = dump(receipt_path, changed)
        no('independent source target census rejects omitted expected target', lambda: aid.context(request, set(source)))


def check_critical():
    required = json.loads((HERE / 'critical-expectations.json').read_text())
    records = {}; source = {}
    for probe in required['probes']:
        source[probe['source']] = probe['source_sha256']
        row = records.setdefault(probe['target'], {'target': probe['target'], 'observed_names': [], 'native_names': [], 'native_ignored_names': [], 'body': '', 'summary': {'failed': 0}})
        row['observed_names'] += probe['names']; row['native_names'] += probe['names']; row['body'] += '\n'.join(probe['markers']) + '\n'
    r57 = required['r57']; source['tests/row_shape.rs'] = r57['source_sha256']
    records['row_shape'] = {'target': 'row_shape', 'observed_names': r57['names'], 'native_names': r57['names'], 'native_ignored_names': [], 'body': '', 'summary': {'failed': 0}}
    ctx = {'source': source, 'stage': 'core-internal', 'tier': 'internal', 'records': list(records.values())}
    yes('all five final source28 critical families and five R57 parents', lambda: aid.critical_checks(ctx)['pass'])
    for probe in required['probes']:
        for marker in probe['markers']:
            altered = copy.deepcopy(ctx)
            row = next(r for r in altered['records'] if r['target'] == probe['target']); row['body'] = row['body'].replace(marker + '\n', '', 1)
            no('missing critical marker ' + marker, lambda altered=altered: aid.critical_checks(altered)['pass'])
        for label, edit in [('duplicate', lambda r, m: r.update(body=r['body'] + m + '\n')),
                            ('unknown', lambda r, m: r.update(body=r['body'] + m.split()[0] + ' unknown=1\n'))]:
            altered = copy.deepcopy(ctx); row = next(r for r in altered['records'] if r['target'] == probe['target'])
            edit(row, next(iter(probe['markers'])))
            no(label + ' critical family ' + probe['source'], lambda altered=altered: aid.critical_checks(altered)['pass'])
        altered = copy.deepcopy(ctx); row = next(r for r in altered['records'] if r['target'] == probe['target']); row['native_ignored_names'] += probe['names'][:1]
        no('critical parent cannot silently become ignored ' + probe['source'], lambda altered=altered: aid.critical_checks(altered)['pass'])
    for name in r57['names']:
        altered = copy.deepcopy(ctx); altered['records'][-1]['native_ignored_names'] = [name]
        no('R57 parent cannot be ignored ' + name, lambda altered=altered: aid.critical_checks(altered)['pass'])
    altered = copy.deepcopy(ctx); altered['source'][required['probes'][0]['source']] = '0' * 64
    no('critical source expectations must be refreshed after fixture changes', lambda: aid.critical_checks(altered))


if __name__ == '__main__':
    parser = argparse.ArgumentParser(); parser.add_argument('--out', required=True); args = parser.parse_args()
    run_checks()
    aid.write_json(args.out, {'kind': 'synthetic_static_selfcheck', 'pass': True, 'positive_cases': len(passed), 'rejected_cases': len(rejected),
                   'positive_labels': passed, 'rejection_labels': rejected, 'child_processes_executed': 0, 'test_bodies_executed': False,
                   'release_acceptance_pass': False, 'source_sha256': {'adjudicate.py': aid.sha(HERE / 'adjudicate.py'), 'selfcheck.py': aid.sha(__file__), 'critical-expectations.json': aid.sha(HERE / 'critical-expectations.json')}})
    print(json.dumps({'static_only': True, 'positive_cases': len(passed), 'rejected_cases': len(rejected), 'child_processes_executed': 0}))
