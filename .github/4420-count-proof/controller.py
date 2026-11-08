#!/usr/bin/env python3
"""Exact five-parent COUNT publication proof; no broad-suite/release acceptance."""
import argparse
import fcntl
import hashlib
import json
import os
from pathlib import Path
import re
import signal
import shutil
import subprocess
import time

HERE = Path(__file__).resolve().parent
FILTER = 'storage::art_manager::count_publication_tests'
PARENTS = {
    'public_sql_counts_never_publish_an_uncommitted_eager_prefix': 24,
    'completed_sql_writers_invalidate_the_entire_cardinality_probe': 24,
    'direct_single_and_batch_claims_cover_the_first_eager_key': 24,
    'insertion_census_preserves_unrelated_counts_and_index_miss_gates': 12,
    'parallel_statements_cannot_overtake_their_shared_raii_enrollment': 4,
}
NAMES = {FILTER + '::' + name for name in PARENTS}
FEATURES = ['default', 'encryption', 'ha-tier1', 'ring-crypto', 'vector-search']
ENV_KEYS = ('RUSTUP_TOOLCHAIN', 'CARGO_HOME', 'RUSTUP_HOME', 'CARGO_TARGET_DIR',
            'CARGO_PROFILE_TEST_DEBUG', 'CARGO_PROFILE_DEV_DEBUG', 'CARGO_BUILD_JOBS',
            'CMAKE_BUILD_PARALLEL_LEVEL', 'RUST_TEST_THREADS', 'RUSTFLAGS',
            'CARGO_ENCODED_RUSTFLAGS', 'CARGO_INCREMENTAL', 'RUSTC', 'RUSTDOC', 'HELIOS_STATEMENT_OFFLOAD', 'CC', 'CXX', 'AR', 'LIBCLANG_PATH')


def require(condition, message):
    if not condition:
        raise ValueError(message)


def digest(path):
    value = hashlib.sha256()
    with path.open('rb') as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b''):
            value.update(block)
    return value.hexdigest()


def write(path, value):
    path.write_text(json.dumps(value, indent=2, sort_keys=True) + '\n')


def read_command(argv, cwd, env):
    return subprocess.check_output(argv, cwd=cwd, env=env, timeout=30)


def source(tree, expected, head, auxiliary, env):
    def git(*args):
        return read_command(['git', '-C', str(tree), *args], tree, env)
    require(git('rev-parse', 'HEAD').decode().strip() == head, 'checkout HEAD mismatch')
    require(not git('diff', '--name-only', 'HEAD') and not git('ls-files', '--others', '--exclude-standard'), 'checkout moved/is dirty')
    entries = {}
    for row in git('ls-tree', '-rz', '--full-tree', 'HEAD').split(b'\0'):
        if not row:
            continue
        info, name = row.split(b'\t', 1)
        mode, kind, oid = info.decode().split()
        require(kind == 'blob', 'unexpected non-blob input')
        entries[name.decode()] = {'mode': mode, 'oid': oid}
    require(set(entries) == set(expected) | set(auxiliary), 'complete source path set differs')
    physical = {}
    for name, record in expected.items():
        require(entries[name] == {key: record[key] for key in ('mode', 'oid')}, 'Git input differs: ' + name)
        path = tree / name
        if record['mode'] == '120000':
            require(path.is_symlink() and os.readlink(path) == record['target_text'], 'Git symlink text differs: ' + name)
        else:
            require(path.is_file() and not path.is_symlink() and digest(path) == record['sha256'], 'physical source differs: ' + name)
            physical[name] = record['sha256']
    aux = {}
    for name in auxiliary:
        path = tree / name
        require(entries[name]['mode'] == '100644' and not path.is_symlink(), 'aux mode differs: ' + name)
        require(path.read_bytes() == git('show', 'HEAD:' + name), 'aux bytes differ: ' + name)
        aux[name] = digest(path)
    return {'head': head, 'entries': entries, 'regular_sha256': physical,
            'auxiliary_sha256': aux, 'canonical_source_pass': True}


def birth(pid):
    return int(Path('/proc/%d/stat' % pid).read_text().rsplit(')', 1)[1].split()[19])


