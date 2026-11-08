#!/usr/bin/env python3
"""Exact historical-schema and quarantine fixtures; no broad-suite/release acceptance."""
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


def native_names(text):
    rows = text.splitlines()
    require(all(row.endswith(': test') for row in rows), 'malformed or benchmark native listing')
    names = [row[:-6] for row in rows]
    require(len(names) == len(set(names)), 'duplicate native name')
    return set(names)


def judge(text, names, code):
    rows = re.findall(r'test result: (ok|FAILED)\. (\d+) passed; (\d+) failed; (\d+) ignored; (\d+) measured; (\d+) filtered out', text)
    require(len(rows) == 1, 'one actual outer native summary required')
    status, passed, failed, ignored, measured, filtered = rows[0]
    actual = dict(passed=int(passed), failed=int(failed), ignored=int(ignored),
                  measured=int(measured), filtered=int(filtered))
    require(actual['passed'] + actual['failed'] == len(names)
            and actual['ignored'] == actual['measured'] == actual['filtered'] == 0,
            'native count differs from complete unignored target')
    observed = re.findall(r'^test (\S+) \.\.\.', text, re.M)
    require(set(observed) == set(names) and len(observed) == len(names), 'missing/extra native parent result')
    green = code == 0 and status == 'ok' and actual['failed'] == 0
    return {'native_summary': actual, 'exact_native_parent_names': sorted(names),
            'target_count_complete': True, 'target_green': green}


