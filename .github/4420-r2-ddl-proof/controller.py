#!/usr/bin/env python3
"""Only the four frozen R2 daemon commands; no broad-suite/release acceptance."""
import argparse
import fcntl
import hashlib
import json
import os
from pathlib import Path
import re
import signal
import stat
import subprocess
import sys
import threading
import time

HERE = Path(__file__).resolve().parent
BUNDLE_SHA256 = '36ca1433e8255835e2eaec6a39475ca35173d2917253fbb1128368828554d6a2'
FIXED = '5451dd091cebebc3dd6bf10900f3f19a53686a5f'
NAMES = ['standby_readers_never_see_a_torn_state_while_the_primary_runs_ddl',
         'standby_readers_never_see_a_torn_state_while_the_primary_runs_ddl',
         'standby_readers_never_see_a_torn_state_when_every_write_group_is_published_on_its_own',
         'the_detector_reports_a_write_group_split_by_the_test_hook']
ENV_KEYS = ('RUSTUP_TOOLCHAIN', 'CARGO_HOME', 'RUSTUP_HOME', 'CARGO_TARGET_DIR',
            'CARGO_PROFILE_TEST_DEBUG', 'CARGO_PROFILE_DEV_DEBUG', 'CARGO_BUILD_JOBS',
            'CMAKE_BUILD_PARALLEL_LEVEL', 'RUST_TEST_THREADS', 'RUSTFLAGS',
            'CARGO_ENCODED_RUSTFLAGS', 'TMPDIR', 'HELIOSDB_TEST_DAEMON_LOG')


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


class DaemonLogs:
    """Read only exact two logs in owned TMPDIR; hold FDs across TempDir Drop."""
    def __init__(self, root, output):
        self.root, self.output = root, output
        self.output.mkdir()
        self.stop = threading.Event()
        self.opened = {}
        self.errors = []
        self.thread = threading.Thread(target=self.watch, name='owned-daemon-log-capture')

    def sweep(self):
        for name in ('primary.log', 'standby.log'):
            for path in self.root.glob('*/' + name):
                try:
                    fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW)
                    info = os.fstat(fd)
                    key = (info.st_dev, info.st_ino)
                    if key in self.opened:
                        os.close(fd)
                        continue
                    if not stat.S_ISREG(info.st_mode):
                        os.close(fd)
                        raise ValueError('daemon log is not a regular file')
                    target = self.output / ('%02d-' % len(self.opened) + name)
                    self.opened[key] = {'reader': os.fdopen(fd, 'rb'), 'writer': target.open('xb'),
                                        'source': str(path.relative_to(self.root)), 'target': target}
                except FileNotFoundError:
                    # TempDir can disappear between discovery/open. An absent
                    # pair later fails the strict eight-log proof.
                    continue
        for row in self.opened.values():
            while True:
                block = row['reader'].read(1024 * 1024)
                if not block:
                    break
                row['writer'].write(block)
                row['writer'].flush()

    def watch(self):
        try:
            while not self.stop.is_set():
                self.sweep()
                self.stop.wait(0.05)
            self.sweep()
        except BaseException as error:
            self.errors.append({'type': type(error).__name__, 'message': str(error)})

    def finish(self):
        self.stop.set()
        self.thread.join(timeout=10)
        require(not self.thread.is_alive(), 'daemon log capture did not stop')
        rows = []
        for row in self.opened.values():
            row['reader'].close()
            row['writer'].close()
            rows.append({'source_relative_to_owned_tmpdir': row['source'], 'path': str(row['target']),
                         'bytes': row['target'].stat().st_size, 'sha256': digest(row['target'])})
        result = {'logs': rows, 'errors': self.errors, 'read_only_capture': True,
                  'scope': 'only primary.log/standby.log; no token/config/database copies'}
        write(self.output / 'capture.json', result)
        return result


