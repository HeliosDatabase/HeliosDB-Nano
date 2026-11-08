#!/usr/bin/env python3
"""Portable B1 MySQL native rerun against independently verified source31 binary."""
import argparse
import ast
import fcntl
import hashlib
import io
import json
import os
from pathlib import Path, PurePosixPath
import signal
import socket
import subprocess
import sys
import tarfile
import time
import zipfile

HERE = Path(__file__).resolve().parent
PRODUCER_HEAD = 'ae46e4b95ed578efa9d543809b4ea67e64096b68'
PRODUCER_RUN = 42462448705
PROVIDER_ZIP_SHA = 'a606306760e2e2b30a921a5eb58f3b5cfc9fe953d78a1398341a20018006bc83'
BINARY_SHA = '5116cee940ca93e0a63dd84ba27792a609a9746a841b46c1132481df2153c45f'
ARCHIVE = 'heliosdb-nano-v4.42.0-x86_64-unknown-linux-gnu.tar.gz'
MODEL = '.github/binary-preflight-source28.json'
LEGACY = {
    MODEL: 'ed47e15814dd3d39b072f5fb98aa632b85cd4b11177277e2796aee7cf3991038',
    '.github/scripts/binary_preflight.py': 'ea23902c06733505a8cbf093f61d88c3f93162fc48f415257b76d3b319a8fa84',
    '.github/workflows/binary-preflight.yml': 'd19d830c9e2797567e9828bcf1bbfc4cccda6ac3e50096c0b60628e48c35de2b',
}
PAYLOAD = {
    'SHA256SUMS': '4998d534c7193d91744b87fd057822f03c4113513034cb376d3f5693eb767187',
    'aggregate-source.json': 'e06dd4e39c60a6496f12926342baa42073a55068b70bb85a468a8d7dcd9f32c6',
    'heliosdb-nano-v4.42.0-aarch64-apple-darwin.tar.gz': '2397e3774e2a79b9509c4dd65b5098386ba589df0107345ead9cbd711a869d72',
    'heliosdb-nano-v4.42.0-aarch64-unknown-linux-gnu.tar.gz': '5e7f21d0934ece2ec292e96410c399b3b8c17a46d6444433e421ad789f3d61e2',
    'heliosdb-nano-v4.42.0-x86_64-pc-windows-msvc.zip': 'f40bfd7c473d92a19e49c9551b49504d137bd0448f96bfe9d0677799d74aa7d0',
    ARCHIVE: 'a5c4572ca94f050978541eb16f7be7b2fffd1bd51c1d13034bef4e8a1748e160',
    'summary.json': '959e6432d6d5eda26c04e7e971499ac6fc5ec260ac2ffb1f655672b0aed066ba',
}

