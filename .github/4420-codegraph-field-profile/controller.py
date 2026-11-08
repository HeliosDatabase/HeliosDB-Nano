#!/usr/bin/env python3
"""Read-only owned-process diagnostic; original native test and600s unchanged."""
import argparse
import fcntl
import hashlib
import json
import os
from pathlib import Path
import signal
import stat
import subprocess
import time
import urllib.error
import urllib.parse
import urllib.request
import zipfile
HERE = Path(__file__).resolve().parent
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

class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        return None


def download(output, model):
    output.mkdir(parents=True, exist_ok=False)
    token = os.environ.get('NANO_ARTIFACT_TOKEN')
    require(bool(token), 'explicit actions-read token required for fixed artifact only')
    api = 'https://api.github.com/repos/HeliosDatabase/HeliosDB-Nano/actions/artifacts/' + str(model['artifact_id'])
    headers = {'Authorization': 'Bearer ' + token, 'Accept': 'application/vnd.github+json'}
    try:
        with urllib.request.urlopen(urllib.request.Request(api, headers=headers), timeout=60) as response:
            metadata = json.load(response)
        require(metadata['id'] == model['artifact_id'] and metadata['workflow_run']['id'] == model['run_id'], 'provider artifact/run mismatch')
        require(metadata['digest'] == 'sha256:' + model['archive_sha256'] and not metadata['expired'], 'provider digest/expiration mismatch')
        try:
            urllib.request.build_opener(NoRedirect).open(urllib.request.Request(api + '/zip', headers=headers), timeout=60)
            raise ValueError('expected provider redirect')
        except urllib.error.HTTPError as response:
            require(response.code in (301, 302, 303, 307), 'artifact redirect failed')
            location = response.headers['Location']
        parsed = urllib.parse.urlparse(location)
        require(parsed.scheme == 'https' and (parsed.hostname.endswith('.blob.core.windows.net') or parsed.hostname.endswith('.actions.githubusercontent.com')), 'unexpected artifact host')
        # The provider URL gets NO GitHub Authorization header; never log it.
        with urllib.request.urlopen(location, timeout=120) as response:
            data = response.read(64 * 1024**2 + 1)
    except Exception as error:
        raise RuntimeError('artifact transport/identity failed: ' + type(error).__name__) from None
    require(len(data) == model['archive_bytes'] and hashlib.sha256(data).hexdigest() == model['archive_sha256'], 'actual provider ZIP changed')
    archive = output / 'provider.zip'
    archive.write_bytes(data)
    write(output / 'provider-metadata.json', metadata)
    found = {}
    with zipfile.ZipFile(archive) as zipped:
        infos = zipped.infolist()
        require(len(infos) < 100 and sum(x.file_size for x in infos) < 1024**3, 'artifact extraction budget exceeded')
        seen = set()
        for entry in infos:
            name = entry.filename
            require(not name.startswith('/') and '..' not in Path(name).parts and '\\' not in name and name not in seen, 'unsafe/duplicate artifact member')
            seen.add(name)
            mode = (entry.external_attr >> 16) & 0xffff
            require(not stat.S_ISLNK(mode) and (not stat.S_IFMT(mode) or stat.S_ISREG(mode) or stat.S_ISDIR(mode)), 'artifact special file')
            if entry.is_dir():
                continue
            body = zipped.read(entry)
            found[name] = hashlib.sha256(body).hexdigest()
            target = output / 'payload' / name
            target.parent.mkdir(parents=True, exist_ok=True)
            target.write_bytes(body)
            target.chmod(0o644)
    require(found == model['payloads'], 'exact verified original artifact member bytes changed')
    write(output / 'download-identity.json', {'artifact_id': model['artifact_id'], 'run_id': model['run_id'], 'zip_sha256': model['archive_sha256'], 'payloads': found, 'native_execution': False})


