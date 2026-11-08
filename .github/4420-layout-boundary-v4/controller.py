#!/usr/bin/env python3
"""Focused boundary V4 native controls on an isolated Count-fixed product."""
import argparse
from collections import Counter
import fcntl
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import signal
import stat
import subprocess
import threading
import time

HERE = Path(__file__).resolve().parent
FIXED = 'd04d8ac7a3723b5d8a8ec2464e13357fc99b979b'
ENV_KEYS = ('RUSTUP_TOOLCHAIN','CARGO_HOME','RUSTUP_HOME','CARGO_TARGET_DIR',
            'CARGO_PROFILE_TEST_DEBUG','CARGO_PROFILE_DEV_DEBUG','CARGO_BUILD_JOBS',
            'CMAKE_BUILD_PARALLEL_LEVEL','RUST_TEST_THREADS','RUSTFLAGS','CARGO_ENCODED_RUSTFLAGS',
            'TMPDIR','HELIOSDB_TEST_DAEMON_LOG')


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

def judge(text, wanted, runtime):
    require(wanted['names'] and len(wanted['names']) == len(set(wanted['names'])), 'nonempty unique expected native names required')
    require(runtime['exit_code'] == 0 and not runtime['timed_out'] and runtime['leader_reaped'],
            'native command failed/timed out/not reaped')
    rows = re.findall(r'test result: (ok|FAILED)\. (\d+) passed; (\d+) failed; (\d+) ignored; (\d+) measured; (\d+) filtered out', text)
    require(len(rows) == 1, 'one native outer summary required')
    status, passed, failed, ignored, measured, filtered = rows[0]
    require((status, int(passed), int(failed), int(ignored), int(measured)) == ('ok', len(wanted['names']), 0, 0, 0),
            'expected complete nonzero native parent counts differ')
    if not wanted.get('filter'):
        require(int(filtered) == 0, 'unfiltered integration omitted native tests')
    # --nocapture output may interrupt a parent result line, so exact native
    # --list below is authoritative for names; summary is authoritative for
    # execution counts. Never infer parent completion from a marker alone.
    return {'passed': int(passed), 'failed': 0, 'ignored': 0, 'measured': 0,
            'filtered': int(filtered), 'listed_names': wanted['names']}