NEW = {'.github/4420-mysql-b1/controller.py', '.github/4420-mysql-b1/mysql_worker.py', '.github/4420-mysql-b1/pymysql-1.2.0-py3-none-any.whl', '.github/workflows/mysql-b1-native.yml'}
WHEEL_SHA = '62169ce6d5510f08e140c5e7990ee884a9764024e4a9a27b2cc11f1099322ae0'
WORKER_SHA = '82938ee128713ff6e6d2dbaa4d69a88046f315b369dbab36d9830851aea0ff0c'
CLIENT_MEMBERS = {'pymysql/__init__.py': 'ccd6a6c1edebfd58d3952fb3d1baa5a4d4f872518f8d22de2a17c6f10ddf659f', 'pymysql/_auth.py': 'edb2059c9ee526b14484a2c49c71a8d7e87b139727641db6d75284d6f6ad0404', 'pymysql/charset.py': '6381a0303c67d18cfef7d370b2d7c22de09fa1115dc32c16a07ae7e469ef8219', 'pymysql/connections.py': '649bb1e0c3e3e797b2b6cd4aa35b2084edd75c4b2f6c5b1900ae696285b9acce', 'pymysql/converters.py': 'f37107d6a14a2c54a5903010c976c750352655ac26bdcea3408aaafaac104dec', 'pymysql/cursors.py': 'bb302b0e6f59813ecbddf03ce6cac205dbe1a92cd0d394e9368f2d248508cdf7', 'pymysql/err.py': '6d1bdc7ec7a195d5ec9fc746b30e0d74f959026f03173459038a86496e0e8eb5', 'pymysql/optionfile.py': '790a33e9ce37caf987b69e4c23d4c1d863d176882038b7a6714580064b1fbad9', 'pymysql/protocol.py': '683f8f18f45873091223a649a095995512a7f47fc0d1fef429f3c3bbae6dab4a', 'pymysql/times.py': 'fea5e00da630b079edbe920a48a5e9d6bad8220b6ae99f692f22e73b65cda0bd', 'pymysql/constants/CLIENT.py': '492bcc14f6424d5314d5459ae333ab7e160c0dd476c06da64b41391b32610ec8', 'pymysql/constants/COMMAND.py': '4c621301470d5a5ab61b0836c2fe542b6ca47537782d84e4fc470924e0a91887', 'pymysql/constants/CR.py': '424df915644cc511dde926bd0822004cc87b8de811df19cb76b75a0424f47534', 'pymysql/constants/ER.py': '9f0a97febd28e2698de02c66ecd551c89393568bffe466e5e697867baa33e5f9', 'pymysql/constants/FIELD_TYPE.py': 'cad1738009c699bf61bddb019672baf2a759bff6ba8d81485d3e954806fad33f', 'pymysql/constants/FLAG.py': '172f8fac22e7508edfc7fa395b2a589d403358033413d779c8bf1f15154a7df6', 'pymysql/constants/SERVER_STATUS.py': '9b6f08ab92460850962e169f13bdfe88ebf0ffd8031aa9f2b56dcd907a5bba00', 'pymysql/constants/__init__.py': 'e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855', 'pymysql-1.2.0.dist-info/licenses/LICENSE': '314120dc65f0800f738a4b31400c76ee14decd1fbe77ce9c3540a421b86ea7b6', 'pymysql-1.2.0.dist-info/METADATA': 'a3256c895206725e3930f1c9f9fc4e6cf6662c9933c498759c0ed992250cf57f', 'pymysql-1.2.0.dist-info/WHEEL': '69e6228a0d35958183cc1812f07c565ce837b95eb51bd8a33acba9fa4f68d6c9', 'pymysql-1.2.0.dist-info/top_level.txt': '20a955f9fe28f74b0e76731de8706fa349769ea7c938653393065e684106b918', 'pymysql-1.2.0.dist-info/RECORD': 'b37a818a71f3b36f00c3deac2b35ac181731de4992a03b19c252dd163a56fc6b'}
PHASES = ['immutable_forks', 'isolated_same_primary_key_writes', 'isolated_reads', 'overlapping_transactions_rollback', 'overlapping_transactions_commit', 'dropped_id_refusal_and_protocol_reset']
ADMISSION_MARKER = 'history-guarded standby does not support MySQL'


def sha(data):
    return hashlib.sha256(data).hexdigest()

def write_json(path, value):
    path.write_text(json.dumps(value, indent=2, sort_keys=True) + '\n')

def git(root, *args):
    return subprocess.check_output(['git', '-C', str(root), *args], timeout=15)

