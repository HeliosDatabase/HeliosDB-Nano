#!/usr/bin/env python3
"""Codegraph native ownership pair plus independent unchanged field benchmark."""
import argparse
import fcntl
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import signal
import subprocess
import time

HERE = Path(__file__).resolve().parent
TARGET = 'code_graph_transaction_ownership'
FIELD = 'code_graph_parallel_index_bench'
FIELD_NAME = 'field_benchmark_nano_src_tree'
NAMES = {'a_cold_public_index_joins_its_own_fk_cleanup_transaction',
         'a_caller_transaction_owns_graph_commit_and_rollback',
         'a_late_reference_refusal_restores_old_graph_rows_and_deletes',
         'a_branch_index_keeps_rows_and_rollback_on_its_own_branch'}
FEATURES = ['code-graph', 'default', 'encryption', 'ha-tier1', 'ring-crypto', 'vector-search']
ENV_KEYS = ('RUSTUP_TOOLCHAIN','CARGO_HOME','RUSTUP_HOME','CARGO_TARGET_DIR',
            'CARGO_PROFILE_TEST_DEBUG','CARGO_PROFILE_DEV_DEBUG','CARGO_BUILD_JOBS',
            'CMAKE_BUILD_PARALLEL_LEVEL','RUST_TEST_THREADS','RUSTFLAGS','CARGO_ENCODED_RUSTFLAGS')


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


def summary(text, passed, failed):
    rows = re.findall(r'test result: (ok|FAILED)\. (\d+) passed; (\d+) failed; (\d+) ignored; (\d+) measured; (\d+) filtered out', text)
    require(len(rows) == 1, 'one actual native summary required')
    status, got_passed, got_failed, ignored, measured, filtered = rows[0]
    require((int(got_passed), int(got_failed), int(ignored), int(measured)) == (passed, failed, 0, 0), 'partial/wrong native counts')
    require(status == ('FAILED' if failed else 'ok'), 'native summary status mismatch')
    return {'passed': passed, 'failed': failed, 'ignored': 0, 'filtered': int(filtered)}


def judge_focused(text, role, code):
    rows = re.findall(r'codegraph-transaction-cell (cold|caller|late-refusal|branch) history=(false|true)(?: (commit|caller)=(false|true))?', text)
    require(text.count('codegraph-transaction-cell') == len(rows) and len(set(rows)) == len(rows), 'malformed/duplicate cell receipts')
    premises = re.findall(r'codegraph-cold-premise history=(false|true) source_rows=1 graph_rows=0 lock_timeout_ms=100', text)
    require(text.count('codegraph-cold-premise') == len(premises), 'malformed cold API premise')
    if role == 'baseline':
        require(code == 101, 'baseline must fail natively')
        actual = summary(text, 0, 4)
        require(not rows and premises == ['false'], 'baseline unexpectedly completed cells or lacked first cold premise')
        faults = re.findall(r'could not obtain lock on table _hdb_code_symbols: FK ON DELETE holds it \(waited (\d+) ms;', text)
        require(len(faults) == 4 and all(int(value) >= 100 for value in faults), 'all four baseline parents must expose the real owner self-wait')
        require(text.count('Error: QueryExecution(') == 3 and 'the late constraint must be reached:' in text, 'baseline failure provenance changed')
        block = text.rsplit('\nfailures:\n', 1)[-1].split('\ntest result:', 1)[0]
        require({line.strip() for line in block.splitlines() if line.strip() in NAMES} == NAMES, 'baseline failing parent set differs')
        status = 'validated_expected_native_owner_self_wait'
    else:
        require(code == 0, 'fixed native regression failed')
        actual = summary(text, 4, 0)
        expected = {(kind, history, '', '') for kind in ('cold', 'branch') for history in ('false', 'true')}
        expected |= {(kind, history, key, value) for kind, key in [('caller', 'commit'), ('late-refusal', 'caller')]
                     for history in ('false', 'true') for value in ('false', 'true')}
        require(set(rows) == expected and sorted(premises) == ['false', 'true'], 'all twelve exact ownership cells/premises required')
        status = 'focused_four_parent_twelve_cell_pass'
    return {'status': status, 'native_summary': actual, 'completed_cells': len(rows), 'cold_premises': len(premises),
            'release_acceptance_pass': False}


