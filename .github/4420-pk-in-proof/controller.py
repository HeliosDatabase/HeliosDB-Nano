#!/usr/bin/env python3
"""Matched PK-IN semantics/timing and unchanged full CodeGraph field gate."""
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
MODES = {'contract':'transaction_pk_in_contract','grouped':'code_graph_grouped_in_diagnostic',
         'field':'code_graph_parallel_index_bench'}
NAMES = {'grouped':'grouped_pk_in_transaction_update_scaling','field':'field_benchmark_nano_src_tree'}
ROLES = {'baseline':'5a3f5c5d9608ffc5f71f8b3b3f309abc5d66785b',
         'fixed':'77b385f79e1d94a44fe87f9749533feacf4048da'}
FEATURES = ['code-graph', 'default', 'encryption', 'ha-tier1', 'ring-crypto', 'vector-search']
ENV_KEYS = ('RUSTUP_TOOLCHAIN','CARGO_HOME','RUSTUP_HOME','CARGO_TARGET_DIR','CARGO_MANIFEST_DIR',
            'CARGO_PROFILE_TEST_DEBUG','CARGO_PROFILE_DEV_DEBUG','CARGO_PROFILE_RELEASE_DEBUG',
            'CARGO_BUILD_JOBS','CMAKE_BUILD_PARALLEL_LEVEL','RUST_TEST_THREADS',
            'RUSTFLAGS','CARGO_ENCODED_RUSTFLAGS')


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