def run_owned(argv, tree, env, output, stem, timeout, native_sha=None):
    record = {'argv': argv, 'cwd': str(tree), 'timeout_seconds': timeout,
              'timed_out': False, 'leader_reaped': False,
              'descendant_cleanup_owner': 'bounded systemd service KillMode=control-group'}
    process = None
    ticks = None
    failure = None
    with (output / (stem + '.stdout')).open('xb') as stdout, (output / (stem + '.stderr')).open('xb') as stderr:
        try:
            process = subprocess.Popen(argv, cwd=tree, env=env, stdout=stdout, stderr=stderr, start_new_session=True)
            # Very short listing commands may already have exited. A live
            # leader must have a recorded birth identity before any signal.
            try:
                ticks = birth(process.pid)
            except FileNotFoundError:
                require(process.poll() is not None, 'live child birth identity unavailable')
            record.update(pid=process.pid, birth_ticks=ticks, owned_process_group=process.pid)
            if native_sha is not None:
                require(ticks is not None and process.poll() is None, 'native exited before actual image observation')
                actual_exe = Path('/proc/%d/exe' % process.pid)
                actual_argv = Path('/proc/%d/cmdline' % process.pid).read_bytes().split(b'\0')[:-1]
                require(actual_exe.resolve() == Path(argv[0]).resolve() and digest(actual_exe) == native_sha,
                        'actual owned native image differs')
                require(actual_argv == [os.fsencode(arg) for arg in argv], 'actual native argv differs')
                record['native_image'] = {'exe': str(actual_exe.resolve()), 'sha256': native_sha,
                                          'argv': argv, 'birth_ticks': ticks}

            try:
                process.wait(timeout=timeout)
            except subprocess.TimeoutExpired:
                record['timed_out'] = True
                require(process.poll() is None and ticks is not None and birth(process.pid) == ticks
                        and os.getpgid(process.pid) == process.pid, 'owned timeout identity changed')
                os.killpg(process.pid, signal.SIGTERM)
                try:
                    process.wait(timeout=15)
                except subprocess.TimeoutExpired:
                    require(birth(process.pid) == ticks and os.getpgid(process.pid) == process.pid,
                            'owned kill identity changed')
                    os.killpg(process.pid, signal.SIGKILL)
                    process.wait(timeout=10)
        except BaseException as error:
            failure = error
        finally:
            if process is not None:
                try:
                    if process.poll() is None:
                        require(ticks is not None and birth(process.pid) == ticks
                                and os.getpgid(process.pid) == process.pid, 'final cleanup identity changed')
                        os.killpg(process.pid, signal.SIGKILL)
                        process.wait(timeout=10)
                    record.update(exit_code=process.returncode, leader_reaped=process.poll() is not None)
                except BaseException as error:
                    failure = failure or error
            if failure is not None:
                record['error'] = {'type': type(failure).__name__, 'message': str(failure)}
    record.update(stdout_sha256=digest(output / (stem + '.stdout')), stderr_sha256=digest(output / (stem + '.stderr')))
    write(output / (stem + '-receipt.json'), record)
    if failure is not None:
        raise failure
    return record


def tool_identity(tree, env):
    result = {}
    for name in ('rustc', 'cargo'):
        path = Path(read_command(['rustup', 'which', name], tree, env).decode().strip())
        version = read_command([str(path), '-vV' if name == 'rustc' else '--version'], tree, env).decode()
        require(name + ' 1.95.0 ' in version, 'wrong compiler/Cargo version')
        result[name] = {'path': str(path), 'sha256': digest(path), 'version': version}
    return result


def config_identity(tree, env):
    paths = {directory / '.cargo' / name for directory in (tree, *tree.parents)
             for name in ('config', 'config.toml')}
    paths.update(Path(env['CARGO_HOME']) / name for name in ('config', 'config.toml'))
    return {str(path): {'exists': path.exists(), 'symlink': path.is_symlink(),
                        'target': os.readlink(path) if path.is_symlink() else None,
                        'sha256': digest(path) if path.is_file() else None}
            for path in sorted(paths)}


def expected_cells():
    values = {}
    for family in ('public', 'aba', 'direct'):
        writers = ('Single', 'Batch') if family == 'direct' else ('Text', 'Params')
        values[family] = {(history, writer, refused, form)
                          for history in ('false', 'true') for writer in writers
                          for refused in ('false', 'true') for form in ('Bare', 'Range', 'In')}
    values['unrelated'] = {(h, w, f) for h in ('false', 'true')
                           for w in ('Text', 'Params') for f in ('Bare', 'Range', 'In')}
    values['shared_raii'] = {(h, r) for h in ('false', 'true') for r in ('false', 'true')}
    return values