def verify_bundle():
    path = HERE / 'bundle/bundle-sha256.json'
    require(digest(path) == BUNDLE_SHA256, 'V7 bundle manifest changed')
    pins = json.loads(path.read_text())
    require(len(pins) == 24, 'V7 member count changed')
    for name, pin in pins.items():
        require(digest(path.parent / name) == pin, 'V7 helper changed: ' + name)
    return pins


def command_list():
    result = []
    for index, name in enumerate(NAMES):
        argv = ['cargo', 'test', '--locked']
        if index:
            argv += ['--features', 'internal-tests']
        result.append(argv + ['--test', 'physical_ddl_window', name, '--', '--exact', '--include-ignored', '--nocapture', '--test-threads=1'])
    return result


def judge(stage, stage_dir):
    require(stage['stage'] == 'ddl-window' and len(stage['commands']) == 4, 'four actual command receipts required')
    require(not stage['failed'], 'original stage wrapper reported failure')
    require(all(row['unchanged'] for row in stage['manifest_inputs'].values()), 'captured source/expected manifests changed')
    expected = command_list()
    observations = []
    standby = {'standby-fresh-0', 'standby-fresh-1', 'standby-fresh-2', 'standby-cached-3'}
    for index, row in enumerate(stage['commands']):
        require(row['argv'] == expected[index], 'frozen command argv/order differs')
        require(row['exit'] == 0 and not row['timed_out'] and not row['completeness_problems'], 'native command failed/partial')
        require(row['results'] == {'physical_ddl_window': {'status': 'ok', 'passed': 1, 'failed': 0, 'ignored': 0}}, 'each exact command must execute one passing parent')
        require(row['features'] == {'explicit': [] if index == 0 else ['internal-tests'], 'source': 'Cargo argv', 'default_features': True, 'all_features': False}, 'per-command features changed')
        require(row['binaries'] and any(item.get('role') == 'Cargo daemon path' for item in row['binaries']), 'native/daemon identity receipt missing')
        text = (stage_dir / ('%02d.log' % index)).read_text(errors='replace')
        summaries = re.findall(r'test result: (ok|FAILED)\. (\d+) passed; (\d+) failed; (\d+) ignored;', text)
        require(summaries == [('ok', '1', '0', '0')], 'actual native summary missing/partial')
        header = re.findall(r'DDL-WINDOW \[([^\]]+)\] (\d+) iteration\(s\), (\d+) statements, (\d+) primary states, (\d+) distinct answers', text)
        wanted = [('plain', '20', '420', '421'), ('plain', '20', '420', '421'), ('paced', '6', '126', '127'), ('split self-check', '4', '84', '85')][index]
        require(len(header) == 1 and header[0][:4] == wanted and int(header[0][4]) > 0, 'actual R2 iterations/statement census incomplete')
        readers = re.findall(r'^DDL-WINDOW\s+([\w-]+)\s+(\d+) rounds, (\d+) answers: torn (\d+), window (\d+), pair (\d+), broken (\d+), not ready (\d+)', text, re.M)
        names = [item[0] for item in readers]
        require(len(set(names)) == len(names) and set(names) == (standby if index == 3 else standby | {'primary-fresh-4'}), 'exact standby/cached/primary reader set missing')
        require(all(int(item[2]) > 0 for item in readers), 'a reader observed no answers')
        require(text.count('DDL-WINDOW   standby /ready at the end: Some(200)') == 1, 'standby final readiness missing')
        detector = None
        if index < 3:
            require(all(all(int(value) == 0 for value in item[3:]) for item in readers), 'R2 reader reported torn/window/pair/broken/not-ready (including40001)')
        else:
            split = re.findall(r'DDL-WINDOW self-check: (\d+) write group\(s\) split, (\d+) torn answer\(s\) of ddl_a reported', text)
            require(len(split) == 1 and all(int(value) > 0 for value in split[0]), 'synthetic split detector was vacuous')
            require(sum(int(item[3]) for item in readers) > 0, 'detector reader census reported no tears')
            detector = {'split_groups': int(split[0][0]), 'torn_ddl_a_answers': int(split[0][1])}
        observations.append({'command_index': index, 'header': header[0], 'readers': readers,
                             'detector': detector, 'raw_log_sha256': digest(stage_dir / ('%02d.log' % index))})
    return {'four_exact_native_commands_pass': True, 'native_parents_passed': 4, 'observations': observations,
            'release_or_full_tier_acceptance': False}


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--product', type=Path, required=True)
    parser.add_argument('--controller', type=Path, required=True)
    parser.add_argument('--out', type=Path, required=True)
    parser.add_argument('--model-sha256', required=True)
    args = parser.parse_args()
    product, control, output = args.product.resolve(), args.controller.resolve(), args.out.resolve()
    require(not output.is_relative_to(product) and not output.is_relative_to(control), 'output must be external')
    output.mkdir(parents=True, exist_ok=False)
    env = os.environ.copy()
    env.update(RUSTUP_TOOLCHAIN='1.95.0', CARGO_BUILD_JOBS='2', CMAKE_BUILD_PARALLEL_LEVEL='2',
               CARGO_PROFILE_TEST_DEBUG='0', CARGO_PROFILE_DEV_DEBUG='0', RUST_TEST_THREADS='2',
               CARGO_TERM_COLOR='never', NANO_FINAL_GATE_BOUNDED='1',
               CARGO_TARGET_DIR=str(output / 'fresh-target'), TMPDIR=str(output / 'runtime-tmp'))
    Path(env['TMPDIR']).mkdir()
    result = {'status': 'invalid', 'errors': [], 'commands_green': False, 'release_acceptance_pass': False}
    before, model, logs, log_capture, tools, fleet = None, None, None, None, None, None
    started = time.monotonic()
    try:
        require(digest(HERE / 'model.json') == args.model_sha256, 'model bootstrap mismatch')
        model = json.loads((HERE / 'model.json').read_text())
        require(model['product_head'] == FIXED, 'wrong fixed product')
        verify_bundle()
        recipe = json.loads((HERE / 'bundle/commands.json').read_text())['ddl-window']
        require(recipe == model['exact_stage'] and recipe['commands'] == command_list(), 'frozen four-command stage changed')
        require(not env.get('RUSTFLAGS') and not env.get('CARGO_ENCODED_RUSTFLAGS'), 'undeclared extra Rust flags')
        require('HELIOSDB_TEST_DDL_WINDOW_ITERATIONS' not in env, 'R2 iteration override forbidden')
        group = next(line.split(':', 2)[2] for line in Path('/proc/self/cgroup').read_text().splitlines() if line.startswith('0::'))
        cg = Path('/sys/fs/cgroup') / group.lstrip('/')
        limits = {name: (cg / name).read_text().strip() for name in ('memory.max', 'memory.swap.max')}
        require(limits == {'memory.max': str(24 * 1024**3), 'memory.swap.max': '0'}, '24GiB/no-swap service required')
        fleet = Path('/home/gpc/HDB/sprint/coordination/build.lock').open('r+')
        fcntl.flock(fleet, fcntl.LOCK_EX | fcntl.LOCK_NB)
        before = {'product': source(product, model['product_entries'], FIXED, [], env),
                  'controller': source(control, model['controller_entries'], os.environ['GITHUB_SHA'], model['auxiliary_paths'], env)}
        write(output / 'source-before.json', before)
        tools = tool_identity(product, env)
        write(output / 'host.json', {'tools': tools, 'cgroup': group, 'limits': limits, 'uname': list(os.uname()),
              'meminfo': Path('/proc/meminfo').read_text(), 'environment': {key: env.get(key) for key in ENV_KEYS},
              'original_command_timeout_default_seconds': recipe.get('timeout_seconds', 21600),
              'outer_shared_service_budget_seconds': 8400})
        capture = run_owned([sys.executable, str(HERE / 'bundle/inventory.py'), str(product), str(output / 'capture'),
                             '--label', 'fixed-R2-four-commands-' + FIXED], product, env, output, 'inventory', 120)
        require(capture['exit_code'] == 0 and not capture['timed_out'], 'source inventory capture failed')
        expected = json.loads((output / 'capture/expected-manifest.json').read_text())
        require(expected['head'] == FIXED and any(row['target'] == 'physical_ddl_window' for row in expected['targets']), 'capture lacks product/native target')
        require(not Path(env['CARGO_TARGET_DIR']).exists(), 'target must be genuinely fresh')
        logs = DaemonLogs(Path(env['TMPDIR']), output / 'daemon-logs')
        logs.thread.start()
        runtime = run_owned([sys.executable, str(HERE / 'bundle/run_stage.py'), 'ddl-window', '--tree', str(product),
                '--target', env['CARGO_TARGET_DIR'], '--out', str(output / 'ddl-window'),
                '--source-manifest', str(output / 'capture/source-sha256.json'),
                '--expected-manifest', str(output / 'capture/expected-manifest.json'), '--execute'],
                product, env, output, 'ddl-stage', max(1, 8100 - (time.monotonic() - started)))
        log_capture = logs.finish()
        logs = None
        require(not runtime['timed_out'] and runtime['leader_reaped'], 'four-command wrapper timed out/not reaped')
        stage = json.loads((output / 'ddl-window/receipt.json').read_text())
        require(len(stage['commands']) == 4, 'not all four original commands attempted')
        write(output / 'stage-attempts.json', {'all_four_attempted': True, 'runtime': runtime,
              'command_outcomes': [{'argv': row['argv'], 'exit': row['exit'], 'results': row['results'],
                                    'problems': row['completeness_problems']} for row in stage['commands']]})
        require(runtime['exit_code'] == 0, 'original four-command stage returned failure; raw failures remain unmodified')
        judged = judge(stage, output / 'ddl-window')
        require(not log_capture['errors'] and len(log_capture['logs']) == 8, 'eight ordinary daemon log identities were not retained')
        by_dir = {}
        for row in log_capture['logs']:
            path = Path(row['source_relative_to_owned_tmpdir'])
            by_dir.setdefault(str(path.parent), set()).add(path.name)
        require(len(by_dir) == 4 and all(names == {'primary.log', 'standby.log'} for names in by_dir.values()), 'four exact daemon log pairs missing')
        require(tool_identity(product, env) == tools, 'compiler/Cargo changed')
        write(output / 'native-adjudication.json', judged)
        result.update(judged)
        result.update(status='focused_R2_four_commands_green', commands_green=True)
    except BaseException as error:
        result['errors'].append({'type': type(error).__name__, 'message': str(error)})
    finally:
        if logs is not None:
            try:
                log_capture = logs.finish()
            except BaseException as error:
                result['errors'].append({'phase': 'daemon-log-cleanup', 'type': type(error).__name__, 'message': str(error)})
                result['commands_green'] = False
        if model is not None:
            try:
                after = {'product': source(product, model['product_entries'], FIXED, [], env),
                         'controller': source(control, model['controller_entries'], os.environ['GITHUB_SHA'], model['auxiliary_paths'], env)}
                write(output / 'source-after.json', after)
                require(before is not None and before == after, 'source before/after differs')
                verify_bundle()
            except BaseException as error:
                result['errors'].append({'phase': 'final-source-guard', 'type': type(error).__name__, 'message': str(error)})
                result['commands_green'] = False
        result['elapsed_seconds'] = time.monotonic() - started
        if result['errors']:
            result['status'] = 'invalid_or_native_red'
            result['commands_green'] = False
        write(output / 'result.json', result)
    return 0 if result['commands_green'] else 1


if __name__ == '__main__':
    raise SystemExit(main())