def source_check(root):
    head = git(root, 'rev-parse', 'HEAD').decode().strip()
    if head != os.environ.get('GITHUB_SHA'):
        raise ValueError('checkout HEAD differs from event SHA')
    if git(root, 'diff', '--name-only', 'HEAD') or git(root, 'diff', '--cached', '--name-only'):
        raise ValueError('tracked source changed')
    for name, digest in LEGACY.items():
        if sha((root / name).read_bytes()) != digest:
            raise ValueError('legacy auxiliary hash mismatch: ' + name)
    model = json.loads((root / MODEL).read_text())
    expected = model['entries']
    if len(expected) != 4049 or model['regular_files'] != 4045:
        raise ValueError('unexpected canonical source count')
    entries = {}
    for record in git(root, 'ls-tree', '-rz', '--full-tree', 'HEAD').split(b'\0'):
        if not record:
            continue
        info, name = record.split(b'\t', 1)
        mode, kind, oid = info.decode().split()
        if kind != 'blob':
            raise ValueError('non-blob committed tree entry')
        entries[name.decode()] = {'mode': mode, 'oid': oid}
    if set(entries) != set(expected) | set(LEGACY) | NEW or len(entries) != 4056:
        raise ValueError('unexpected committed paths')
    physical = {}
    for name, record in expected.items():
        if entries[name] != {k: record[k] for k in ('mode', 'oid')}:
            raise ValueError('canonical Git blob/mode changed: ' + name)
        path = root / name
        if record['mode'] == '120000':
            if not path.is_symlink() or os.readlink(path) != record['target_text']:
                raise ValueError('canonical symlink target changed: ' + name)
        else:
            if path.is_symlink() or not path.is_file():
                raise ValueError('regular source missing or is symlink: ' + name)
            physical[name] = sha(path.read_bytes())
            if physical[name] != record['sha256']:
                raise ValueError('physical source bytes changed: ' + name)
    for name in set(LEGACY) | NEW:
        if entries[name]['mode'] != '100644' or (root / name).is_symlink():
            raise ValueError('invalid auxiliary mode: ' + name)
        if git(root, 'show', 'HEAD:' + name) != (root / name).read_bytes():
            raise ValueError('physical auxiliary differs from committed blob: ' + name)
    unknown = git(root, 'ls-files', '--others', '--exclude-standard', '-z').split(b'\0')
    if any(x and not x.decode().startswith(('.mysql-artifacts/',)) for x in unknown):
        raise ValueError('unexpected untracked file')
    return {'head': head, 'git_entries': entries,
            'regular_file_count': len(physical), 'source_input_count': len(expected),
            'physical_sha256_digest': sha(json.dumps(physical, sort_keys=True).encode()),
            'model_sha256': LEGACY[MODEL], 'product_source': 'source31',
            'extra_auxiliary_paths': sorted(set(LEGACY) | NEW)}

def artifact_check(root, incoming, out, source):
    names = {p.name for p in incoming.iterdir() if p.is_file()}
    if names != set(PAYLOAD) or any(p.is_symlink() or not p.is_file() for p in incoming.iterdir()):
        raise ValueError('fixed artifact payload must contain exactly seven regular files')
    for name, digest in PAYLOAD.items():
        if sha((incoming / name).read_bytes()) != digest:
            raise ValueError('fixed artifact hash mismatch: ' + name)
    aggregate = json.loads((incoming / 'aggregate-source.json').read_text())
    summary = json.loads((incoming / 'summary.json').read_text())
    old_entries = {k: v for k, v in source['git_entries'].items() if k not in NEW}
    if (aggregate['head'] != PRODUCER_HEAD or not aggregate['success']
            or aggregate['git_entries'] != old_entries
            or aggregate['source_input_count'] != 4049 or aggregate['regular_file_count'] != 4045
            or aggregate['expected_manifest_sha256'] != LEGACY[MODEL]
            or summary['head'] != PRODUCER_HEAD or not summary['success']
            or summary['target_count'] != 4 or len(summary['targets']) != 4):
        raise ValueError('producer source/aggregate identity mismatch')
    target = next(x for x in summary['targets'] if x['target'] == 'x86_64-unknown-linux-gnu')
    if target['binary_sha256'] != BINARY_SHA or target['sha256'] != PAYLOAD[ARCHIVE]:
        raise ValueError('producer native identity mismatch')
    sums = dict((name, digest) for digest, name in
                (line.split() for line in (incoming / 'SHA256SUMS').read_text().splitlines()))
    if sums != {x['archive']: x['sha256'] for x in summary['targets']}:
        raise ValueError('aggregate checksum map differs')
    with tarfile.open(incoming / ARCHIVE, 'r:gz') as archive:
        members = archive.getmembers()
        if {x.name for x in members} != {'heliosdb-nano', 'LICENSE', 'README.md'} or len(members) != 3 or any(not x.isfile() for x in members):
            raise ValueError('unexpected native archive members')
        raw = archive.extractfile('heliosdb-nano').read()
    if sha(raw) != BINARY_SHA:
        raise ValueError('native binary digest mismatch')
    binary = out / 'heliosdb-nano'
    binary.write_bytes(raw)
    binary.chmod(0o755)
    version = subprocess.run([str(binary), '--version'], capture_output=True, timeout=8)
    if version.returncode != 0 or version.stdout.decode().strip() != 'heliosdb-nano 4.42.0':
        raise ValueError('native version mismatch: ' + repr(version.stdout))
    write_json(out / 'artifact-identity.json', {
        'producer_run': PRODUCER_RUN, 'producer_head': PRODUCER_HEAD,
        'provider_artifact_id': 13610852764, 'provider_zip_sha256_root_verified': PROVIDER_ZIP_SHA,
        'provider_zip_recomputed_here': False, 'download_action_payload_sha256': PAYLOAD,
        'native_sha256': BINARY_SHA, 'version_stdout': version.stdout.decode(),
        'version_stderr': version.stderr.decode(), 'version_exit': version.returncode,
        'source_join': '4049 canonical Git entries and 4045 physical inputs identical to producer; seven declared auxiliaries only',
    })
    return binary