def cells(text):
    specs = {
        'public': r'history=(false|true) family=(Text|Params) refused=(false|true) form=(Bare|Range|In)',
        'aba': r'history=(false|true) writer_family=(Text|Params) refused=(false|true) form=(Bare|Range|In)',
        'direct': r'history=(false|true) writer=(Single|Batch) refused=(false|true) form=(Bare|Range|In)',
        'unrelated': r'history=(false|true) writer_family=(Text|Params) form=(Bare|Range|In)',
        'shared_raii': r'history=(false|true) rollback=(false|true)',
    }
    result = {}
    count = 0
    for family, spec in specs.items():
        rows = [match.groups() for match in re.finditer(
            r'count-publication-cell ' + family + ' ' + spec + r' ok=(true|false)(?=\s|$)', text)]
        require(len({row[:-1] for row in rows}) == len(rows), 'duplicate case: ' + family)
        result[family] = {row[:-1]: row[-1] == 'true' for row in rows}
        count += len(rows)
    require(text.count('count-publication-cell') == count, 'malformed/unknown case marker')
    require({family: set(rows) for family, rows in result.items()} == expected_cells(),
            'missing/unexpected exact native case identity')
    return result


def judge(text, role, code):
    summaries = re.findall(r'test result: (ok|FAILED)\. (\d+) passed; (\d+) failed; (\d+) ignored; (\d+) measured; (\d+) filtered out', text)
    require(len(summaries) == 1, 'exactly one native Rust summary required')
    status, passed, failed, ignored, measured, filtered = summaries[0]
    actual = dict(passed=int(passed), failed=int(failed), ignored=int(ignored),
                  measured=int(measured), filtered=int(filtered))
    require(actual['passed'] + actual['failed'] == 5 and actual['ignored'] == actual['measured'] == 0,
            'wrong/empty/ignored native parent inventory')
    observed = cells(text)
    failed_cells = {family: sum(not ok for ok in rows.values()) for family, rows in observed.items()}
    parents = dict(zip(('public', 'aba', 'direct', 'unrelated', 'shared_raii'), PARENTS))
    failed_names = {FILTER + '::' + parents[family] for family, count in failed_cells.items() if count}
    require(actual['failed'] == len(failed_names), 'parent summary disagrees with case failures')
    if failed_names:
        block = text.rsplit('\nfailures:\n', 1)[-1].split('\ntest result:', 1)[0]
        actual_failed = {line.strip() for line in block.splitlines() if line.strip().startswith(FILTER + '::')}
        require(actual_failed == failed_names, 'unexpected failing parent')
    if role == 'fixed':
        require(code == 0 and status == 'ok' and actual['failed'] == 0
                and not any(failed_cells.values()), 'fixed requires all5 parents/all88 cases green')
        outcome = 'focused_fixed_count_publication_pass'
        evidence = {}
    else:
        require(code == 101 and status == 'FAILED' and actual['failed'] > 0,
                'baseline must expose actual native regression')
        partial = [int(value) for value in re.findall(r'torn public COUNT: (\d+)', text)]
        require(any(0 < value < 12 for value in partial) and failed_cells['public'] > 0,
                'baseline lacks genuine partial COUNT after realART/raw0 and join oracles')
        require('second statement passed unpublished enrollment: Ok(())' in text
                and failed_cells['shared_raii'] > 0,
                'baseline lacks actual second-writer progress while shared enrollment paused')
        require(failed_cells['unrelated'] == 0, 'unrelated table/index-miss controls must remain healthy')
        evidence = {'observed_partial_public_counts': partial,
                    'real_art_raw_zero_sequence_premise_checked_before_public_assertion': True,
                    'actual_shared_entry_and_second_art_progress_before_first_release': True,
                    'baseline_pass_fail_counts_are_observed_not_predicted': True}
        outcome = 'validated_expected_native_count_publication_failure'
    return {'status': outcome, 'native_summary': actual, 'native_case_count': 88,
            'case_family_counts': {family: len(rows) for family, rows in observed.items()},
            'failed_case_counts': failed_cells, 'regression_evidence': evidence,
            'full_suite_or_release_acceptance': False}