def boundary_cells(text, case):
    cells = re.findall(r'layout-boundary-(?:explicit|interrupt|native)-cell[^\r\n]*', text)
    require(case['markers'] and len(case['markers']) == len(set(case['markers'])) == case['cells'], 'nonempty exact expected boundary cells required')
    require(Counter(cells) == Counter(case['markers']), 'boundary cell identities missing/unexpected/duplicated')
    return cells


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--configuration', choices=['default','internal','persistent'], required=True)
    parser.add_argument('--product', type=Path, required=True)
    parser.add_argument('--controller', type=Path, required=True)
    parser.add_argument('--out', type=Path, required=True)
    parser.add_argument('--model-sha256', required=True)
    args = parser.parse_args()
    product, control, output = args.product.resolve(), args.controller.resolve(), args.out.resolve()
    require(not output.is_relative_to(product) and not output.is_relative_to(control), 'output/target must be external')
    output.mkdir(parents=True, exist_ok=False)
    env = os.environ.copy()
    env.update(RUSTUP_TOOLCHAIN='1.95.0', CARGO_BUILD_JOBS='2', CMAKE_BUILD_PARALLEL_LEVEL='2',
               CARGO_PROFILE_TEST_DEBUG='0', CARGO_PROFILE_DEV_DEBUG='0', RUST_TEST_THREADS='2',
               CARGO_TERM_COLOR='never', CARGO_TARGET_DIR=str(output / 'fresh-target'),
               TMPDIR=str(output / 'runtime-tmp'), HELIOSDB_TEST_DAEMON_LOG='info,heliosdb_nano::replication::promote=trace')
    Path(env['TMPDIR']).mkdir(mode=0o700)
    result = {'configuration': args.configuration, 'status': 'invalid', 'commands_green': False,
              'errors': [], 'commands': [], 'release_acceptance_pass': False}
    before, model, fleet, logs = None, None, None, None
    started = time.monotonic()
    try:
        require(digest(HERE / 'model.json') == args.model_sha256, 'model bootstrap mismatch')
        model = json.loads((HERE / 'model.json').read_text())
        require(model['product_head'] == FIXED, 'wrong immutable fixed product')
        plan = model['configurations'][args.configuration]
        require(not env.get('RUSTFLAGS') and not env.get('CARGO_ENCODED_RUSTFLAGS'), 'undeclared Rust flags')
        group = next(line.split(':', 2)[2] for line in Path('/proc/self/cgroup').read_text().splitlines() if line.startswith('0::'))
        cg = Path('/sys/fs/cgroup') / group.lstrip('/')
        limits = {name: (cg / name).read_text().strip() for name in ('memory.max','memory.swap.max')}
        require(limits == {'memory.max': str(24 * 1024**3), 'memory.swap.max':'0'}, '24GiB/zeroSwap required')
        fleet = Path('/home/gpc/HDB/sprint/coordination/build.lock').open('r+')
        fcntl.flock(fleet, fcntl.LOCK_EX | fcntl.LOCK_NB)
        before = {'product': source(product, model['product_entries'], FIXED, [], env),
                  'controller': source(control, model['controller_entries'], os.environ['GITHUB_SHA'], model['auxiliary_paths'], env)}
        write(output / 'source-before.json', before)
        tools = tool_identity(product, env)
        write(output / 'host.json', {'tools':tools,'limits':limits,'cgroup':group,'uname':list(os.uname()),
              'environment':{key:env.get(key) for key in ENV_KEYS},'upstream_pair_run':model['upstream_pair_run']})
        require(not Path(env['CARGO_TARGET_DIR']).exists(), 'target must be genuinely fresh')
        def remaining(maximum):
            value = min(maximum, 7200 - (time.monotonic() - started))
            require(value > 0, 'shared build/native7200s budget expired')
            return value
        features = ['--features',plan['features'][0]] if plan['features'] else []
        prefix = ['cargo','test','--locked'] + features
        targets = [case['target'] for case in plan['cases'] if case['target'] != 'lib']
        build_argv = prefix + ['--lib']
        for target in targets:
            build_argv += ['--test',target]
        built = run_owned(build_argv + ['--no-run','--message-format=json'], product, env, output, 'build', remaining(4200))
        require(built['exit_code'] == 0 and not built['timed_out'], 'native compile failed/timed out')
        messages = [json.loads(line) for line in (output / 'build.stdout').read_text().splitlines()]
        artifacts = {}
        for message in messages:
            if message.get('reason') != 'compiler-artifact' or not message.get('executable'):
                continue
            kind = message.get('target',{}).get('kind')
            is_test = message.get('profile',{}).get('test')
            key = 'lib' if kind == ['lib'] and is_test else message.get('target',{}).get('name')
            if not ((key == 'lib' and is_test) or (key in targets and kind == ['test'] and is_test)
                    ):
                continue
            require(key not in artifacts, 'duplicate compiler executable identity')
            require(sorted(message['features']) == plan['compiled_features'], 'compiled feature identity differs')
            require(message['profile']['debuginfo'] == 0 and message['profile']['opt_level'] == '0'
                    and message['fresh'] is False, 'wrong/nonfresh debug0 profile')
            require(Path(message['manifest_path']).resolve() == product / 'Cargo.toml', 'wrong package compiled')
            rel = 'src/lib.rs' if key == 'lib' else ('src/main.rs' if key == 'heliosdb-nano' else 'tests/' + key + '.rs')
            require(Path(message['target']['src_path']).resolve() == product / rel, 'wrong target source compiled')
            binary = Path(message['executable']).resolve()
            require(binary.is_file() and binary.is_relative_to(Path(env['CARGO_TARGET_DIR'])), 'binary escapes fresh target')
            with binary.open('rb') as stream:
                require(stream.read(6) == b'\x7fELF\x02\x01', 'native Linux64 ELF required')
            artifacts[key] = {'compiler_artifact':message,'path':str(binary),'sha256':digest(binary)}
        require(set(artifacts) == set(targets) | {'lib'}, 'not all native harnesses/daemon came from compiler')
        write(output / 'native-build-receipt.json',{'tools':tools,'build':built,'artifacts':artifacts})
        for index, case in enumerate(plan['cases']):
            stem = '%02d-' % index + case['target']
            binary = Path(artifacts[case['target']]['path'])
            listing_argv = [str(binary)] + ([case['filter']] if case.get('filter') else []) + ['--list','--format','terse']
            listing = run_owned(listing_argv, product, env, output, stem+'-list', remaining(60))
            require(listing['exit_code'] == 0 and not listing['timed_out'], 'native listing failed')
            names = [line[:-6] for line in (output / (stem+'-list.stdout')).read_text().splitlines() if line.endswith(': test')]
            require(len(names) == len(set(names)) and set(names) == set(case['names']) and names,
                    'actual native parent names/counts differ')
            if case['kind'] == 'unchanged_promotion_diagnostic':
                # Only this single unchanged case uses the original strict R2
                # two-log watcher, so exactly its P/S lifecycle logs are kept.
                promote_tmp = output / 'promotion-tmp'
                promote_tmp.mkdir(mode=0o700)
                env['TMPDIR'] = str(promote_tmp)
                logs = DaemonLogs(promote_tmp, output / 'promotion-daemon-logs')
                logs.thread.start()
            runtime = run_owned(case['argv'], product, env, output, stem, remaining(1800))
            captured = None
            if logs is not None:
                captured = logs.finish()
                logs = None
            require(all(digest(Path(row['path'])) == row['sha256'] for row in artifacts.values()), 'compiled native bytes changed')
            text = (output / (stem+'.stdout')).read_text(errors='replace') + '\n' + (output / (stem+'.stderr')).read_text(errors='replace')
            pattern = r'Running unittests src/lib\.rs \(([^\n]+)\)' if case['target'] == 'lib' else r'Running tests/' + re.escape(case['target']) + r'\.rs \(([^\n]+)\)'
            invoked = re.findall(pattern,text)
            require(len(invoked) == 1 and (product / invoked[0]).resolve() == binary, 'runtime binary differs from compiler/list')
            observation = {'argv':case['argv'],'runtime':runtime,'native_parent_names':names,'green':False,
                           'environment':{key:env.get(key) for key in ENV_KEYS},'daemon_log_capture':captured}
            try:
                observation['summary'] = judge(text,case,runtime)
                cells = boundary_cells(text, case)
                observation['cells'] = cells
                observation['cell_count'] = len(cells)
                if captured is not None:
                    require(not captured['errors'] and len(captured['logs']) == 2
                            and all(row['bytes'] > 0 for row in captured['logs']), 'full nonempty two-daemon promotion logs not retained')
                    require({Path(row['source_relative_to_owned_tmpdir']).name for row in captured['logs']}
                            == {'primary.log','standby.log'}, 'unexpected promotion log identities')
                observation['green'] = True
            except ValueError as error:
                observation['failure'] = str(error)
            result['commands'].append(observation)
            write(output / 'command-observations.json',result['commands'])
            # A native failure does not hide the following independent cases.
            require(source(product, model['product_entries'], FIXED, [], env) == before['product'], 'product changed between commands')
        require(tool_identity(product,env) == tools, 'compiler/Cargo identity changed')
        result['commands_green'] = (len(result['commands']) == len(plan['cases'])
                                    and all(row['green'] for row in result['commands'])
                                    and sum(row.get('cell_count',0) for row in result['commands']) == plan['native_cell_total'])
        result['status'] = 'all_focused_boundary_v4_controls_pass' if result['commands_green'] else 'actual_boundary_v4_control_failure'
    except BaseException as error:
        result['errors'].append({'type':type(error).__name__,'message':str(error)})
    finally:
        if logs is not None:
            try:
                logs.finish()
            except BaseException as error:
                result['errors'].append({'phase':'log-cleanup','type':type(error).__name__,'message':str(error)})
        if model is not None:
            try:
                after = {'product':source(product,model['product_entries'],FIXED,[],env),
                         'controller':source(control,model['controller_entries'],os.environ['GITHUB_SHA'],model['auxiliary_paths'],env)}
                write(output / 'source-after.json',after)
                require(before is not None and before == after, 'source before/after differs')
            except BaseException as error:
                result['errors'].append({'phase':'final-source-guard','type':type(error).__name__,'message':str(error)})
        result['elapsed_seconds'] = time.monotonic() - started
        if result['errors']:
            result['commands_green'] = False
            result['status'] = 'invalid_or_native_failure'
        write(output / 'result.json',result)
    return 0 if result['commands_green'] else 1


if __name__ == '__main__':
    raise SystemExit(main())