def birth(pid):
    return int(Path('/proc/{}/stat'.format(pid)).read_text().rsplit(')', 1)[1].split()[19])

def owned(proc, started, argv, binary):
    return (proc.poll() is None and birth(proc.pid) == started
            and Path('/proc/{}/cmdline'.format(proc.pid)).read_bytes().split(b'\0')[:-1] == [os.fsencode(x) for x in argv]
            and Path('/proc/{}/exe'.format(proc.pid)).resolve() == binary.resolve())

def owns_listener(pid, port):
    inodes = set()
    for fd in Path('/proc/{}/fd'.format(pid)).iterdir():
        try:
            link = os.readlink(fd)
            if link.startswith('socket:['):
                inodes.add(link[8:-1])
        except FileNotFoundError:
            pass
    return any(len(v) > 9 and v[1] == '0100007F:{:04X}'.format(port) and v[3] == '0A' and v[9] in inodes
               for v in (line.split() for line in Path('/proc/net/tcp').read_text().splitlines()[1:]))

def require(condition, message):
    if not condition:
        raise ValueError(message)


def load(path):
    return json.loads(Path(path).read_text())


def cleanup_owned(process, owner):
    if process.poll() is not None:
        process.wait()
        return {'owned': True, 'already_exited': True, 'exit': process.returncode}
    require(owner(), 'refusing cleanup: child identity changed')
    process.terminate()
    try:
        process.wait(timeout=15)
        return {'owned': True, 'term': True, 'exit': process.returncode}
    except subprocess.TimeoutExpired:
        require(owner(), 'refusing KILL: child identity changed')
        process.kill()
        process.wait(timeout=10)
        return {'owned': True, 'term': True, 'kill': True, 'exit': process.returncode}


def start_process(command, environment, cwd, log):
    process = subprocess.Popen(command, cwd=cwd, env=environment, stdout=log, stderr=subprocess.STDOUT)
    ticks = birth(process.pid)
    def owner():
        try:
            return owned(process, ticks, command, Path(command[0]))
        except (OSError, ValueError):
            return False
    return process, ticks, owner