def judge_grouped(text, code):
    require(code == 0, 'native diagnostic assertion/error')
    summaries = re.findall(r'test result: (ok|FAILED)\. (\d+) passed; (\d+) failed; (\d+) ignored; (\d+) measured; (\d+) filtered out', text)
    require(summaries == [('ok','1','0','0','0','0')], 'exact complete native parent required')
    cells = re.findall(r'codegraph-grouped-in-cell rows=(\d+) groups=(\d+) touched=(\d+) commit=(false|true) setup_ns=(\d+) updates_ns=(\d+) settle_ns=(\d+) group_ns=(\[[0-9, ]+\])', text)
    operations = re.findall(r'codegraph-grouped-in-operation rows=(\d+) groups=(\d+) commit=(false|true) ordinal=(\d+) selected=(\d+) affected=(\d+) elapsed_ns=(\d+)', text)
    require(text.count('codegraph-grouped-in-cell') == len(cells) == 12, 'all twelve complete row/settlement cells required')
    require(text.count('codegraph-grouped-in-operation') == len(operations) == 324, 'all324 actual operations required')
    expected = {(rows,groups,commit) for rows in (4096,16384) for groups in (1,16,64) for commit in ('false','true')}
    actual = {}
    for row in cells:
        rows,groups,touched,commit,setup,updates,settle,durations = row
        key = (int(rows),int(groups),commit)
        require(key in expected and key not in actual and int(touched) == 256, 'duplicate/unknown cell')
        ns = json.loads(durations)
        require(len(ns) == key[1] and all(type(x) is int and x > 0 for x in ns), 'missing/empty actual group timings')
        require(int(setup) > 0 and int(updates) >= sum(ns) and int(settle) > 0, 'invalid elapsed observations')
        actual[key] = {'rows':key[0],'groups':key[1],'commit':commit,'touched':256,
                       'setup_ns':int(setup),'updates_ns':int(updates),'settle_ns':int(settle),'group_ns':ns}
    require(set(actual) == expected, 'cell identity set differs')
    seen = set()
    for rows,groups,commit,ordinal,selected,affected,elapsed in operations:
        key = (int(rows),int(groups),commit)
        identity = key + (int(ordinal),)
        require(key in actual and identity not in seen and 0 <= int(ordinal) < key[1], 'duplicate/unknown operation')
        require(int(selected) == int(affected) == 256 // key[1], 'changed row workload')
        require(actual[key]['group_ns'][int(ordinal)] == int(elapsed) > 0, 'operation/cell time join differs')
        seen.add(identity)
    require(seen == {key + (ordinal,) for key in expected for ordinal in range(key[1])}, 'partial operation identity set')
    return {'status':'native_diagnostic_complete','native_summary':{'passed':1,'failed':0,'ignored':0},
            'completed_cells':12,'actual_update_statements':324,'updated_rows_per_cell':256,
            'measurements':[actual[key] for key in sorted(actual)],
            'interpretation':'Raw timings only; no ratio threshold or release acceptance. Original443-file/600s field gate remains.',
            'release_acceptance_pass':False}


def summary(text, passed, failed):
    rows = re.findall(r'test result: (ok|FAILED)\. (\d+) passed; (\d+) failed; (\d+) ignored; (\d+) measured; (\d+) filtered out', text)
    require(len(rows) == 1, 'one actual native summary required')
    status, got_passed, got_failed, ignored, measured, filtered = rows[0]
    require((int(got_passed), int(got_failed), int(ignored), int(measured)) == (passed, failed, 0, 0), 'partial/wrong native counts')
    require(status == ('FAILED' if failed else 'ok'), 'native summary status mismatch')
    return {'passed': passed, 'failed': failed, 'ignored': 0, 'filtered': int(filtered)}

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

def contract_cells():
    expected = set()
    for kind in ('staged','branch','session'):
        for history in ('false','true'):
            for family in ('Text','Params'):
                for commit in ('false','true'):
                    expected.add((kind,history,family,commit,''))
    for kind in ('implicit','fallback'):
        for history in ('false','true'):
            for family in ('Text','Params'):
                expected.add((kind,history,family,'',''))
    for history in ('false','true'):
        for family in ('Text','Params'):
            for kind in ('check','unique','foreign'):
                expected.add(('constraint',history,family,'',kind))
        for commit in ('false','true'):
            expected.add(('raii',history,'',commit,''))
    return expected


def judge_contract(text, names, code):
    require(code == 0,'actual PK-IN contract failed')
    actual = summary(text,7,0)
    require(actual['filtered'] == 0,'contract target must be complete/unfiltered')
    observed = re.findall(r'^test (\S+) \.\.\.',text,re.M)
    require(set(observed) == set(names) and len(observed) == len(names) == 7,'all seven exact parents required')
    lines = re.findall(r'^transaction-pk-in-cell ([^\n]+)$',text,re.M)
    require(text.count('transaction-pk-in-cell') == len(lines) == 48,'all48 actual completed cells required')
    cells = set()
    for line in lines:
        tokens = line.split()
        kind = tokens[0]
        pairs = [token.split('=',1) for token in tokens[1:]]
        require(all(len(pair)==2 for pair in pairs),'malformed cell')
        fields = dict(pairs)
        require(len(fields)==len(pairs) and set(fields) <= {'history','family','commit','kind'},'duplicate/unknown cell field')
        cell = (kind,fields.get('history',''),fields.get('family',''),fields.get('commit',''),fields.get('kind',''))
        require(cell not in cells,'duplicate contract cell')
        cells.add(cell)
    require(cells == contract_cells(),'partial/unknown contract identity set')
    return {'status':'seven_parent48_cell_contract_pass','native_summary':actual,'completed_cells':48,
            'scope':'Real text/count and params/RETURNING, independent full row and owner/rollback truth.',
            'release_acceptance_pass':False}


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--role',choices=['baseline','fixed'],required=True)
    parser.add_argument('--mode',choices=sorted(MODES),required=True)
    parser.add_argument('--product',type=Path,required=True)
    parser.add_argument('--controller',type=Path,required=True)
    parser.add_argument('--out',type=Path,required=True)
    parser.add_argument('--model-sha256',required=True)
    args = parser.parse_args()
    require(args.mode != 'field' or args.role == 'fixed','field gate is fixed-only')
    target = MODES[args.mode]
    name = NAMES.get(args.mode)
    head = ROLES[args.role]
    product,control,output = args.product.resolve(),args.controller.resolve(),args.out.resolve()
    require(not output.is_relative_to(product) and not output.is_relative_to(control),'output must be external')
    output.mkdir(parents=True,exist_ok=False)
    result = {'role':args.role,'mode':args.mode,'status':'invalid','proof_valid':False,'errors':[],'release_acceptance_pass':False}
    env = os.environ.copy()
    env.update(RUSTUP_TOOLCHAIN='1.95.0',CARGO_BUILD_JOBS='2',CMAKE_BUILD_PARALLEL_LEVEL='2',
               CARGO_PROFILE_TEST_DEBUG='0',CARGO_PROFILE_DEV_DEBUG='0',CARGO_PROFILE_RELEASE_DEBUG='0',
               RUST_TEST_THREADS='1',CARGO_TERM_COLOR='never',CARGO_MANIFEST_DIR=str(product),CARGO_TARGET_DIR=str(output/'fresh-target'))
    before,model,fleet,tools,config = None,None,None,None,None
    started = time.monotonic()
    try:
        require(digest(HERE/'model.json') == args.model_sha256,'model bootstrap mismatch')
        model = json.loads((HERE/'model.json').read_text())
        require(model['roles'] == ROLES and model['targets'] == MODES,'source/target roles changed')
        require(not env.get('RUSTFLAGS') and not env.get('CARGO_ENCODED_RUSTFLAGS'),'undeclared Rust flags')
        group = next(line.split(':',2)[2] for line in Path('/proc/self/cgroup').read_text().splitlines() if line.startswith('0::'))
        cg = Path('/sys/fs/cgroup')/group.lstrip('/')
        limits = {name:(cg/name).read_text().strip() for name in ('memory.max','memory.swap.max')}
        require(limits == {'memory.max':str(24*1024**3),'memory.swap.max':'0'},'24GiB noSwap service required')
        fleet = Path('/home/gpc/HDB/sprint/coordination/build.lock').open('r+')
        fcntl.flock(fleet,fcntl.LOCK_EX|fcntl.LOCK_NB)
        before = {'product':source(product,model['product_entries'][args.role],head,[],env),
                  'controller':source(control,model['controller_entries'],os.environ['GITHUB_SHA'],model['auxiliary_paths'],env)}
        write(output/'source-before.json',before)
        tools,config = tool_identity(product,env),config_identity(product,env)
        write(output/'host.json',{'tools':tools,'cargo_configs':config,'limits':limits,'cgroup':group,
              'uname':list(os.uname()),'environment':{key:env.get(key) for key in ENV_KEYS},
              'contract_provenance':model['contracts'], 'cpu_count':os.cpu_count(),
              'affinity':sorted(os.sched_getaffinity(0)), 'cpuinfo':Path('/proc/cpuinfo').read_text()})
        require(not Path(env['CARGO_TARGET_DIR']).exists(),'fresh target already exists')
        budget = 3600 if args.mode == 'contract' else 5400
        def remaining(maximum):
            value = min(maximum,budget-(time.monotonic()-started))
            require(value > 0,'shared build/native deadline expired')
            return value
        prefix = ['cargo','test','--locked'] + ([] if args.mode == 'contract' else ['--release','--features','code-graph']) + ['--test',target]
        built = run_owned(prefix+['--no-run','--message-format=json'],product,env,output,'build',remaining(3000 if args.mode == 'contract' else 4800))
        require(built['exit_code'] == 0 and not built['timed_out'],'compile failed/timed out')
        messages = [json.loads(line) for line in (output/'build.stdout').read_text().splitlines()]
        artifacts = [m for m in messages if m.get('reason') == 'compiler-artifact' and m.get('executable')
                     and m.get('profile',{}).get('test') and m.get('target',{}).get('kind') == ['test'] and m['target']['name'] == target]
        require(len(artifacts) == 1,'one compiler-owned integration ELF required')
        artifact = artifacts[0]
        require(sorted(artifact['features']) == (sorted(set(FEATURES)-{'code-graph'}) if args.mode == 'contract' else FEATURES) and artifact['profile']['debuginfo'] == 0
                and artifact['profile']['opt_level'] == ('0' if args.mode == 'contract' else '3') and artifact['fresh'] is False,'feature/profile/fresh identity changed')
        require(Path(artifact['manifest_path']).resolve() == product/'Cargo.toml'
                and Path(artifact['target']['src_path']).resolve() == product/('tests/'+target+'.rs'),'wrong source compiled')
        binary = Path(artifact['executable']).resolve()
        require(binary.is_relative_to(Path(env['CARGO_TARGET_DIR'])) and binary.is_file(),'ELF escapes owned fresh target')
        with binary.open('rb') as stream:
            require(stream.read(6) == b'\x7fELF\x02\x01','native Linux64 ELF required')
        binary_sha = digest(binary)
        shutil.copyfile(binary,output/'native-test.elf')
        require(digest(output/'native-test.elf') == binary_sha,'preserved ELF differs')
        write(output/'native-build-receipt.json',{'compiler_artifact':artifact,'binary':str(binary),
              'binary_sha256':binary_sha,'head':head,'build':built,'tools':tools})
        for stem,extra in [('native-list',[]),('native-ignored-list',['--ignored'])]:
            listing = run_owned([str(binary),'--list','--format','terse']+extra,product,env,output,stem,remaining(60))
            require(listing['exit_code'] == 0 and not listing['timed_out'],'actual native listing failed')
            lines = (output/(stem+'.stdout')).read_text().splitlines()
            wanted = (model['contract_names'] if args.mode == 'contract' else [name]) if not extra else ([] if args.mode == 'contract' else [name])
            require(len(lines) == len(set(lines)) and set(lines) == {value+': test' for value in wanted},'exact actual native parent/ignored list differs')
        if args.mode != 'contract':
            require(os.getloadavg()[0] < 6,'load gate not met; timing run not started')
        native_argv = [str(binary)] + ([] if args.mode == 'contract' else [name,'--exact','--include-ignored']) + ['--nocapture','--test-threads=1']
        if args.mode == 'field':
            require(prefix+[name,'--','--exact','--include-ignored','--nocapture','--test-threads=1'] == model['original_field_command'],'original field command changed')
        runtime = run_owned(native_argv,product,env,output,'native',remaining(300 if args.mode == 'contract' else 600),native_sha=binary_sha)
        require(not runtime['timed_out'] and runtime['leader_reaped'],'native timed out/not reaped')
        require(digest(binary) == binary_sha,'native ELF changed')
        text = (output/'native.stdout').read_text(errors='replace')+'\n'+(output/'native.stderr').read_text(errors='replace')
        judged = judge_contract(text,model['contract_names'],runtime['exit_code']) if args.mode == 'contract' else (judge_grouped(text,runtime['exit_code']) if args.mode == 'grouped' else judge_field(text,runtime['exit_code'],443))
        write(output/'native-adjudication.json',judged)
        result.update(judged)
        result.update(proof_valid=True,native_binary_sha256=binary_sha)
    except BaseException as error:
        result['errors'].append({'type':type(error).__name__,'message':str(error)})
    finally:
        if tools is not None:
            try:
                require(tool_identity(product,env) == tools and config_identity(product,env) == config,'tools/configs changed')
            except BaseException as error:
                result['errors'].append({'phase':'final-tools-config','type':type(error).__name__,'message':str(error)})
        if model is not None:
            try:
                after = {'product':source(product,model['product_entries'][args.role],head,[],env),
                         'controller':source(control,model['controller_entries'],os.environ['GITHUB_SHA'],model['auxiliary_paths'],env)}
                write(output/'source-after.json',after)
                require(before is not None and before == after,'source before/after differs')
            except BaseException as error:
                result['errors'].append({'phase':'final-source','type':type(error).__name__,'message':str(error)})
        result['elapsed_seconds'] = time.monotonic()-started
        if result['errors']:
            result.update(status='invalid_or_native_red',proof_valid=False)
        write(output/'result.json',result)
    return 0 if result['proof_valid'] else 1

if __name__ == '__main__':
    raise SystemExit(main())