def sample(pid, started, binary, argv, cgroup):
    require(birth(pid) == started, 'native PID birth changed')
    require(Path('/proc/%d/exe' % pid).resolve(strict=True) == binary, 'native executable identity changed')
    command = Path('/proc/%d/cmdline' % pid).read_bytes().rstrip(b'\0').split(b'\0')
    require(command == [x.encode() for x in argv], 'native argv changed')
    tasks = []
    for task in sorted(Path('/proc/%d/task' % pid).iterdir()):
        try:
            fields = (task / 'stat').read_text().rpartition(') ')[2].split()
            tasks.append({'tid': int(task.name), 'comm': (task / 'comm').read_text().strip(), 'state': fields[0], 'user_ticks': int(fields[11]), 'system_ticks': int(fields[12]), 'wchan': (task / 'wchan').read_text().strip()})
        except FileNotFoundError:
            continue
    return {'monotonic': time.monotonic(), 'pid': pid, 'birth_ticks': started, 'tasks': tasks,
            'cgroup_cpu_stat': (cgroup / 'cpu.stat').read_text(), 'cgroup_memory_current': (cgroup / 'memory.current').read_text().strip()}


def stack_sample(proc, started, binary, native_argv, gdb, output, env):
    # Only the already-owned live child can be attached. No arguments or locals
    # are printed, and all automatic source Python/GDB scripts are disabled.
    require(proc.poll() is None and birth(proc.pid) == started, 'stack target birth changed')
    require(Path('/proc/%d/exe' % proc.pid).resolve(strict=True) == binary, 'stack target executable changed')
    require(Path('/proc/%d/cmdline' % proc.pid).read_bytes().rstrip(b'\0').split(b'\0') == [x.encode() for x in native_argv], 'stack target argv changed')
    argv = ['/usr/bin/sudo', '-n', '/usr/bin/timeout', '--signal=TERM', '--kill-after=2s', '10s',
            str(gdb), '--nx', '--nh', '--batch', '--quiet',
            '-iex', 'set print frame-arguments none', '-iex', 'set print entry-values no',
            '-iex', 'set print frame-info location',
            '-iex', 'set pagination off', '-iex', 'set debuginfod enabled off',
            '-iex', 'set auto-load python-scripts off', '-iex', 'set auto-load gdb-scripts off',
            '-iex', 'set auto-load local-gdbinit off', '-iex', 'set auto-load safe-path /usr/lib:/lib',
            '--pid', str(proc.pid), '-ex', 'thread apply all backtrace -no-filters 12', '-ex', 'detach', '-ex', 'quit']
    began = time.monotonic()
    record = {'argv': argv, 'target_pid': proc.pid, 'target_birth_ticks': started,
              'target_elf_sha256': digest(binary), 'gdb_sha256': digest(gdb),
              'maximum_attach_seconds': 10, 'kill_after_seconds': 2,
              'no_frame_arguments_or_locals': True}
    try:
        with (output / 'gdb.stdout').open('xb') as stdout, (output / 'gdb.stderr').open('xb') as stderr:
            # Root-owned timeout can terminate its root-owned debugger. The
            # user-owned native tracee is resumed below even on debugger error.
            child = subprocess.Popen(argv, env=env, stdout=stdout, stderr=stderr, start_new_session=True)
            record['debugger_supervisor_pid'] = child.pid
            try:
                record['exit_code'] = child.wait(timeout=15)
            except subprocess.TimeoutExpired:
                # This fallback targets only the debugger's owned process group.
                cleanup = subprocess.run(['/usr/bin/sudo', '-n', '/usr/bin/kill', '-KILL', '--', '-%d' % child.pid], env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, timeout=5)
                record['fallback_cleanup_exit_code'] = cleanup.returncode
                record['exit_code'] = child.wait(timeout=5)
                record['supervisor_timeout'] = True
            record['debugger_supervisor_reaped'] = child.poll() is not None
    finally:
        if proc.poll() is None:
            require(birth(proc.pid) == started, 'refuse unrelated tracee resume')
            os.kill(proc.pid, signal.SIGCONT)
            record['owned_tracee_continue_sent'] = True
        record['elapsed_seconds'] = time.monotonic() - began
        for name in ('gdb.stdout', 'gdb.stderr'):
            if (output / name).is_file():
                record[name + '_sha256'] = digest(output / name)
        write(output / 'stack-receipt.json', record)
    require(record.get('exit_code') == 0 and record.get('debugger_supervisor_reaped') and not record.get('supervisor_timeout'), 'bounded stack sampling failed')
    return record