def client_guard(client_root):
    actual = {str(p.relative_to(client_root)) for p in client_root.rglob('*') if p.is_file() or p.is_symlink()}
    require(actual == set(CLIENT_MEMBERS), 'native client file census changed')
    for name, expected in CLIENT_MEMBERS.items():
        p = client_root / name
        require(not p.is_symlink() and sha(p.read_bytes()) == expected, 'native client payload changed: ' + name)
    require(sha((HERE / 'mysql_worker.py').read_bytes()) == WORKER_SHA, 'actual-pass worker source changed')
    require(sha((HERE / 'pymysql-1.2.0-py3-none-any.whl').read_bytes()) == WHEEL_SHA, 'original wheel changed')


def prepare_client(output, environment):
    wheel = HERE / 'pymysql-1.2.0-py3-none-any.whl'
    require(sha(wheel.read_bytes()) == WHEEL_SHA, 'original PyMySQL wheel hash mismatch')
    client_root = output / 'client'
    client_root.mkdir()
    with zipfile.ZipFile(wheel) as packed:
        names = [m.filename for m in packed.infolist()]
        require(len(names) == len(set(names)) and set(names) == set(CLIENT_MEMBERS), 'wheel member census mismatch')
        for member in packed.infolist():
            parts = PurePosixPath(member.filename).parts
            require(not member.filename.startswith('/') and '..' not in parts and not member.is_dir()
                    and ((member.external_attr >> 16) & 0o170000) != 0o120000, 'unsafe wheel member')
            data = packed.read(member)
            require(sha(data) == CLIENT_MEMBERS[member.filename], 'wheel member hash mismatch')
            target = client_root / member.filename
            target.parent.mkdir(parents=True, exist_ok=True)
            target.write_bytes(data)
    client_guard(client_root)
    code = "import sys,json,importlib.metadata;sys.path.insert(0,sys.argv[1]);import pymysql;print(json.dumps({'python':sys.executable,'python_version':sys.version,'python_realpath':str(__import__('pathlib').Path(sys.executable).resolve()),'package':str(__import__('pathlib').Path(pymysql.__file__).resolve().parent),'distribution_version':importlib.metadata.version('PyMySQL'),'VERSION_STRING':pymysql.VERSION_STRING,'compatibility_version':pymysql.__version__,'optimize':sys.flags.optimize}))"
    checked = subprocess.run([sys.executable, '-S', '-c', code, str(client_root)], env=environment,
                             capture_output=True, text=True, timeout=8)
    require(checked.returncode == 0, 'original PyMySQL import/version probe failed: ' + checked.stderr)
    result = json.loads(checked.stdout)
    require(result['distribution_version'] == '1.2.0' and result['VERSION_STRING'] == '1.2.0'
            and result['compatibility_version'] == '2.2.8' and result['optimize'] == 0, 'native reference client version differs')
    require(Path(result['package']).resolve() == client_root / 'pymysql', 'wrong native client loaded')
    result.update(wheel_sha256=WHEEL_SHA, original_worker_sha256=WORKER_SHA,
                  python_sha256=sha(Path(result['python_realpath']).read_bytes()),
                  portable_boundary='Same original PyMySQL wheel/source bytes; actual Ubuntu Python3.11 identity recorded separately from old Rocky Python3.9')
    write_json(output / 'client-identity.json', result)
    return result, client_root

def port():
    sock=socket.socket();sock.bind(('127.0.0.1',0));return sock,sock.getsockname()[1]