def native_list(text):
    lines = text.splitlines()
    require(all(line.endswith(': test') for line in lines), 'unknown/benchmark native listing line')
    names = [line[:-6] for line in lines]
    require(len(names) == len(set(names)), 'duplicate native listed name')
    return set(names)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--role', choices=['baseline', 'fixed'], required=True)
    parser.add_argument('--config', choices=['default', 'internal-tests'], required=True)
    parser.add_argument('--product', type=Path, required=True)
    parser.add_argument('--controller', type=Path, required=True)
    parser.add_argument('--controller-head', help='explicit checkout head for root-owned local runs')
    parser.add_argument('--out', type=Path, required=True)
    parser.add_argument('--model-sha256', required=True)
    args = parser.parse_args()
    product, control, output = args.product.resolve(), args.controller.resolve(), args.out.resolve()
    require(not output.is_relative_to(product) and not output.is_relative_to(control), 'output/target escapes immutable ownership')
    output.mkdir(parents=True, exist_ok=False)
    result = {'role': args.role, 'config': args.config, 'status': 'invalid',
              'focused_pair_role_valid': False, 'release_acceptance_pass': False, 'errors': []}
    env = os.environ.copy()
    # An inherited native-test subprocess mode must never turn --list into a body.
    for key in tuple(env):
        if key.startswith('__RUST_TEST_'):
            env.pop(key)
    env.update(RUSTUP_TOOLCHAIN='1.95.0', CARGO_BUILD_JOBS='2', CMAKE_BUILD_PARALLEL_LEVEL='2',
               CARGO_PROFILE_TEST_DEBUG='0', CARGO_PROFILE_DEV_DEBUG='0', RUST_TEST_THREADS='2',
               CARGO_INCREMENTAL='0', CARGO_TERM_COLOR='never', CARGO_TARGET_DIR=str(output / 'fresh-target'),
               HELIOS_STATEMENT_OFFLOAD='on')
    before = model = fleet = None
    started = time.monotonic()
    head = args.controller_head or env.get('GITHUB_SHA')
    try:
        require(head is not None and re.fullmatch(r'[0-9a-f]{40}', head), 'explicit actual controller head required')
        require(digest(HERE / 'model.json') == args.model_sha256, 'model bootstrap mismatch')
        model = json.loads((HERE / 'model.json').read_text())
        require(model['roles'] == model['source_pair']['heads'] and model['configs'] == ['default', 'internal-tests'], 'source-pair roles/configs changed')
        require(model['parents'] == 5 and model['cases'] == 88, 'source-pair counts changed')
        require(set(model['entries']['baseline']) == set(model['entries']['fixed']), 'pair path sets differ')
        changes = {name for name in model['entries']['baseline'] if model['entries']['baseline'][name] != model['entries']['fixed'][name]}
        require(changes == set(model['source_pair']['only_differing_paths']) == {'src/lib.rs', 'src/storage/engine.rs', 'src/storage/transaction.rs'}, 'unexpected production difference')
        for name, value in model['source_pair']['identical_native_tests_and_manager_seams'].items():
            require(all(model['entries'][role][name]['sha256'] == value for role in ('baseline', 'fixed')), 'native fixture/seam mismatch')
        require(not any(env.get(key) for key in ('RUSTFLAGS', 'CARGO_ENCODED_RUSTFLAGS', 'RUSTC', 'RUSTDOC', 'RUSTC_WRAPPER', 'RUSTC_WORKSPACE_WRAPPER')), 'undeclared compiler override')
        group = next(line.split(':', 2)[2] for line in Path('/proc/self/cgroup').read_text().splitlines() if line.startswith('0::'))
        cg = Path('/sys/fs/cgroup') / group.lstrip('/')
        limits = {name: (cg / name).read_text().strip() for name in ('memory.max', 'memory.swap.max')}
        require(limits == {'memory.max': str(24 * 1024**3), 'memory.swap.max': '0'}, '24GiB/no-swap service required')
        fleet = Path('/home/gpc/HDB/sprint/coordination/build.lock').open('r+')
        fcntl.flock(fleet, fcntl.LOCK_EX | fcntl.LOCK_NB)
        before = {'product': source(product, model['entries'][args.role], model['roles'][args.role], [], env),
                  'controller': source(control, model['controller_entries'], head, model['auxiliary_paths'], env),
                  'cargo_configs': config_identity(product, env)}
        write(output / 'source-before.json', before)
        tools = tool_identity(product, env)
        env['RUSTC'] = tools['rustc']['path']
        env['RUSTDOC'] = read_command(['rustup', 'which', 'rustdoc'], product, env).decode().strip()
        write(output / 'host.json', {'tools': tools, 'rustdoc_sha256': digest(Path(env['RUSTDOC'])),
              'cgroup': group, 'limits': limits, 'uname': list(os.uname()),
              'environment': {key: env.get(key) for key in ENV_KEYS},
              'source_pair_sha256': model['source_pair_sha256'], 'controller_head': head})
        require(not Path(env['CARGO_TARGET_DIR']).exists(), 'target must be genuinely fresh')
        def remaining(maximum):
            value = min(maximum, 5400 - (time.monotonic() - started))
            require(value > 0, 'shared build/list/native5400s deadline expired')
            return value
        argv = ['cargo', 'test', '--locked', '--lib']
        if args.config == 'internal-tests':
            argv += ['--features', 'internal-tests']
        built = run_owned(argv + ['--no-run', '--message-format=json'], product, env, output, 'build', remaining(4500))
        require(built['exit_code'] == 0 and not built['timed_out'], 'native compile failed/timed out')
        artifacts = []
        for line in (output / 'build.stdout').read_text().splitlines():
            message = json.loads(line)
            if message.get('reason') == 'compiler-artifact' and message.get('executable') and message.get('profile', {}).get('test') and 'lib' in message.get('target', {}).get('kind', []):
                artifacts.append(message)
        require(len(artifacts) == 1, 'one actual compiler-owned libtest required')
        artifact = artifacts[0]
        features = FEATURES + (['internal-tests'] if args.config == 'internal-tests' else [])
        require(sorted(artifact['features']) == sorted(features) and artifact.get('fresh') is False,
                'wrong features/stale compiled native artifact')
        require(artifact['profile']['debuginfo'] == 0 and str(artifact['profile']['opt_level']) == '0', 'actual test profile must be opt0/debug0')
        require(artifact['target']['name'] == 'heliosdb_nano' and Path(artifact['manifest_path']).resolve() == product / 'Cargo.toml' and Path(artifact['target']['src_path']).resolve() == product / 'src/lib.rs', 'wrong compiled product')
        binary = Path(artifact['executable']).resolve()
        require(binary.is_relative_to(Path(env['CARGO_TARGET_DIR'])) and binary.is_file(), 'native ELF escapes own fresh target')
        with binary.open('rb') as stream:
            require(stream.read(6) == b'\x7fELF\x02\x01', 'actual native must be Linux64 little-endian ELF')
        binary_sha = digest(binary)
        shutil.copyfile(binary, output / 'native-libtest.elf')
        require(digest(output / 'native-libtest.elf') == binary_sha, 'preserved native copy differs')
        write(output / 'native-build-receipt.json', {'compiler_artifact': artifact, 'binary': str(binary),
              'binary_sha256': binary_sha, 'tools': tools, 'build': built,
              'role_head': model['roles'][args.role], 'config': args.config})
        for ignored in (False, True):
            stem = 'native-list-ignored' if ignored else 'native-list'
            listed = run_owned([str(binary), FILTER, '--list', '--format', 'terse'] + (['--ignored'] if ignored else []), product, env, output, stem, remaining(30))
            require(listed['exit_code'] == 0 and not listed['timed_out'], 'native listing failed')
            require(native_list((output / (stem + '.stdout')).read_text()) == (set() if ignored else NAMES), 'exact5/unignored native inventory differs')
            require(digest(binary) == binary_sha, 'native image changed while listing')
        runtime = run_owned([str(binary), FILTER, '--nocapture', '--test-threads=2'], product, env, output, 'native', remaining(600), native_sha=binary_sha)
        require(not runtime['timed_out'] and runtime['leader_reaped'], 'native timed out/not reaped')
        require(digest(binary) == binary_sha, 'native ELF changed after run')
        text = (output / 'native.stdout').read_text(errors='replace') + '\n' + (output / 'native.stderr').read_text(errors='replace')
        judged = judge(text, args.role, runtime['exit_code'])
        require(tool_identity(product, env) == tools, 'compiler/Cargo identity changed')
        host = json.loads((output / 'host.json').read_text())
        require(digest(Path(env['RUSTDOC'])) == host['rustdoc_sha256'], 'rustdoc identity changed')
        write(output / 'native-adjudication.json', judged)
        result.update(judged)
        result.update(focused_pair_role_valid=True, native_binary_sha256=binary_sha)
    except BaseException as error:
        result['errors'].append({'type': type(error).__name__, 'message': str(error)})
    finally:
        if model is not None:
            try:
                after = {'product': source(product, model['entries'][args.role], model['roles'][args.role], [], env),
                         'controller': source(control, model['controller_entries'], head, model['auxiliary_paths'], env),
                         'cargo_configs': config_identity(product, env)}
                write(output / 'source-after.json', after)
                require(before is not None and before == after, 'before/after source/config proof differs')
            except BaseException as error:
                result['errors'].append({'phase': 'final-source-guard', 'type': type(error).__name__, 'message': str(error)})
                result['focused_pair_role_valid'] = False
                result['status'] = 'invalid'
        result['elapsed_seconds'] = time.monotonic() - started
        write(output / 'result.json', result)
    return 0 if result['focused_pair_role_valid'] else 1


if __name__ == '__main__':
    raise SystemExit(main())
