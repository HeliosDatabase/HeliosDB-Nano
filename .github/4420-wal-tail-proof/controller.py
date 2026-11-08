#!/usr/bin/env python3
"""Fixed four-test native WAL-tail baseline/fix proof; no broad-suite or release acceptance."""
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
FILTER = 'replication::physical::physical_wal_tail_tests'
NATIVE = FILTER + '::a_real_native_wal_rotation_requests_a_fresh_iterator'
COMPLETE = FILTER + '::a_completed_poll_keeps_its_sampled_barrier_after_real_wal_rotation'
PARTIAL = FILTER + '::a_byte_limited_prefix_survives_real_wal_rotation_without_skipping_a_group'
EMPTY = FILTER + '::no_validated_prefix_preserves_real_try_again_and_the_interior_cursor_refusal'
NAMES = {NATIVE, COMPLETE, PARTIAL, EMPTY}
MARKERS = {'native_rotation', 'completed_barrier', 'bounded_prefix', 'no_prefix_refusal'}
FEATURES = ['default', 'encryption', 'ha-tier1', 'ring-crypto', 'vector-search']
ENV_KEYS = ('RUSTUP_TOOLCHAIN', 'CARGO_HOME', 'RUSTUP_HOME', 'CARGO_TARGET_DIR',
            'CARGO_PROFILE_TEST_DEBUG', 'CARGO_PROFILE_DEV_DEBUG', 'CARGO_BUILD_JOBS',
            'CMAKE_BUILD_PARALLEL_LEVEL', 'RUST_TEST_THREADS', 'RUSTFLAGS',
            'CARGO_ENCODED_RUSTFLAGS', 'CC', 'CXX', 'AR', 'LIBCLANG_PATH')


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


def run_owned(argv, tree, env, output, stem, timeout):
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