def verify_worker_result(result, client):
    expected={'worker_phases': PHASES}
    require(result.get('complete') is True and result.get('errors')==[], 'MySQL worker did not complete exact checks')
    require([phase.get('name') for phase in result.get('phases',[])]==expected['worker_phases']
            and all(phase.get('completed') is True for phase in result['phases']), 'MySQL phase proof missing/duplicated/reordered')
    transactions=[phase for phase in result['phases'] if phase.get('overlapping_explicit_transactions')]
    require(len(transactions)==2 and all(phase.get('connections')==3 for phase in transactions), 'concurrent transaction proof incomplete')
    connected=[event for event in result.get('events',[]) if event.get('connected') is True]
    require(len(connected)==3 and {event.get('actor') for event in connected}=={'main','a','b'}
            and len({event.get('connection_id') for event in connected})==3, 'three distinct live MySQL connection identities missing')
    resets=[event for event in result['events'] if event.get('command')=='COM_RESET_CONNECTION']
    require(len(resets)==2 and [event.get('actor') for event in resets]==['a','b']
            and all(event.get('command_byte')==31 and type(event.get('server_status')) is int and event['server_status']&1==0 for event in resets), 'actual protocol reset/transaction-clear proof incomplete')
    refused=[event for event in result['events'] if event.get('error_code') is not None]
    require(len(refused)==3 and all(event.get('actor')=='a' and event.get('error_code')==1105
            and 'USE BRANCH main or another branch' in event.get('error_message','') and 'dropped' in event['error_message']
            and 'mysql_a' in event['error_message'] for event in refused), 'stale-session exact error observations differ')
    require(Path(result['python_executable']).resolve()==Path(client['python_realpath']) and result['python_version']==client['python_version']
            and Path(result['pymysql_file']).resolve()==Path(client['package'])/'__init__.py'
            and result['pymysql_distribution_version']==client['distribution_version'], 'actual Python/PyMySQL identity differs')

def admission_matches(record):
    return bool(type(record.get('exit_code')) is int and record['exit_code']!=0 and not record.get('timed_out') and record.get('marker_seen') is True
                and record.get('ever_created_data_or_socket') is False and record.get('data_exists') is False and record.get('socket_exists') is False
                and record.get('primary_connection_attempted') is False)