def main():
    parser = argparse.ArgumentParser()
    for name in ('product', 'controller', 'out'):
        parser.add_argument('--' + name, type=Path, required=True)
    parser.add_argument('--controller-head')
    parser.add_argument('--model-sha256', required=True)
    args = parser.parse_args()
    product, control, output = args.product.resolve(), args.controller.resolve(), args.out.resolve()
    require(not output.is_relative_to(product) and not output.is_relative_to(control), 'external own output required')
    output.mkdir(parents=True, exist_ok=False)
    result = {'status': 'invalid', 'focused_fixture_proof_valid': False,
              'full_suite_or_release_acceptance': False, 'errors': [], 'targets': {}}
    env = os.environ.copy()
    for key in tuple(env):
        if key.startswith('__RUST_TEST_') or key == 'RUST_TEST_INVOKE':
            env.pop(key)
    env.update(RUSTUP_TOOLCHAIN='1.95.0', CARGO_BUILD_JOBS='2', CMAKE_BUILD_PARALLEL_LEVEL='2',
               CARGO_PROFILE_TEST_DEBUG='0', CARGO_PROFILE_DEV_DEBUG='0', RUST_TEST_THREADS='1',
               CARGO_INCREMENTAL='0', CARGO_TERM_COLOR='never', HELIOS_STATEMENT_OFFLOAD='on',
               CARGO_TARGET_DIR=str(output / 'fresh-target'))
    before = model = fleet = None
    started = time.monotonic()
    head = args.controller_head or env.get('GITHUB_SHA')
    try:
        require(head is not None and re.fullmatch(r'[0-9a-f]{40}', head), 'actual controller head required')
        require(digest(HERE / 'model.json') == args.model_sha256, 'model bootstrap differs')
        model = json.loads((HERE / 'model.json').read_text())
        require({name: value['count'] for name, value in model['targets'].items()} == {
            'time_travel_integration_tests': 11, 'time_travel_system_views_tests': 12,
            'generic_mutation_indexes': 9}, 'exact three-target/32-parent contract changed')
        for name, target in model['targets'].items():
            require(target['source'] == 'tests/' + name + '.rs'
                    and target['source_sha256'] == model['product_entries'][target['source']]['sha256']
                    and len(set(target['names'])) == target['count'], 'target source/name contract differs')
        require(not any(env.get(key) for key in ('RUSTFLAGS', 'CARGO_ENCODED_RUSTFLAGS', 'RUSTC',
                    'RUSTDOC', 'RUSTC_WRAPPER', 'RUSTC_WORKSPACE_WRAPPER')), 'undeclared compiler override')
        group = next(line.split(':', 2)[2] for line in Path('/proc/self/cgroup').read_text().splitlines() if line.startswith('0::'))
        cg = Path('/sys/fs/cgroup') / group.lstrip('/')
        limits = {name: (cg / name).read_text().strip() for name in ('memory.max', 'memory.swap.max')}
        require(limits == {'memory.max': str(24 * 1024**3), 'memory.swap.max': '0'}, '24GiB/no-swap service required')
        fleet = Path('/home/gpc/HDB/sprint/coordination/build.lock').open('r+')
        fcntl.flock(fleet, fcntl.LOCK_EX | fcntl.LOCK_NB)
        before = {'product': source(product, model['product_entries'], model['product'], [], env),
                  'controller': source(control, model['controller_entries'], head, model['auxiliary_paths'], env),
                  'cargo_configs': config_identity(product, env)}
        write(output / 'source-before.json', before)
        tools = tool_identity(product, env)
        env['RUSTC'] = tools['rustc']['path']
        env['RUSTDOC'] = read_command(['rustup', 'which', 'rustdoc'], product, env).decode().strip()
        rustdoc_sha = digest(Path(env['RUSTDOC']))
        write(output / 'host.json', {'tools': tools, 'rustdoc_sha256': rustdoc_sha, 'cgroup': group,
              'limits': limits, 'uname': list(os.uname()),
              'environment': {key: env.get(key) for key in ENV_KEYS},
              'product_head': model['product'], 'controller_head': head})
        require(not Path(env['CARGO_TARGET_DIR']).exists(), 'genuinely fresh target required')
        def remaining(maximum):
            value = min(maximum, 5400 - (time.monotonic() - started))
            require(value > 0, 'shared build/list/native5400s deadline expired')
            return value
        argv = ['cargo', 'test', '--locked', '--features', 'internal-tests']
        for name in model['targets']:
            argv += ['--test', name]
        built = run_owned(argv + ['--no-run', '--message-format=json'], product, env, output, 'build', remaining(4500))
        require(built['exit_code'] == 0 and not built['timed_out'], 'native compile failed/timed out')
        artifacts = {}
        for line in (output / 'build.stdout').read_text().splitlines():
            message = json.loads(line)
            if message.get('reason') == 'compiler-artifact' and message.get('executable') and message.get('profile', {}).get('test') and 'test' in message.get('target', {}).get('kind', []):
                name = message['target']['name']
                require(name not in artifacts, 'duplicate compiler executable')
                artifacts[name] = message
        require(set(artifacts) == set(model['targets']), 'missing/extra native compiler artifacts')
        for name, expected in model['targets'].items():
            artifact = artifacts[name]
            require(sorted(artifact['features']) == model['features'] and artifact.get('fresh') is False,
                    'wrong feature set/stale artifact')
            require(artifact['profile']['debuginfo'] == 0 and str(artifact['profile']['opt_level']) == '0', 'test opt0/debug0 required')
            require(Path(artifact['manifest_path']).resolve() == product / 'Cargo.toml'
                    and Path(artifact['target']['src_path']).resolve() == product / expected['source'], 'wrong compiled source')
            binary = Path(artifact['executable']).resolve()
            require(binary.is_relative_to(Path(env['CARGO_TARGET_DIR'])) and binary.is_file(), 'native escapes fresh own target')
            with binary.open('rb') as stream:
                require(stream.read(6) == b'\x7fELF\x02\x01', 'native must be Linux64 little-endian ELF')
            binary_sha = digest(binary)
            shutil.copyfile(binary, output / (name + '.elf'))
            require(digest(output / (name + '.elf')) == binary_sha, 'copied ELF differs')
            write(output / (name + '-build-receipt.json'), {'compiler_artifact': artifact,
                  'binary': str(binary), 'binary_sha256': binary_sha, 'tools': tools, 'build': built})
            for ignored in (False, True):
                stem = name + ('-list-ignored' if ignored else '-list')
                listed = run_owned([str(binary), '--list', '--format', 'terse'] + (['--ignored'] if ignored else []), product, env, output, stem, remaining(30))
                require(listed['exit_code'] == 0 and not listed['timed_out'], 'native listing failed')
                require(native_names((output / (stem + '.stdout')).read_text()) == (set() if ignored else set(expected['names'])), 'exact native target names/ignored set differs')
                require(digest(binary) == binary_sha, 'listed ELF changed')
            runtime = run_owned([str(binary), '--nocapture', '--test-threads=1'], product, env, output, name + '-native', remaining(300), native_sha=binary_sha)
            require(not runtime['timed_out'] and runtime['leader_reaped'], 'native timed out/not reaped')
            require(digest(binary) == binary_sha, 'native ELF changed after run')
            text = (output / (name + '-native.stdout')).read_text(errors='replace') + '\n' + (output / (name + '-native.stderr')).read_text(errors='replace')
            # Runtime assertion failures retain their exact results and do not
            # prevent the other two independent targets from being attempted.
            result['targets'][name] = judge(text, expected['names'], runtime['exit_code'])
            result['targets'][name]['native_binary_sha256'] = binary_sha
        require(tool_identity(product, env) == tools and digest(Path(env['RUSTDOC'])) == rustdoc_sha, 'tool identities changed')
        require(len(result['targets']) == 3 and sum(r['native_summary']['passed'] + r['native_summary']['failed'] for r in result['targets'].values()) == 32, 'incomplete exact32 native targets')
        result['focused_fixture_proof_valid'] = all(r['target_green'] for r in result['targets'].values())
        result['status'] = 'focused_fixture_pass' if result['focused_fixture_proof_valid'] else 'actual_native_fixture_failure'
    except BaseException as error:
        result['errors'].append({'type': type(error).__name__, 'message': str(error)})
    finally:
        if model is not None:
            try:
                after = {'product': source(product, model['product_entries'], model['product'], [], env),
                         'controller': source(control, model['controller_entries'], head, model['auxiliary_paths'], env),
                         'cargo_configs': config_identity(product, env)}
                write(output / 'source-after.json', after)
                require(before is not None and before == after, 'before/after source/config differs')
            except BaseException as error:
                result['errors'].append({'phase': 'final-source-guard', 'type': type(error).__name__, 'message': str(error)})
                result['focused_fixture_proof_valid'] = False
                result['status'] = 'invalid'
        result['elapsed_seconds'] = time.monotonic() - started
        write(output / 'result.json', result)
    return 0 if result['focused_fixture_proof_valid'] else 1


if __name__ == '__main__':
    raise SystemExit(main())