def judge(text, role, code):
    summaries = re.findall(r'test result: (ok|FAILED)\. (\d+) passed; (\d+) failed; (\d+) ignored; (\d+) measured; (\d+) filtered out', text)
    require(len(summaries) == 1, 'one actual native summary required')
    status, passed, failed, ignored, measured, filtered = summaries[0]
    actual = dict(passed=int(passed), failed=int(failed), ignored=int(ignored), measured=int(measured), filtered=int(filtered))
    require(actual['ignored'] == 0 and actual['measured'] == 0 and actual['passed'] + actual['failed'] == 4,
            'four nonempty actual native parents required')
    cells = re.findall(r'physical-wal-tail-cell ([a-z_]+)', text)
    require(text.count('physical-wal-tail-cell') == len(cells), 'malformed native cell marker')
    require(len(cells) == len(set(cells)), 'duplicate actual cell marker')
    if role == 'baseline':
        require(code == 101 and status == 'FAILED' and (actual['passed'], actual['failed']) == (2, 2),
                'baseline must show exactly two native failures and two passes')
        require(set(cells) == {'native_rotation', 'no_prefix_refusal'}, 'baseline native premises/negative control incomplete')
        require('Error: Storage(' in text and 'Operation failed. Try again.' in text
                and 'Create a new iterator to fetch the new tail.' in text,
                'baseline lacks the actual native iterator tail error')
        failure_block = text.rsplit('\nfailures:\n', 1)[-1].split('\ntest result:', 1)[0]
        names = {line.strip() for line in failure_block.splitlines() if line.strip().startswith(FILTER + '::')}
        require(names == {COMPLETE, PARTIAL}, 'unexpected baseline failing parent')
        outcome = 'validated_expected_native_tail_failure'
    else:
        require(code == 0 and status == 'ok' and (actual['passed'], actual['failed']) == (4, 0),
                'fixed must pass all four actual native parents')
        require(set(cells) == MARKERS, 'four actual native rotation/prefix/cursor cells incomplete')
        outcome = 'focused_fixed_wal_tail_regression_pass'
    return {'status': outcome, 'native_summary': actual, 'completed_cells': sorted(cells),
            'expected_four_parent_proof': True, 'full_suite_or_release_acceptance': False}


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--role', choices=['baseline', 'fixed'], required=True)
    parser.add_argument('--product', type=Path, required=True)
    parser.add_argument('--controller', type=Path, required=True)
    parser.add_argument('--out', type=Path, required=True)
    parser.add_argument('--model-sha256', required=True)
    args = parser.parse_args()
    product, control, output = args.product.resolve(), args.controller.resolve(), args.out.resolve()
    require(not output.is_relative_to(product) and not output.is_relative_to(control), 'output/target must be outside immutable checkouts')
    output.mkdir(parents=True, exist_ok=False)
    result = {'role': args.role, 'status': 'invalid', 'focused_pair_role_valid': False,
              'release_acceptance_pass': False, 'errors': []}
    env = os.environ.copy()
    env.update(RUSTUP_TOOLCHAIN='1.95.0', CARGO_BUILD_JOBS='2', CMAKE_BUILD_PARALLEL_LEVEL='2',
               CARGO_PROFILE_TEST_DEBUG='0', CARGO_PROFILE_DEV_DEBUG='0', RUST_TEST_THREADS='2',
               CARGO_TERM_COLOR='never', CARGO_TARGET_DIR=str(output / 'fresh-target'))
    before = None
    model = None
    fleet = None
    started = time.monotonic()
    try:
        require(digest(HERE / 'model.json') == args.model_sha256, 'model bootstrap mismatch')
        model = json.loads((HERE / 'model.json').read_text())
        require(set(model['roles']) == {'baseline', 'fixed'}, 'paired roles changed')
        require(set(model['entries']['baseline']) == set(model['entries']['fixed']), 'pair path sets differ')
        require(not env.get('RUSTFLAGS') and not env.get('CARGO_ENCODED_RUSTFLAGS'), 'undeclared extra Rust flags')
        changes = {name for name in model['entries']['baseline'] if model['entries']['baseline'][name] != model['entries']['fixed'][name]}
        require(changes == {'src/replication/physical.rs'}, 'pair differs beyond production correction')
        group = next(line.split(':', 2)[2] for line in Path('/proc/self/cgroup').read_text().splitlines() if line.startswith('0::'))
        cg = Path('/sys/fs/cgroup') / group.lstrip('/')
        limits = {name: (cg / name).read_text().strip() for name in ('memory.max', 'memory.swap.max')}
        require(limits == {'memory.max': str(24 * 1024**3), 'memory.swap.max': '0'}, '24GiB/no-swap service required')
        fleet = Path('/home/gpc/HDB/sprint/coordination/build.lock').open('r+')
        fcntl.flock(fleet, fcntl.LOCK_EX | fcntl.LOCK_NB)
        head = model['roles'][args.role]
        before = {'product': source(product, model['entries'][args.role], head, [], env),
                  'controller': source(control, model['controller_entries'], os.environ['GITHUB_SHA'], model['auxiliary_paths'], env)}
        write(output / 'source-before.json', before)
        tools = tool_identity(product, env)
        write(output / 'host.json', {'tools': tools, 'cgroup': group, 'limits': limits,
              'uname': list(os.uname()), 'meminfo': Path('/proc/meminfo').read_text(),
              'environment': {key: env.get(key) for key in ENV_KEYS}, 'source_pair_sha256': model['source_pair_sha256']})
        require(not Path(env['CARGO_TARGET_DIR']).exists(), 'target must be genuinely fresh')
        def remaining(maximum):
            value = min(maximum, 3600 - (time.monotonic() - started))
            require(value > 0, 'shared build/native3600s deadline expired')
            return value
        built = run_owned(['cargo', 'test', '--locked', '--lib', '--no-run', '--message-format=json'],
                          product, env, output, 'build', remaining(3000))
        require(built['exit_code'] == 0 and not built['timed_out'], 'native compile failed/timed out')
        artifacts = []
        for line in (output / 'build.stdout').read_text().splitlines():
            message = json.loads(line)
            if message.get('reason') == 'compiler-artifact' and message.get('executable') and message.get('profile', {}).get('test') and 'lib' in message.get('target', {}).get('kind', []):
                artifacts.append(message)
        require(len(artifacts) == 1, 'exactly one actual compiler-owned libtest ELF required')
        artifact = artifacts[0]
        require(sorted(artifact['features']) == FEATURES, 'default feature receipt changed')
        require(artifact['profile']['debuginfo'] == 0, 'test profile must use debug0')
        require(Path(artifact['manifest_path']).resolve() == product / 'Cargo.toml' and Path(artifact['target']['src_path']).resolve() == product / 'src/lib.rs', 'wrong compiled product')
        binary = Path(artifact['executable']).resolve()
        require(binary.is_relative_to(Path(env['CARGO_TARGET_DIR'])) and binary.is_file(), 'native ELF escapes fresh target')
        with binary.open('rb') as stream:
            require(stream.read(6) == b'\x7fELF\x02\x01', 'actual native executable must be Linux64 little-endian ELF')
        binary_sha = digest(binary)
        shutil.copyfile(binary, output / 'native-libtest.elf')
        require(digest(output / 'native-libtest.elf') == binary_sha, 'preserved native ELF copy differs')
        write(output / 'native-build-receipt.json', {'compiler_artifact': artifact, 'binary': str(binary),
              'binary_sha256': binary_sha, 'tools': tools, 'build': built, 'role_head': head})
        listed = run_owned([str(binary), FILTER, '--list', '--format', 'terse'], product, env, output, 'native-list', remaining(60))
        require(listed['exit_code'] == 0 and not listed['timed_out'], 'actual native listing failed')
        names = {line[:-6] for line in (output / 'native-list.stdout').read_text().splitlines() if line.endswith(': test')}
        require(names == NAMES, 'actual native filter must list exactly four expected parents')
        runtime = run_owned(['cargo', 'test', '--locked', '--lib', FILTER, '--', '--nocapture', '--test-threads=2'],
                            product, env, output, 'native', remaining(300))
        require(not runtime['timed_out'] and runtime['leader_reaped'], 'native run timed out/not reaped')
        require(digest(binary) == binary_sha, 'native ELF changed after listing/run')
        text = (output / 'native.stdout').read_text(errors='replace') + '\n' + (output / 'native.stderr').read_text(errors='replace')
        invoked = re.findall(r'Running unittests src/lib\.rs \(([^\n]+)\)', text)
        require(len(invoked) == 1 and (product / invoked[0]).resolve() == binary, 'Cargo runtime did not invoke the listed ELF')
        judged = judge(text, args.role, runtime['exit_code'])
        require(tool_identity(product, env) == tools, 'compiler/Cargo identity changed')
        write(output / 'native-adjudication.json', judged)
        result.update(judged)
        result.update(focused_pair_role_valid=True, native_binary_sha256=binary_sha)
    except BaseException as error:
        result['errors'].append({'type': type(error).__name__, 'message': str(error)})
        result['status'] = 'invalid'
        result['focused_pair_role_valid'] = False
    finally:
        if model is not None:
            try:
                after = {'product': source(product, model['entries'][args.role], model['roles'][args.role], [], env),
                         'controller': source(control, model['controller_entries'], os.environ['GITHUB_SHA'], model['auxiliary_paths'], env)}
                write(output / 'source-after.json', after)
                require(before is not None and before == after, 'before/after source proof differs')
            except BaseException as error:
                result['errors'].append({'phase': 'final-source-guard', 'type': type(error).__name__, 'message': str(error)})
                result['status'] = 'invalid'
                result['focused_pair_role_valid'] = False
        result['elapsed_seconds'] = time.monotonic() - started
        write(output / 'result.json', result)
    return 0 if result['focused_pair_role_valid'] else 1


if __name__ == '__main__':
    raise SystemExit(main())