def admission_case(binary,environment,root,kind):
    root.mkdir();pg,pg_port=port();mysql,mysql_port=port()
    pg.close();mysql.close()
    primary,primary_port=port();primary.listen(1);primary.setblocking(False)
    data=root/'data';sock=root/'owned-mysql.sock'
    command=[binary,'start','--data-dir',str(data),'--port',str(pg_port),'--listen','127.0.0.1','--http-port','0',
             '--replication-role','standby','--primary-host','127.0.0.1:'+str(primary_port)]
    command += ['--mysql','--mysql-listen','127.0.0.1:'+str(mysql_port)] if kind=='tcp' else ['--mysql-socket',str(sock)]
    record={'kind':kind,'command':command,'data':str(data),'socket':str(sock),'ever_created_data_or_socket':False,'primary_endpoint':'127.0.0.1:'+str(primary_port),'primary_connection_attempted':False,'primary_peers':[],'release_acceptance_pass':False}
    process=None;owner=None
    def check_primary():
        while True:
            try:connection,peer=primary.accept()
            except BlockingIOError:break
            else:
                record['primary_connection_attempted']=True
                record['primary_peers'].append(list(peer))
                connection.close()
    try:
        with (root/'daemon.log').open('xb') as log:
            process,ticks,owner=start_process(command,environment,root,log)
            record.update(pid=process.pid,start_ticks=ticks)
            deadline=time.monotonic()+10
            while process.poll() is None and time.monotonic()<deadline:
                record['ever_created_data_or_socket'] |= data.exists() or sock.exists()
                check_primary()
                time.sleep(.01)
            record['timed_out']=process.poll() is None
            if record['timed_out']:record['cleanup']=cleanup_owned(process,owner)
            else:record['exit_code']=process.returncode
            check_primary()
    finally:
        try:
            if process is not None and process.poll() is None:record['cleanup']=cleanup_owned(process,owner or (lambda:False))
            check_primary()
        finally:
            primary.close();pg.close();mysql.close()
    text=(root/'daemon.log').read_text(errors='replace')
    record.update(log=str(root/'daemon.log'),log_sha256=sha((root/'daemon.log').read_bytes()),marker_seen=ADMISSION_MARKER in text,
                  data_exists=data.exists(),socket_exists=sock.exists())
    record['passed']=admission_matches(record)
    write_json(root/'observed.json',record)
    return record


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--artifact-dir', required=True, type=Path)
    parser.add_argument('--out', required=True, type=Path)
    args = parser.parse_args()
    root = Path.cwd().resolve()
    output = args.out.resolve()
    require(not output.is_relative_to(root), 'output/client/runtime must be outside immutable checkout')
    output.mkdir(parents=True, exist_ok=False)
    report = {'schema': 'source31-native-b1-mysql-v1', 'b1_mysql_acceptance_pass': False,
              'release_acceptance_pass': False, 'errors': [], 'admission': [],
              'actual_source18_reference_receipt_sha256': '9102a84a9f79ff3dd6c4f5a12f2cfa55c697be32c9bac44f232645abfe241667',
              'note': 'Original real PyMySQL fixture/oracles; fresh source31 binary. No rawKV or physical acceptance claim.'}
    daemon = worker = None
    daemon_owner = worker_owner = None
    client = client_root = before = binary = None
    fleet = None
    environment = {k: os.environ[k] for k in ('PATH', 'HOME', 'USER', 'LOGNAME', 'LANG') if k in os.environ}
    environment.update(TZ='UTC', RUST_LOG='warn', NO_COLOR='1', HELIOSDB_NANO_STATEMENT_OFFLOAD='on',
                       PYTHONDONTWRITEBYTECODE='1')
    try:
        # The original fixture is tiny, but its future execution remains inside
        # the same explicit no-swap resource admission as the source18 run.
        group = next(x.split(':', 2)[2].strip() for x in Path('/proc/self/cgroup').read_text().splitlines() if x.startswith('0::'))
        cg = Path('/sys/fs/cgroup') / group.lstrip('/')
        limits = {n: (cg / n).read_text().strip() for n in ('memory.max', 'memory.swap.max')}
        require(limits == {'memory.max': str(24 * 1024**3), 'memory.swap.max': '0'}, '24GiB/no-swap native service required')
        fleet = Path('/home/gpc/HDB/sprint/coordination/build.lock').open('r+')
        fcntl.flock(fleet, fcntl.LOCK_EX | fcntl.LOCK_NB)
        write_json(output / 'host.json', {'cgroup': group, 'limits': limits, 'uname': list(os.uname()),
                   'meminfo': Path('/proc/meminfo').read_text(), 'scope': 'owned fresh hosted VM, no shared-host service'})
        before = source_check(root)
        write_json(output / 'source-before.json', before)
        binary = artifact_check(root, args.artifact_dir.resolve(), output, before)
        client, client_root = prepare_client(output, environment)
        for kind in ('tcp', 'uds'):
            report['admission'].append(admission_case(str(binary), environment, output / ('standby-' + kind), kind))
        config = output / 'memory.toml'
        config.write_text('[storage]\nmemory_only=true\ntime_travel_enabled=true\ndurable_commit=false\n')
        pg_sock, pg_port = port()
        mysql_sock, mysql_port = port()
        pg_sock.close()
        mysql_sock.close()
        argv = [str(binary), 'start', '--memory', '--listen', '127.0.0.1', '--port', str(pg_port),
                '--auth', 'trust', '--http-port', '0', '--mysql', '--mysql-listen', '127.0.0.1:' + str(mysql_port),
                '--config', str(config)]
        report.update(server_command=argv, mysql_port=mysql_port, pg_port=pg_port,
                      memory_config_sha256=sha(config.read_bytes()), reference_worker_sha256=WORKER_SHA)
        with (output / 'daemon.log').open('xb') as daemon_log, (output / 'worker.log').open('xb') as worker_log:
            daemon, ticks, daemon_owner = start_process(argv, environment, output, daemon_log)
            identity = {'pid': daemon.pid, 'start_ticks': ticks, 'command': argv}
            report['daemon_identity'] = identity
            deadline = time.monotonic() + 30
            while time.monotonic() < deadline:
                require(daemon.poll() is None and daemon_owner(), 'owned daemon lost before startup')
                if owns_listener(daemon.pid, mysql_port) and owns_listener(daemon.pid, pg_port):
                    break
                time.sleep(.1)
            else:
                raise TimeoutError('owned PG/MySQL listener startup exceeded30s')
            server_identity = output / 'server-identity.json'
            write_json(server_identity, identity)
            worker_result = output / 'mysql-worker.json'
            worker_argv = [client['python'], '-S', str(HERE / 'mysql_worker.py'), '--execute', '--port', str(mysql_port),
                           '--server-identity', str(server_identity), '--client-package', client['package'], '--out', str(worker_result)]
            worker_env = dict(environment, PYTHONPYCACHEPREFIX=str(output / 'new-python-cache'))
            worker, worker_ticks, worker_owner = start_process(worker_argv, worker_env, output, worker_log)
            report['worker_identity'] = {'pid': worker.pid, 'start_ticks': worker_ticks, 'command': worker_argv}
            deadline = time.monotonic() + 150
            while worker.poll() is None and time.monotonic() < deadline:
                require(daemon.poll() is None and daemon_owner() and owns_listener(daemon.pid, mysql_port),
                        'owned daemon/listener lost during real MySQL phases')
                time.sleep(.1)
            if worker.poll() is None:
                report['worker_timeout'] = True
                report['worker_cleanup'] = cleanup_owned(worker, worker_owner)
            else:
                report['worker_exit_code'] = worker.returncode
            require(report.get('worker_exit_code') == 0 and worker_result.is_file(), 'native MySQL worker failed/timed out/no complete receipt')
            result = load(worker_result)
            verify_worker_result(result, client)
            report.update(worker_complete=True, worker_result_sha256=sha(worker_result.read_bytes()))
    except BaseException as error:
        report['errors'].append({'type': type(error).__name__, 'message': str(error)})
    finally:
        for name, process, owner in [('worker', worker, worker_owner), ('daemon', daemon, daemon_owner)]:
            if process is not None:
                try:
                    report[name + '_cleanup'] = cleanup_owned(process, owner or (lambda: False))
                except BaseException as error:
                    report['errors'].append({'scope': name + '-cleanup', 'type': type(error).__name__, 'message': str(error)})
        try:
            if before is not None:
                after = source_check(root)
                write_json(output / 'source-after.json', after)
                require(before == after, 'source/auxiliary identity moved during MySQL proof')
                report['source_guards_pass'] = True
            if binary is not None:
                require(sha(binary.read_bytes()) == BINARY_SHA, 'native binary changed during MySQL proof')
                for name, digest in PAYLOAD.items():
                    require(sha((args.artifact_dir / name).read_bytes()) == digest, 'producer payload changed: ' + name)
            if client is not None:
                client_guard(client_root)
                require(sha(Path(client['python_realpath']).read_bytes()) == client['python_sha256'], 'actual portable client interpreter changed')
                report['client_guards_pass'] = True
        except BaseException as error:
            report['errors'].append({'scope': 'final-guard', 'type': type(error).__name__, 'message': str(error)})
        report['b1_mysql_acceptance_pass'] = bool(not report['errors'] and report.get('worker_complete') is True
            and report.get('source_guards_pass') is True and report.get('client_guards_pass') is True
            and len(report['admission']) == 2 and all(x['passed'] for x in report['admission'])
            and report.get('daemon_cleanup', {}).get('owned') is True and report.get('worker_cleanup', {}).get('owned') is True)
        write_json(output / 'observed.json', report)
    print(json.dumps({'b1_mysql_acceptance_pass': report['b1_mysql_acceptance_pass'], 'release_acceptance_pass': False,
                      'errors': report['errors']}), flush=True)
    return 0 if report['b1_mysql_acceptance_pass'] else 1


if __name__ == '__main__':
    signal.signal(signal.SIGTERM, lambda *_: (_ for _ in ()).throw(KeyboardInterrupt('owned service stopped')))
    raise SystemExit(main())
