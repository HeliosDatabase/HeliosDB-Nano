#!/usr/bin/env python3
"""Only frozen V8 public smoke, separate controller/product, no release acceptance."""
import argparse
import fcntl
import hashlib
import json
import os
from pathlib import Path
import signal
import subprocess
import sys
import time

HERE = Path(__file__).resolve().parent
BUNDLE_SHA256 = 'f80f9a18a3c2d4d008049ddb8e1261a393c8dcf2aff819c4ea89dfe4a1d6572f'
PRODUCT = '00b58922b030797da5b262dd680442eb93f37038'
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

def verify_bundle():
    path = HERE / 'bundle/bundle-sha256.json'
    require(digest(path) == BUNDLE_SHA256, 'V8 bundle manifest differs')
    pins = json.loads(path.read_bytes())
    require(len(pins) == 24, 'V8 member count differs')
    for name, expected in pins.items():
        require(digest(path.parent / name) == expected, 'V8 helper changed: ' + name)


def judge(stage):
    require(stage['stage'] == 'public-perf-smoke' and not stage['failed'], 'public smoke native stage failed')
    require(len(stage['commands']) == 1, 'exactly one frozen public command required')
    require(all(item['unchanged'] for item in stage['manifest_inputs'].values()), 'stage capture identity moved')
    row = stage['commands'][0]
    require(row['argv'] == ['bash','benches/public/ci_perf_smoke.sh'] and row['exit'] == 0
            and not row['timed_out'] and not row['completeness_problems'], 'original public command failed/partial')
    smoke = row['public_perf_smoke']
    native = smoke['native_proof']
    require(native['schema'] == 'nano-public-perf-native-v1' and native['native_tests'] == 6
            and native['repetitions'] == 3 and native['workloads'] == 12
            and len(smoke['labels']) == 12 and smoke['threshold'] == 2.5
            and smoke['repetitions'] == 3 and native['release_acceptance_pass'] is False,
            'structured native proof contract differs')
    return {'exact_public_smoke_commands':1, 'native_parents':6, 'repetitions':3, 'labels':12,
            'threshold':2.5, 'native_proof':native, 'release_acceptance_pass':False}


def main():
    parser=argparse.ArgumentParser()
    for name in ('controller','product','out'):
        parser.add_argument('--'+name,type=Path,required=True)
    parser.add_argument('--model-sha256',required=True)
    args=parser.parse_args()
    control,product,out=args.controller.resolve(),args.product.resolve(),args.out.resolve()
    require(not out.is_relative_to(control) and not out.is_relative_to(product),'output must be external')
    out.mkdir(parents=True,exist_ok=False)
    env=os.environ.copy()
    env.update(RUSTUP_TOOLCHAIN='1.95.0',CARGO_BUILD_JOBS='2',CMAKE_BUILD_PARALLEL_LEVEL='2',
               RUST_TEST_THREADS='1',CARGO_TERM_COLOR='never',NANO_FINAL_GATE_BOUNDED='1',
               CARGO_TARGET_DIR=str(out/'fresh-target'),CARGO_PROFILE_TEST_DEBUG='0',CARGO_PROFILE_DEV_DEBUG='0')
    result={'commands_green':False,'errors':[],'release_acceptance_pass':False}
    model,before,tools,fleet=None,None,None,None
    started=time.monotonic()
    try:
        require(digest(HERE/'model.json') == args.model_sha256,'model bootstrap differs')
        model=json.loads((HERE/'model.json').read_bytes())
        require(model['product_head'] == PRODUCT,'wrong final product')
        verify_bundle()
        recipe=json.loads((HERE/'bundle/commands.json').read_bytes())['public-perf-smoke']
        require(recipe == model['exact_stage'],'frozen V8 public recipe differs')
        require(not env.get('RUSTFLAGS') and not env.get('CARGO_ENCODED_RUSTFLAGS'),'undeclared compiler flags')
        group=next(line.split(':',2)[2] for line in Path('/proc/self/cgroup').read_text().splitlines() if line.startswith('0::'))
        cg=Path('/sys/fs/cgroup')/group.lstrip('/')
        limits={name:(cg/name).read_text().strip() for name in ('memory.max','memory.swap.max')}
        require(limits == {'memory.max':str(24*1024**3),'memory.swap.max':'0'},'24GiB/no-swap required')
        fleet=Path('/home/gpc/HDB/sprint/coordination/build.lock').open('r+')
        fcntl.flock(fleet,fcntl.LOCK_EX|fcntl.LOCK_NB)
        before={'product':source(product,model['product_entries'],PRODUCT,[],env),
                'controller':source(control,model['controller_entries'],os.environ['GITHUB_SHA'],model['auxiliary_paths'],env)}
        write(out/'source-before.json',before)
        tools=tool_identity(product,env)
        write(out/'host.json',{'tools':tools,'cgroup':group,'limits':limits,'uname':list(os.uname()),
              'meminfo':Path('/proc/meminfo').read_text(),'environment':{key:env.get(key) for key in ENV_KEYS},
              'outer_service_budget_seconds':8400,'original_stage_timeout_seconds':recipe['timeout_seconds']})
        inventory=run_owned([sys.executable,str(HERE/'bundle/inventory.py'),str(product),str(out/'capture'),
                             '--label','public-smoke-'+PRODUCT],product,env,out,'inventory',120)
        require(inventory['exit_code'] == 0 and not inventory['timed_out'],'source capture failed')
        expected=json.loads((out/'capture/expected-manifest.json').read_bytes())
        require(expected['head'] == PRODUCT and len(expected['targets']) == 451,'final source inventory differs')
        require(not Path(env['CARGO_TARGET_DIR']).exists(),'target must be fresh')
        runtime=run_owned([sys.executable,str(HERE/'bundle/run_stage.py'),'public-perf-smoke',
            '--tree',str(product),'--target',env['CARGO_TARGET_DIR'],'--out',str(out/'public-perf-smoke'),
            '--source-manifest',str(out/'capture/source-sha256.json'),
            '--expected-manifest',str(out/'capture/expected-manifest.json'),'--execute'],
            product,env,out,'public-stage',max(1,8100-(time.monotonic()-started)))
        require(runtime['exit_code'] == 0 and not runtime['timed_out'],'public stage failed; original raw evidence retained')
        stage=json.loads((out/'public-perf-smoke/receipt.json').read_bytes())
        judged=judge(stage)
        require(tool_identity(product,env) == tools,'compiler/Cargo changed')
        write(out/'native-proof-summary.json',judged)
        result.update(judged)
        result['commands_green']=True
    except BaseException as error:
        result['errors'].append({'type':type(error).__name__,'message':str(error)})
    finally:
        if model is not None:
            try:
                after={'product':source(product,model['product_entries'],PRODUCT,[],env),
                       'controller':source(control,model['controller_entries'],os.environ['GITHUB_SHA'],model['auxiliary_paths'],env)}
                write(out/'source-after.json',after)
                require(before is not None and before == after,'source before/after differs')
                verify_bundle()
            except BaseException as error:
                result['errors'].append({'phase':'final-source-guard','type':type(error).__name__,'message':str(error)})
                result['commands_green']=False
        result['elapsed_seconds']=time.monotonic()-started
        if result['errors']:result['commands_green']=False
        write(out/'result.json',result)
    return 0 if result['commands_green'] else 1


if __name__ == '__main__':
    raise SystemExit(main())