def run(args, model):
    product, control, output = args.product.resolve(), args.controller.resolve(), args.out.resolve()
    require(str(product) == model['original_manifest_dir'], 'original compiler/runtime manifest directory must exist at exact path')
    require(not output.is_relative_to(product) and not output.is_relative_to(control), 'diagnostic output escapes external root')
    output.mkdir(parents=True, exist_ok=False)
    env = os.environ.copy()
    require('NANO_ARTIFACT_TOKEN' not in env, 'download token must not reach native service')
    group = next(row.split(':', 2)[2].strip() for row in Path('/proc/self/cgroup').read_text().splitlines() if row.startswith('0::'))
    cgroup = Path('/sys/fs/cgroup') / group.lstrip('/')
    require({name: (cgroup / name).read_text().strip() for name in ('memory.max', 'memory.swap.max')} == {'memory.max': str(24 * 1024**3), 'memory.swap.max': '0'}, '24GiB/noSwap scope required')
    fleet = Path('/home/gpc/HDB/sprint/coordination/build.lock').open('r+')
    fcntl.flock(fleet, fcntl.LOCK_EX | fcntl.LOCK_NB)
    before = {'product': source(product, model['entries'], model['product'], [], env), 'controller': source(control, model['controller_entries'], os.environ['GITHUB_SHA'], model['auxiliary_paths'], env)}
    write(output / 'source-before.json', before)
    saved = args.download.resolve() / 'payload' / 'codegraph-fixed-release-field'
    require(digest(args.download.resolve() / 'provider.zip') == model['archive_sha256'], 'downloaded ZIP changed before execution')
    for name, wanted in model['payloads'].items():
        require(digest(args.download.resolve() / 'payload' / name) == wanted, 'original artifact payload changed: ' + name)
    build = json.loads((saved / 'native-build-receipt.json').read_text())
    require(build['role_head'] == model['product'] and build['compiler_artifact']['manifest_path'] == str(product / 'Cargo.toml'), 'original artifact source/path mismatch')
    artifact = build['compiler_artifact']
    require(artifact['fresh'] is False and artifact['profile']['opt_level'] == '3' and artifact['profile']['debuginfo'] == 0, 'original optimized fresh compiler receipt changed')
    require(artifact['features'] == ['code-graph', 'default', 'encryption', 'ha-tier1', 'ring-crypto', 'vector-search'], 'original native features changed')
    require('rustc 1.95.0 ' in build['tools']['rustc']['version'], 'original compiler version changed')
    binary = (saved / 'native-test.elf').resolve()
    require(digest(binary) == model['binary_sha256'] == build['binary_sha256'], 'original native ELF changed')
    binary.chmod(0o755)
    env['CARGO_MANIFEST_DIR'] = str(product)
    argv = [str(binary)] + model['argv_tail']
    write(output / 'input.json', {'argv': argv, 'source': model['product'], 'CARGO_MANIFEST_DIR': str(product), 'native_sha256': model['binary_sha256'], 'original_compiler': build['tools'], 'sample_seconds': 5, 'native_seconds': 600, 'host': list(os.uname()), 'clock_ticks_per_second': os.sysconf('SC_CLK_TCK'), 'release_acceptance_pass': False})
    require(os.getloadavg()[0] < 6, 'unchanged field load gate not met')
    gdb = Path('/usr/bin/gdb').resolve(strict=True)
    gdb_identity = {'path': str(gdb), 'sha256': digest(gdb), 'version': read_command([str(gdb), '--version'], control, env).decode()}
    require(gdb_identity['version'].startswith('GNU gdb '), 'actual GNU gdb required')
    write(output / 'gdb-identity.json', gdb_identity)
    proc = None
    started = None
    result = {'release_acceptance_pass': False, 'scope': 'owned process state diagnostic; original workload/600 deadline unchanged', 'errors': []}
    try:
        with (output / 'native.stdout').open('xb') as stdout, (output / 'native.stderr').open('xb') as stderr, (output / 'samples.jsonl').open('x') as samples:
            proc = subprocess.Popen(argv, cwd=product, env=env, stdout=stdout, stderr=stderr, start_new_session=True)
            started = birth(proc.pid)
            native_began = time.monotonic()
            deadline = native_began + 600
            stacked = False
            while proc.poll() is None and time.monotonic() < deadline:
                try:
                    observation = sample(proc.pid, started, binary, argv, cgroup)
                    samples.write(json.dumps(observation) + '\n')
                    samples.flush()
                except FileNotFoundError:
                    if proc.poll() is None:
                        raise
                    break
                if not stacked and time.monotonic() - native_began >= 180:
                    stacked = True
                    try:
                        stack_sample(proc, started, binary, argv, gdb, output, env)
                    except Exception as error:
                        result['errors'].append({'phase': 'stack-sample', 'type': type(error).__name__, 'message': str(error)})
                time.sleep(min(5, max(0, deadline - time.monotonic())))
            result['timed_out'] = proc.poll() is None
    except BaseException as error:
        result['errors'].append({'type': type(error).__name__, 'message': str(error)})
    finally:
        if proc is not None:
            if proc.poll() is None:
                require(birth(proc.pid) == started, 'refuse unrelated PID cleanup')
                os.killpg(proc.pid, signal.SIGTERM)
                try:
                    proc.wait(timeout=15)
                except subprocess.TimeoutExpired:
                    require(birth(proc.pid) == started, 'refuse unrelated PID kill')
                    os.killpg(proc.pid, signal.SIGKILL)
                    proc.wait(timeout=10)
            else:
                proc.wait()
            result.update(pid=proc.pid, birth_ticks=started, exit_code=proc.returncode, leader_reaped=True)
        after = {'product': source(product, model['entries'], model['product'], [], env), 'controller': source(control, model['controller_entries'], os.environ['GITHUB_SHA'], model['auxiliary_paths'], env)}
        write(output / 'source-after.json', after)
        require(before == after and digest(binary) == model['binary_sha256'], 'final source/native bytes changed')
        require(digest(gdb) == gdb_identity['sha256'], 'debugger bytes changed')
        for name in ('native.stdout', 'native.stderr', 'samples.jsonl'):
            result[name + '_sha256'] = digest(output / name)
        write(output / 'result.json', result)
    # A timeout remains a real red; state samples never turn it into acceptance.
    return 0 if not result['errors'] and result.get('exit_code') == 0 and not result.get('timed_out') else 1


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--model-sha256', required=True)
    parser.add_argument('--download-only', action='store_true')
    parser.add_argument('--download', type=Path, required=True)
    parser.add_argument('--product', type=Path)
    parser.add_argument('--controller', type=Path)
    parser.add_argument('--out', type=Path)
    args = parser.parse_args()
    require(digest(HERE / 'model.json') == args.model_sha256, 'model bootstrap changed')
    model = json.loads((HERE / 'model.json').read_text())
    if args.download_only:
        download(args.download.resolve(), model)
        return 0
    require(args.product is not None and args.controller is not None and args.out is not None, 'native diagnostic paths required')
    return run(args, model)


if __name__ == '__main__':
    raise SystemExit(main())