def judge_field(text, code, corpus):
    require(code == 0, 'unchanged release field benchmark failed')
    actual = summary(text, 1, 0)
    found = re.findall(r'Corpus: (\d+) Rust files', text)
    require(found == [str(corpus)] and corpus == 443, 'exact source-bound443-file corpus required')
    rows = re.findall(r'total (\d+) ms, parse (\d+) ms, write (\d+) ms, files parsed (\d+)', text)
    require(len(rows) == 3 and all(int(row[3]) == corpus for row in rows), 'serial/parallel/chunked must each parse the entire corpus')
    require(all(int(row[0]) > 0 and int(row[1]) > 0 for row in rows), 'empty field measurement')
    require('Parse-phase speedup:' in text and 'Effective utilisation (parse_speedup / workers):' in text,
            'unchanged benchmark ratio receipts missing')
    return {'status': 'unchanged_release_field_benchmark_pass', 'native_summary': actual,
            'corpus_files': corpus, 'serial_parallel_chunked_raw_ms': rows,
            'acceptance_scope': 'original native assertions and full-corpus completion; no additional under2min/70percent claim',
            'release_acceptance_pass': False}


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--mode', choices=['focused', 'field'], required=True)
    parser.add_argument('--role', choices=['baseline', 'fixed'], required=True)
    parser.add_argument('--product', type=Path, required=True)
    parser.add_argument('--controller', type=Path, required=True)
    parser.add_argument('--out', type=Path, required=True)
    parser.add_argument('--model-sha256', required=True)
    args = parser.parse_args()
    require(args.mode != 'field' or args.role == 'fixed', 'field validation is a separate fixed-source job')
    product, control, output = args.product.resolve(), args.controller.resolve(), args.out.resolve()
    require(not output.is_relative_to(product) and not output.is_relative_to(control), 'output/target must be external')
    output.mkdir(parents=True, exist_ok=False)
    result = {'role': args.role, 'mode': args.mode, 'status': 'invalid', 'proof_valid': False, 'errors': [], 'release_acceptance_pass': False}
    env = os.environ.copy()
    env.update(RUSTUP_TOOLCHAIN='1.95.0', CARGO_BUILD_JOBS='2', CMAKE_BUILD_PARALLEL_LEVEL='2',
               CARGO_PROFILE_TEST_DEBUG='0', CARGO_PROFILE_DEV_DEBUG='0', RUST_TEST_THREADS='2',
               CARGO_TERM_COLOR='never', CARGO_TARGET_DIR=str(output / 'fresh-target'))
    before, model, fleet = None, None, None
    started = time.monotonic()
    try:
        require(digest(HERE / 'model.json') == args.model_sha256, 'model bootstrap mismatch')
        model = json.loads((HERE / 'model.json').read_text())
        require(set(model['roles']) == {'baseline', 'fixed'}, 'roles changed')
        changes = {name for name in model['entries']['baseline'] if model['entries']['baseline'][name] != model['entries']['fixed'][name]}
        require(changes == {'src/code_graph/storage.rs'}, 'pair differs beyond the one production path')
        require(not env.get('RUSTFLAGS') and not env.get('CARGO_ENCODED_RUSTFLAGS'), 'undeclared extra Rust flags')
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
        write(output / 'host.json', {'tools': tools, 'cgroup': group, 'limits': limits, 'uname': list(os.uname()),
              'meminfo': Path('/proc/meminfo').read_text(), 'environment': {key: env.get(key) for key in ENV_KEYS},
              'source_pair_sha256': model['source_pair_sha256']})
        require(not Path(env['CARGO_TARGET_DIR']).exists(), 'target must be genuinely fresh')
        budget = 5400 if args.mode == 'field' else 3600
        def remaining(maximum):
            value = min(maximum, budget - (time.monotonic() - started))
            require(value > 0, 'shared build/native deadline expired')
            return value
        target = FIELD if args.mode == 'field' else TARGET
        prefix = ['cargo', 'test', '--locked'] + (['--release'] if args.mode == 'field' else []) + ['--features', 'code-graph', '--test', target]
        built = run_owned(prefix + ['--no-run', '--message-format=json'], product, env, output, 'build', remaining(4800 if args.mode == 'field' else 3000))
        require(built['exit_code'] == 0 and not built['timed_out'], 'native compile failed/timed out')
        messages = [json.loads(line) for line in (output / 'build.stdout').read_text().splitlines()]
        artifacts = [m for m in messages if m.get('reason') == 'compiler-artifact' and m.get('executable')
                     and m.get('profile', {}).get('test') and m.get('target', {}).get('kind') == ['test'] and m['target']['name'] == target]
        require(len(artifacts) == 1, 'one compiler-owned integration executable required')
        artifact = artifacts[0]
        require(sorted(artifact['features']) == FEATURES and artifact['profile']['debuginfo'] == 0 and artifact['fresh'] is False, 'feature/profile/fresh build receipt changed')
        require(artifact['profile']['opt_level'] == ('3' if args.mode == 'field' else '0'), 'wrong profile optimisation')
        require(Path(artifact['manifest_path']).resolve() == product / 'Cargo.toml' and Path(artifact['target']['src_path']).resolve() == product / ('tests/' + target + '.rs'), 'wrong source compiled')
        binary = Path(artifact['executable']).resolve()
        require(binary.is_relative_to(Path(env['CARGO_TARGET_DIR'])) and binary.is_file(), 'ELF escapes owned fresh target')
        with binary.open('rb') as stream:
            require(stream.read(6) == b'\x7fELF\x02\x01', 'nativeLinux64ELF required')
        binary_sha = digest(binary)
        shutil.copyfile(binary, output / 'native-test.elf')
        require(digest(output / 'native-test.elf') == binary_sha, 'preserved ELF copy differs')
        write(output / 'native-build-receipt.json', {'compiler_artifact': artifact, 'binary': str(binary), 'binary_sha256': binary_sha,
              'role_head': head, 'build': built, 'tools': tools})
        listing_argv = [str(binary)] + ([FIELD_NAME] if args.mode == 'field' else []) + ['--list', '--format', 'terse']
        listed = run_owned(listing_argv, product, env, output, 'native-list', remaining(60))
        require(listed['exit_code'] == 0 and not listed['timed_out'], 'actual native listing failed')
        names = {line[:-6] for line in (output / 'native-list.stdout').read_text().splitlines() if line.endswith(': test')}
        require(names == ({FIELD_NAME} if args.mode == 'field' else NAMES), 'actual native parent list incomplete')
        argv = prefix + ([FIELD_NAME, '--', '--exact', '--include-ignored', '--nocapture', '--test-threads=1'] if args.mode == 'field' else ['--', '--nocapture', '--test-threads=2'])
        if args.mode == 'field':
            require(argv == model['unchanged_field_command'], 'field command differs from original frozen recipe')
            require(os.getloadavg()[0] < 6, 'field load gate not met; no timing run started')
        runtime = run_owned(argv, product, env, output, 'native', remaining(600 if args.mode == 'field' else 300))
        require(not runtime['timed_out'] and runtime['leader_reaped'], 'native timeout/leader not reaped')
        require(digest(binary) == binary_sha, 'native ELF changed after listing/run')
        text = (output / 'native.stdout').read_text(errors='replace') + '\n' + (output / 'native.stderr').read_text(errors='replace')
        invoked = re.findall(r'Running tests/' + re.escape(target) + r'\.rs \(([^\n]+)\)', text)
        require(len(invoked) == 1 and (product / invoked[0]).resolve() == binary, 'runtime ELF does not match compiler/list')
        judged = judge_field(text, runtime['exit_code'], model['source_rust_corpus_files']) if args.mode == 'field' else judge_focused(text, args.role, runtime['exit_code'])
        require(tool_identity(product, env) == tools, 'compiler/Cargo identity changed')
        write(output / 'native-adjudication.json', judged)
        result.update(judged)
        result.update(proof_valid=True, native_binary_sha256=binary_sha)
    except BaseException as error:
        result['errors'].append({'type': type(error).__name__, 'message': str(error)})
    finally:
        if model is not None:
            try:
                after = {'product': source(product, model['entries'][args.role], model['roles'][args.role], [], env),
                         'controller': source(control, model['controller_entries'], os.environ['GITHUB_SHA'], model['auxiliary_paths'], env)}
                write(output / 'source-after.json', after)
                require(before is not None and before == after, 'source before/after differs')
            except BaseException as error:
                result['errors'].append({'phase': 'final-source-guard', 'type': type(error).__name__, 'message': str(error)})
        result['elapsed_seconds'] = time.monotonic() - started
        if result['errors']:
            result['status'] = 'invalid_or_native_red'
            result['proof_valid'] = False
        write(output / 'result.json', result)
    return 0 if result['proof_valid'] else 1


if __name__ == '__main__':
    raise SystemExit(main())
